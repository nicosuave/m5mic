mod audio;
mod ble;
mod codec;
mod discovery;
mod display;
mod i2c_bus;
mod power;
mod setup;
mod usb_audio;
mod wifi_config;

use std::{
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender},
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use esp_idf_hal::{
    delay::FreeRtos,
    gpio::{Input, PinDriver, Pull},
    i2s::{I2sDriver, I2sRx},
    ledc::{
        config::{Resolution, TimerConfig},
        LedcDriver, LedcTimerDriver,
    },
    peripherals::Peripherals,
    units::FromValueType,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    log::EspLogger,
    mdns::EspMdns,
    netif::{EspNetif, NetifStack},
    nvs::EspDefaultNvsPartition,
    wifi::{BlockingWifi, EspWifi, WifiDriver},
    ws::client::{EspWebSocketClient, EspWebSocketClientConfig, FrameType, WebSocketEventType},
};
use log::{info, warn};
use m5mic_protocol::{
    ima_adpcm4_encode, ima_adpcm4_encoded_len, parse_control_command, AudioFrameHeader, Codec,
    ControlAction, ControlMode, ImaAdpcmState, CONTROL_PORT, CONTROL_PRIORITY_PHONE,
    FLAG_PUSH_TO_TALK, FLAG_STREAM_END, FLAG_STREAM_START, HEADER_LEN,
};
use usb_audio::TransportMode;
use wifi_config::{AppSettings, BatteryBrightness};

const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u8 = 1;
const FRAME_MS: usize = 40;
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize * FRAME_MS / 1_000;
const PCM_BYTES: usize = FRAME_SAMPLES * 2;
const ADPCM_BYTES: usize = ima_adpcm4_encoded_len(PCM_BYTES);
const FRAME_BYTES: usize = HEADER_LEN + PCM_BYTES;
const AUDIO_BUFFER_FRAMES: usize = 8;
const DRAIN_FRAMES: usize = 8;
const BATTERY_REFRESH_US: u64 = 30_000_000;
const RECORDING_POWER_REFRESH_US: u64 = 1_000_000;
const LEVEL_REFRESH_US: u64 = 200_000;
const SETUP_BOOT_HOLD_MS: u32 = 1_200;
const SETUP_IDLE_HOLD_MS: u32 = 2_000;
const PUSH_TO_TALK_HOLD_MS: u32 = 450;
const CAPTURE_THREAD_STACK: usize = 12_288;
const CONTROL_LEASE_US: u64 = 30_000_000;

#[derive(Debug, Eq, PartialEq)]
enum IdleAction {
    Record { mode: RecordMode, priority: u8 },
    Setup,
    CycleMode,
    SetMode(ModeCommand),
    ProvisionWifi(ble::ProvisionedWifi),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ButtonBAction {
    CycleMode,
    Setup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordMode {
    Latched,
    PushToTalk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModeCommand {
    mode: ActiveMode,
    priority: u8,
}

#[derive(Debug, Eq, PartialEq)]
enum ControlEvent {
    SetMode(ModeCommand),
    RecordStart { priority: u8 },
    RecordStop,
    ProvisionWifi(ble::ProvisionedWifi),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveMode {
    Wifi,
    Bluetooth,
    Usb,
}

impl ActiveMode {
    const fn from_control(mode: ControlMode) -> Self {
        match mode {
            ControlMode::Usb => Self::Usb,
            ControlMode::Wifi => Self::Wifi,
            ControlMode::Bluetooth => Self::Bluetooth,
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Wifi => Self::Bluetooth,
            Self::Bluetooth => Self::Usb,
            Self::Usb => Self::Wifi,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Wifi => "Wi-Fi",
            Self::Bluetooth => "Bluetooth",
            Self::Usb => "USB",
        }
    }
}

#[derive(Default)]
struct ControlLease {
    priority: u8,
    expires_us: u64,
}

impl ControlLease {
    fn accepts(&mut self, command: ModeCommand, now_us: u64) -> bool {
        if now_us >= self.expires_us {
            self.priority = 0;
        }
        if command.priority < self.priority {
            return false;
        }
        self.priority = command.priority;
        self.expires_us = now_us.saturating_add(CONTROL_LEASE_US);
        true
    }
}

impl RecordMode {
    const fn is_push_to_talk(self) -> bool {
        matches!(self, Self::PushToTalk)
    }

    const fn display_mode(self) -> display::RecordModeView {
        match self {
            Self::Latched => display::RecordModeView::Latched,
            Self::PushToTalk => display::RecordModeView::PushToTalk,
        }
    }
}

struct CapturedFrame {
    bytes: [u8; FRAME_BYTES],
    len: usize,
    level: u8,
    sequence: u32,
}

impl CapturedFrame {
    fn new(sequence: u32) -> Self {
        Self {
            bytes: [0; FRAME_BYTES],
            len: HEADER_LEN,
            level: 0,
            sequence,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamStop {
    User,
    CaptureEnded,
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let peripherals = Peripherals::take().context("take peripherals")?;
    let sys_loop = EspSystemEventLoop::take().context("take system event loop")?;
    let nvs = EspDefaultNvsPartition::take().context("take nvs partition")?;
    let wifi_store = wifi_config::WifiStore::new(nvs.clone());
    let app_settings = wifi_store.load_settings().context("load app settings")?;

    let button_a = PinDriver::input(peripherals.pins.gpio11, Pull::Up).context("create BtnA")?;
    let button_b = PinDriver::input(peripherals.pins.gpio12, Pull::Up).context("create BtnB")?;

    let internal_i2c = find_internal_i2c().context("find StickS3 internal I2C")?;
    let mut pm1_i2c = match internal_i2c.probe(0x6e) {
        Ok(()) => {
            let mut pm1_i2c = internal_i2c
                .add_device(0x6e)
                .context("add M5PM1 I2C device")?;
            power::init_sticks3_power(&mut pm1_i2c).context("enable StickS3 power rails")?;
            info!("StickS3 power rails enabled");
            Some(pm1_i2c)
        }
        Err(err) => {
            warn!("M5PM1 not found at 0x6e; continuing without PM1 init: {err}");
            None
        }
    };

    let backlight_timer = LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::new()
            .frequency(25.kHz().into())
            .resolution(Resolution::Bits8),
    )
    .context("create LCD backlight PWM timer")?;
    let backlight = LedcDriver::new(
        peripherals.ledc.channel0,
        backlight_timer,
        peripherals.pins.gpio38,
    )
    .context("create LCD backlight PWM")?;

    let mut display = display::StickDisplay::new(
        peripherals.spi3,
        peripherals.pins.gpio40,
        peripherals.pins.gpio39,
        peripherals.pins.gpio41,
        peripherals.pins.gpio45,
        peripherals.pins.gpio21,
        backlight,
    )
    .context("create display")?;
    set_battery_from_pm1(&mut pm1_i2c, &mut display);
    apply_idle_brightness(&mut display, &app_settings).context("set display brightness")?;

    let wifi_driver =
        WifiDriver::new(peripherals.modem, sys_loop.clone(), Some(nvs)).context("create wifi")?;
    let wifi = EspWifi::wrap_all(
        wifi_driver,
        EspNetif::new(NetifStack::Sta).context("create STA netif")?,
        setup::create_ap_netif().context("create setup AP netif")?,
    )
    .context("attach wifi netifs")?;
    let mut wifi = BlockingWifi::wrap(wifi, sys_loop).context("wrap wifi")?;
    let setup_ssid = setup::ap_ssid(&wifi);

    if button_held(&button_b, SETUP_BOOT_HOLD_MS) {
        info!("BtnB held at boot; entering setup portal");
        let setup_ble = match ble::BleAudioServer::start() {
            Ok(server) => Some(server),
            Err(err) => {
                warn!("Bluetooth setup server failed to start: {err:#}");
                None
            }
        };
        setup::run(
            &mut wifi,
            wifi_store.clone(),
            &mut display,
            &setup_ssid,
            &button_b,
            setup_ble.as_ref(),
        )?;
        return Ok(());
    }

    let i2s_config = audio::mic_i2s_config(SAMPLE_RATE);
    let mut i2s = I2sDriver::<I2sRx>::new_std_rx(
        peripherals.i2s1,
        &i2s_config,
        peripherals.pins.gpio17,
        peripherals.pins.gpio16,
        Some(peripherals.pins.gpio18),
        peripherals.pins.gpio15,
    )
    .context("create I2S mic RX")?;
    i2s.rx_enable().context("enable I2S RX")?;
    info!("I2S mic enabled");

    internal_i2c.probe(0x18).context("probe ES8311")?;
    let mut codec_i2c = internal_i2c
        .add_device(0x18)
        .context("add ES8311 I2C device")?;
    codec::Es8311::new(&mut codec_i2c)
        .enable_adc()
        .context("enable ES8311 ADC")?;
    info!("ES8311 ADC enabled");
    let usb_audio = usb_audio::UsbAudio::new(i2s).context("start USB audio")?;
    usb_audio.set_transport(TransportMode::Wireless);
    drain_i2s(&usb_audio, DRAIN_FRAMES).context("drain startup audio")?;
    let control_socket = create_control_socket().context("start mode control socket")?;

    info!("press BtnA to start recording");
    let mut mdns = None;
    let mut ble_audio = None;
    let mut cached_receiver = None;
    let mut active_mode = ActiveMode::Wifi;
    let mut control_lease = ControlLease::default();
    activate_mode(
        active_mode,
        &mut wifi,
        &wifi_store,
        &mut mdns,
        &mut ble_audio,
        &mut display,
        &mut pm1_i2c,
        &usb_audio,
        &app_settings,
    )?;

    loop {
        let action = match active_mode {
            ActiveMode::Usb => wait_for_usb_action(
                &button_b,
                &mut display,
                &mut pm1_i2c,
                &usb_audio,
                &control_socket,
                ble_audio.as_ref(),
                &mut control_lease,
            )
            .context("wait for USB mode action")?,
            ActiveMode::Wifi | ActiveMode::Bluetooth => wait_for_idle_action(
                &button_a,
                &button_b,
                &mut display,
                &mut pm1_i2c,
                &control_socket,
                ble_audio.as_ref(),
                &mut control_lease,
            )
            .context("wait for idle action")?,
        };

        let mode = match action {
            IdleAction::Record { mode, priority } => {
                if active_mode == ActiveMode::Wifi && priority >= CONTROL_PRIORITY_PHONE {
                    cached_receiver = None;
                }
                mode
            }
            IdleAction::CycleMode => {
                active_mode = active_mode.next();
                activate_mode(
                    active_mode,
                    &mut wifi,
                    &wifi_store,
                    &mut mdns,
                    &mut ble_audio,
                    &mut display,
                    &mut pm1_i2c,
                    &usb_audio,
                    &app_settings,
                )?;
                info!("mode switched to {}", active_mode.label());
                continue;
            }
            IdleAction::SetMode(command) => {
                if command.priority >= CONTROL_PRIORITY_PHONE && command.mode == ActiveMode::Wifi {
                    cached_receiver = None;
                }
                if command.mode == active_mode {
                    info!("mode already {}", active_mode.label());
                    continue;
                }
                active_mode = command.mode;
                activate_mode(
                    active_mode,
                    &mut wifi,
                    &wifi_store,
                    &mut mdns,
                    &mut ble_audio,
                    &mut display,
                    &mut pm1_i2c,
                    &usb_audio,
                    &app_settings,
                )?;
                info!("mode switched to {}", active_mode.label());
                continue;
            }
            IdleAction::ProvisionWifi(credentials) => {
                wifi_store
                    .save(&wifi_config::WifiCredentials {
                        ssid: credentials.ssid,
                        password: credentials.password,
                    })
                    .context("save Bluetooth-provisioned Wi-Fi")?;
                display
                    .show_setup_saved()
                    .context("draw Bluetooth Wi-Fi saved screen")?;
                FreeRtos::delay_ms(900);
                let _ = wifi.disconnect();
                let _ = wifi.stop();
                mdns = None;
                active_mode = ActiveMode::Wifi;
                activate_mode(
                    active_mode,
                    &mut wifi,
                    &wifi_store,
                    &mut mdns,
                    &mut ble_audio,
                    &mut display,
                    &mut pm1_i2c,
                    &usb_audio,
                    &app_settings,
                )?;
                info!("Bluetooth Wi-Fi provisioning saved; switched to Wi-Fi");
                continue;
            }
            IdleAction::Setup => {
                info!("BtnB held while idle; entering setup portal");
                if ble_audio.is_none() {
                    match ble::BleAudioServer::start() {
                        Ok(server) => ble_audio = Some(server),
                        Err(err) => warn!("Bluetooth setup server failed to start: {err:#}"),
                    }
                }
                setup::run(
                    &mut wifi,
                    wifi_store.clone(),
                    &mut display,
                    &setup_ssid,
                    &button_b,
                    ble_audio.as_ref(),
                )?;
                return Ok(());
            }
        };

        if active_mode == ActiveMode::Usb {
            continue;
        }
        info!("recording requested: {mode:?}");

        let record_result = match active_mode {
            ActiveMode::Wifi => ensure_wifi_ready(
                &mut wifi,
                &wifi_store,
                &mut mdns,
                &mut display,
                &app_settings,
                &mut pm1_i2c,
            )
            .and_then(|()| {
                let mdns = mdns
                    .as_ref()
                    .ok_or_else(|| anyhow!("mDNS is not initialized"))?;
                record_once_wifi(
                    mdns,
                    &mut cached_receiver,
                    &usb_audio,
                    &button_a,
                    &button_b,
                    &mut display,
                    &mut pm1_i2c,
                    &app_settings,
                    mode,
                    &control_socket,
                    ble_audio.as_ref(),
                    &mut control_lease,
                )
            }),
            ActiveMode::Bluetooth => ensure_ble_audio(&mut ble_audio).and_then(|ble_audio| {
                record_once_ble(
                    ble_audio,
                    &usb_audio,
                    &button_a,
                    &button_b,
                    &mut display,
                    &mut pm1_i2c,
                    &app_settings,
                    mode,
                    &control_socket,
                    &mut control_lease,
                )
            }),
            ActiveMode::Usb => Ok(()),
        };
        display
            .set_brightness(display::Brightness::Full)
            .context("set display brightness")?;

        match record_result {
            Ok(()) => {
                info!("recording stopped");
                activate_mode(
                    active_mode,
                    &mut wifi,
                    &wifi_store,
                    &mut mdns,
                    &mut ble_audio,
                    &mut display,
                    &mut pm1_i2c,
                    &usb_audio,
                    &app_settings,
                )?;
            }
            Err(err) => {
                warn!("recording failed: {err:#}");
                apply_idle_brightness(&mut display, &app_settings)
                    .context("set display brightness")?;
                display
                    .show_error("STREAM", "CHECK SERVER")
                    .context("draw stream error")?;
                FreeRtos::delay_ms(750);
                activate_mode(
                    active_mode,
                    &mut wifi,
                    &wifi_store,
                    &mut mdns,
                    &mut ble_audio,
                    &mut display,
                    &mut pm1_i2c,
                    &usb_audio,
                    &app_settings,
                )?;
            }
        }
        info!("press BtnA to start recording");
    }
}

fn find_internal_i2c() -> Result<i2c_bus::I2cBus> {
    for (label, sda, scl) in [
        (
            "GPIO47/GPIO48",
            esp_idf_sys::gpio_num_t_GPIO_NUM_47,
            esp_idf_sys::gpio_num_t_GPIO_NUM_48,
        ),
        (
            "GPIO48/GPIO47",
            esp_idf_sys::gpio_num_t_GPIO_NUM_48,
            esp_idf_sys::gpio_num_t_GPIO_NUM_47,
        ),
    ] {
        let bus = i2c_bus::I2cBus::new(sda, scl)
            .with_context(|| format!("create StickS3 internal I2C on {label}"))?;
        let count = log_i2c_devices(label, &bus);
        if count > 0 {
            return Ok(bus);
        }
        warn!("no internal I2C devices found on {label}");
    }

    Err(anyhow!(
        "no internal I2C devices found on GPIO47/GPIO48 or GPIO48/GPIO47"
    ))
}

fn log_i2c_devices(label: &str, bus: &i2c_bus::I2cBus) -> usize {
    let mut count = 0;
    for address in 0x08..=0x77 {
        if bus.probe(address).is_ok() {
            info!("internal I2C {label}: device at 0x{address:02x}");
            count += 1;
        }
    }
    count
}

fn create_control_socket() -> Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, CONTROL_PORT))
        .context("bind mode control socket")?;
    socket
        .set_nonblocking(true)
        .context("set mode control nonblocking")?;
    Ok(socket)
}

#[derive(Default)]
struct PolledControl {
    mode: Option<ModeCommand>,
    record_start_priority: Option<u8>,
    record_stop: bool,
}

fn poll_transport_control(socket: &UdpSocket) -> Result<PolledControl> {
    let mut buf = [0u8; 128];
    let mut requested = PolledControl::default();

    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, addr)) => {
                let payload = &buf[..len];
                if let Some(command) = parse_control_command(payload) {
                    merge_control_command(&mut requested, command.action, command.priority);
                    info!(
                        "control command from {addr}: {:?} priority {}",
                        command.action, command.priority
                    );
                } else {
                    warn!("unknown control command from {addr}");
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(requested),
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err).context("receive control command"),
        }
    }
}

fn poll_control(
    socket: &UdpSocket,
    ble_audio: Option<&ble::BleAudioServer>,
) -> Result<PolledControl> {
    let mut requested = poll_transport_control(socket)?;

    let Some(ble_audio) = ble_audio else {
        return Ok(requested);
    };
    if let Some(command) = ble_audio.take_control_command() {
        merge_control_command(&mut requested, command.action, command.priority);
    }
    Ok(requested)
}

fn merge_control_command(requested: &mut PolledControl, action: ControlAction, priority: u8) {
    match action {
        ControlAction::SetMode(mode) => {
            let command = ModeCommand {
                mode: ActiveMode::from_control(mode),
                priority,
            };
            requested.mode = best_mode_command(requested.mode, command);
        }
        ControlAction::RecordStart => {
            requested.record_start_priority =
                Some(requested.record_start_priority.unwrap_or(0).max(priority));
        }
        ControlAction::RecordStop => requested.record_stop = true,
    }
}

fn best_mode_command(current: Option<ModeCommand>, next: ModeCommand) -> Option<ModeCommand> {
    if current
        .map(|current| next.priority >= current.priority)
        .unwrap_or(true)
    {
        Some(next)
    } else {
        current
    }
}

fn poll_accepted_control_event(
    socket: &UdpSocket,
    ble_audio: Option<&ble::BleAudioServer>,
    control_lease: &mut ControlLease,
) -> Result<Option<ControlEvent>> {
    if let Some(ble_audio) = ble_audio {
        if let Some(credentials) = ble_audio.take_provisioned_wifi() {
            return Ok(Some(ControlEvent::ProvisionWifi(credentials)));
        }
    }

    let requested = poll_control(socket, ble_audio)?;
    if requested.record_stop {
        return Ok(Some(ControlEvent::RecordStop));
    }

    if let Some(command) = requested.mode {
        let now_us = esp_timer_us();
        if control_lease.accepts(command, now_us) {
            return Ok(Some(ControlEvent::SetMode(command)));
        }
        info!(
            "ignored lower-priority mode command: {} priority {}",
            command.mode.label(),
            command.priority
        );
    }

    if let Some(priority) = requested.record_start_priority {
        return Ok(Some(ControlEvent::RecordStart { priority }));
    }

    Ok(None)
}

fn connect_wifi(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    store: &wifi_config::WifiStore,
) -> Result<()> {
    let credentials = match store.load().context("load saved wifi config")? {
        Some(credentials) => {
            info!("using saved wifi credentials");
            credentials
        }
        None => {
            info!("using build-time wifi credentials");
            wifi_config::fallback_credentials().context("load build-time wifi credentials")?
        }
    };

    let auth_method = if credentials.password.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };

    let wifi_configuration = Configuration::Client(ClientConfiguration {
        ssid: credentials
            .ssid
            .as_str()
            .try_into()
            .map_err(|_| anyhow!("wifi ssid too long"))?,
        password: credentials
            .password
            .as_str()
            .try_into()
            .map_err(|_| anyhow!("wifi password too long"))?,
        auth_method,
        ..Default::default()
    });

    wifi.set_configuration(&wifi_configuration)
        .context("set wifi config")?;
    wifi.start().context("start wifi")?;
    wifi.connect().context("connect wifi")?;
    wifi.wait_netif_up().context("wait for wifi netif")?;
    info!("wifi connected");
    Ok(())
}

fn ensure_wifi_ready(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    store: &wifi_config::WifiStore,
    mdns: &mut Option<EspMdns>,
    display: &mut display::StickDisplay<'_>,
    settings: &AppSettings,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
) -> Result<()> {
    if mdns.is_some() {
        return Ok(());
    }

    display
        .show_wifi_connecting()
        .context("draw wifi connecting")?;
    connect_wifi(wifi, store).context("connect Wi-Fi")?;
    *mdns = Some(start_mdns().context("start mDNS")?);
    refresh_battery(pm1_i2c, display).context("draw battery")?;
    apply_idle_brightness(display, settings).context("set display brightness")?;
    display.show_ready().context("draw Wi-Fi ready screen")
}

fn start_mdns() -> Result<EspMdns> {
    let mut mdns = EspMdns::take().context("start mdns")?;
    mdns.set_hostname("m5sticks3-mic")
        .context("set mdns hostname")?;
    mdns.set_instance_name("M5StickS3 Mic")
        .context("set mdns instance")?;
    Ok(mdns)
}

fn ensure_ble_audio(ble_audio: &mut Option<ble::BleAudioServer>) -> Result<&ble::BleAudioServer> {
    if ble_audio.is_none() {
        *ble_audio = Some(ble::BleAudioServer::start().context("start Bluetooth audio server")?);
    }
    Ok(ble_audio
        .as_ref()
        .expect("Bluetooth audio server initialized"))
}

#[allow(clippy::too_many_arguments)]
fn activate_mode(
    mode: ActiveMode,
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    store: &wifi_config::WifiStore,
    mdns: &mut Option<EspMdns>,
    ble_audio: &mut Option<ble::BleAudioServer>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    usb_audio: &usb_audio::UsbAudio,
    settings: &AppSettings,
) -> Result<()> {
    match mode {
        ActiveMode::Wifi => {
            usb_audio.set_transport(TransportMode::Wireless);
            if let Err(err) = ensure_wifi_ready(wifi, store, mdns, display, settings, pm1_i2c) {
                warn!("wifi connection failed: {err:#}");
                apply_idle_brightness(display, settings).context("set display brightness")?;
                display
                    .show_error("WIFI FAIL", "HOLD B SETUP")
                    .context("draw wifi setup hint")?;
                return Ok(());
            }
            refresh_battery(pm1_i2c, display).context("draw battery")?;
            apply_idle_brightness(display, settings).context("set display brightness")?;
            display.show_ready().context("draw Wi-Fi ready screen")
        }
        ActiveMode::Bluetooth => {
            usb_audio.set_transport(TransportMode::Wireless);
            ensure_ble_audio(ble_audio).context("start Bluetooth audio server")?;
            refresh_battery(pm1_i2c, display).context("draw battery")?;
            apply_idle_brightness(display, settings).context("set display brightness")?;
            display
                .show_bluetooth_ready()
                .context("draw Bluetooth ready screen")
        }
        ActiveMode::Usb => {
            usb_audio.set_transport(TransportMode::Usb);
            refresh_battery(pm1_i2c, display).context("draw battery")?;
            apply_idle_brightness(display, settings).context("set display brightness")?;
            display.show_usb_ready().context("draw USB mic screen")
        }
    }
}

fn record_once_wifi(
    mdns: &EspMdns,
    cached_receiver: &mut Option<String>,
    audio: &usb_audio::UsbAudio,
    button_a: &PinDriver<Input>,
    button_b: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    settings: &AppSettings,
    mode: RecordMode,
    control_socket: &UdpSocket,
    ble_audio: Option<&ble::BleAudioServer>,
    control_lease: &mut ControlLease,
) -> Result<()> {
    if let Some(server_url) = cached_receiver.clone() {
        info!("using cached receiver: {server_url}");
        match connect_audio(&server_url) {
            Ok(client) => {
                info!("recording started; press BtnA to stop");
                let result = stream_audio_connected(
                    client,
                    audio,
                    button_a,
                    button_b,
                    display,
                    pm1_i2c,
                    settings,
                    mode,
                    control_socket,
                    ble_audio,
                    control_lease,
                );
                if result.is_err() {
                    *cached_receiver = None;
                }
                return result;
            }
            Err(err) => {
                warn!("cached receiver failed: {err:#}");
                *cached_receiver = None;
            }
        }
    }

    display
        .show_finding_receiver(display::TransportView::Wifi)
        .context("draw finding receiver")?;
    let server_url = discovery::discover_server(mdns, |phase| {
        if let Err(err) = display.show_finding_receiver_phase(display::TransportView::Wifi, phase) {
            warn!("failed to animate receiver discovery: {err:#}");
        }
    })
    .context("discover receiver")?;
    info!("receiver: {server_url}");
    let client = connect_audio(&server_url).context("connect discovered receiver")?;
    *cached_receiver = Some(server_url);

    info!("recording started; press BtnA to stop");
    let result = stream_audio_connected(
        client,
        audio,
        button_a,
        button_b,
        display,
        pm1_i2c,
        settings,
        mode,
        control_socket,
        ble_audio,
        control_lease,
    );
    if result.is_err() {
        *cached_receiver = None;
    }
    result
}

fn record_once_ble(
    ble_audio: &ble::BleAudioServer,
    audio: &usb_audio::UsbAudio,
    button_a: &PinDriver<Input>,
    button_b: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    settings: &AppSettings,
    mode: RecordMode,
    control_socket: &UdpSocket,
    control_lease: &mut ControlLease,
) -> Result<()> {
    display
        .show_finding_receiver(display::TransportView::Bluetooth)
        .context("draw finding Bluetooth receiver")?;
    for phase in 0..120 {
        if ble_audio.is_ready() {
            info!("Bluetooth receiver connected");
            ble_audio.set_status(b"connected");
            return stream_audio_ble(
                ble_audio,
                audio,
                button_a,
                button_b,
                display,
                pm1_i2c,
                settings,
                mode,
                control_socket,
                control_lease,
            );
        }
        if should_stop_stream(
            mode,
            button_a,
            control_socket,
            Some(ble_audio),
            control_lease,
        )? {
            return Ok(());
        }
        if let Err(err) =
            display.show_finding_receiver_phase(display::TransportView::Bluetooth, phase)
        {
            warn!("failed to animate Bluetooth receiver discovery: {err:#}");
        }
        FreeRtos::delay_ms(100);
    }

    Err(anyhow!("Bluetooth receiver is not connected"))
}

fn connect_audio(server_url: &str) -> Result<EspWebSocketClient<'static>> {
    let config = EspWebSocketClientConfig {
        buffer_size: FRAME_BYTES + 128,
        disable_auto_reconnect: true,
        keep_alive_idle: Some(Duration::from_secs(10)),
        keep_alive_interval: Some(Duration::from_secs(5)),
        keep_alive_count: Some(3),
        reconnect_timeout_ms: Duration::from_secs(2),
        network_timeout_ms: Duration::from_secs(5),
        ..Default::default()
    };

    let client = EspWebSocketClient::new(server_url, &config, Duration::from_secs(10), |event| {
        if let Ok(event) = event {
            match event.event_type {
                WebSocketEventType::Connected => info!("websocket connected"),
                WebSocketEventType::Disconnected => warn!("websocket disconnected"),
                WebSocketEventType::Closed => warn!("websocket closed"),
                WebSocketEventType::Close(reason) => warn!("websocket close: {reason:?}"),
                WebSocketEventType::Text(text) => info!("websocket text: {text}"),
                WebSocketEventType::Binary(_) => {}
                WebSocketEventType::Ping | WebSocketEventType::Pong => {}
                WebSocketEventType::BeforeConnect => {}
            }
        }
    })
    .context("connect websocket")?;

    for _ in 0..50 {
        if client.is_connected() {
            break;
        }
        FreeRtos::delay_ms(100);
    }
    if !client.is_connected() {
        return Err(anyhow!("websocket did not connect"));
    }

    Ok(client)
}

fn stream_audio_connected(
    mut client: EspWebSocketClient<'static>,
    audio: &usb_audio::UsbAudio,
    button_a: &PinDriver<Input>,
    button_b: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    settings: &AppSettings,
    mode: RecordMode,
    control_socket: &UdpSocket,
    ble_audio: Option<&ble::BleAudioServer>,
    control_lease: &mut ControlLease,
) -> Result<()> {
    drain_i2s(audio, DRAIN_FRAMES).context("drain pre-stream audio")?;
    refresh_battery(pm1_i2c, display).context("draw battery")?;
    let power_save_recording = !display.external_power() && settings.recording_battery_saver;
    let live_meters = !power_save_recording;
    let track_level = AtomicBool::new(live_meters);
    let brightness = display_brightness_for_power(display, settings);
    display
        .set_brightness(brightness)
        .context("set recording brightness")?;
    display
        .show_recording(
            display::TransportView::Wifi,
            0,
            mode.display_mode(),
            live_meters,
        )
        .context("draw recording screen")?;
    if live_meters {
        display
            .update_buffer(0, AUDIO_BUFFER_FRAMES)
            .context("draw buffer meter")?;
    }

    let stream_id = next_stream_id();
    let wireless_codec = settings.wireless_codec.protocol_codec();
    info!("wireless audio codec: {}", wireless_codec.as_str());
    let (tx, rx) = sync_channel::<CapturedFrame>(AUDIO_BUFFER_FRAMES);
    let stop_capture = AtomicBool::new(false);
    let queued_frames = AtomicUsize::new(0);
    let mut last_sequence = 0u32;
    let mut have_sent_audio = false;

    let stop_reason = thread::scope(|scope| -> Result<StreamStop> {
        let capture_handle = thread::Builder::new()
            .name("m5mic-capture".to_string())
            .stack_size(CAPTURE_THREAD_STACK)
            .spawn_scoped(scope, || {
                capture_audio_frames(
                    audio,
                    tx,
                    &stop_capture,
                    &queued_frames,
                    stream_id,
                    mode,
                    wireless_codec,
                    &track_level,
                )
            })
            .map_err(|err| anyhow!("spawn audio capture thread: {err}"))?;

        let send_result = send_captured_audio(
            &mut client,
            rx,
            &queued_frames,
            button_a,
            button_b,
            display,
            pm1_i2c,
            mode,
            &track_level,
            live_meters,
            power_save_recording,
            settings,
            &mut last_sequence,
            &mut have_sent_audio,
            control_socket,
            ble_audio,
            control_lease,
        );

        stop_capture.store(true, Ordering::Relaxed);
        let capture_result = capture_handle
            .join()
            .map_err(|_| anyhow!("audio capture thread panicked"))?;

        match (send_result, capture_result) {
            (Ok(StreamStop::User), Err(err)) => {
                warn!("audio capture stopped after user stop: {err:#}");
                Ok(StreamStop::User)
            }
            (Ok(stop), Ok(())) => Ok(stop),
            (Ok(StreamStop::CaptureEnded), Err(err)) => Err(err.context("audio capture")),
            (Err(err), Ok(())) => Err(err),
            (Err(err), Err(capture_err)) => {
                warn!("audio capture also failed: {capture_err:#}");
                Err(err)
            }
        }
    })?;

    if have_sent_audio && client.is_connected() {
        if let Err(err) = send_stream_end(
            &mut client,
            stream_id,
            last_sequence.wrapping_add(1),
            mode,
            wireless_codec,
        ) {
            warn!("failed to send stream end: {err:#}");
        }
    }

    match stop_reason {
        StreamStop::User => Ok(()),
        StreamStop::CaptureEnded => Err(anyhow!("audio capture ended")),
    }
}

fn stream_audio_ble(
    ble_audio: &ble::BleAudioServer,
    audio: &usb_audio::UsbAudio,
    button_a: &PinDriver<Input>,
    button_b: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    settings: &AppSettings,
    mode: RecordMode,
    control_socket: &UdpSocket,
    control_lease: &mut ControlLease,
) -> Result<()> {
    drain_i2s(audio, DRAIN_FRAMES).context("drain pre-stream audio")?;
    refresh_battery(pm1_i2c, display).context("draw battery")?;
    let power_save_recording = !display.external_power() && settings.recording_battery_saver;
    let live_meters = !power_save_recording;
    let track_level = AtomicBool::new(live_meters);
    let brightness = display_brightness_for_power(display, settings);
    display
        .set_brightness(brightness)
        .context("set recording brightness")?;
    display
        .show_recording(
            display::TransportView::Bluetooth,
            0,
            mode.display_mode(),
            live_meters,
        )
        .context("draw recording screen")?;
    if live_meters {
        display
            .update_buffer(0, AUDIO_BUFFER_FRAMES)
            .context("draw buffer meter")?;
    }

    let stream_id = next_stream_id();
    let wireless_codec = Codec::ImaAdpcm4;
    ble_audio.set_status(b"recording");
    info!("Bluetooth audio codec: {}", wireless_codec.as_str());

    let (tx, rx) = sync_channel::<CapturedFrame>(AUDIO_BUFFER_FRAMES);
    let stop_capture = AtomicBool::new(false);
    let queued_frames = AtomicUsize::new(0);
    let mut last_sequence = 0u32;
    let mut have_sent_audio = false;

    let stop_reason = thread::scope(|scope| -> Result<StreamStop> {
        let capture_handle = thread::Builder::new()
            .name("m5mic-capture".to_string())
            .stack_size(CAPTURE_THREAD_STACK)
            .spawn_scoped(scope, || {
                capture_audio_frames(
                    audio,
                    tx,
                    &stop_capture,
                    &queued_frames,
                    stream_id,
                    mode,
                    wireless_codec,
                    &track_level,
                )
            })
            .map_err(|err| anyhow!("spawn audio capture thread: {err}"))?;

        let send_result = send_captured_audio_ble(
            ble_audio,
            rx,
            &queued_frames,
            button_a,
            button_b,
            display,
            pm1_i2c,
            mode,
            &track_level,
            live_meters,
            power_save_recording,
            settings,
            &mut last_sequence,
            &mut have_sent_audio,
            control_socket,
            Some(ble_audio),
            control_lease,
        );

        stop_capture.store(true, Ordering::Relaxed);
        let capture_result = capture_handle
            .join()
            .map_err(|_| anyhow!("audio capture thread panicked"))?;

        match (send_result, capture_result) {
            (Ok(StreamStop::User), Err(err)) => {
                warn!("audio capture stopped after user stop: {err:#}");
                Ok(StreamStop::User)
            }
            (Ok(stop), Ok(())) => Ok(stop),
            (Ok(StreamStop::CaptureEnded), Err(err)) => Err(err.context("audio capture")),
            (Err(err), Ok(())) => Err(err),
            (Err(err), Err(capture_err)) => {
                warn!("audio capture also failed: {capture_err:#}");
                Err(err)
            }
        }
    })?;

    if have_sent_audio && ble_audio.is_ready() {
        send_stream_end_ble(
            ble_audio,
            stream_id,
            last_sequence.wrapping_add(1),
            mode,
            wireless_codec,
        )
        .context("send Bluetooth stream end")?;
    }
    ble_audio.set_status(b"idle");

    match stop_reason {
        StreamStop::User => Ok(()),
        StreamStop::CaptureEnded => Err(anyhow!("audio capture ended")),
    }
}

fn capture_audio_frames(
    audio: &usb_audio::UsbAudio,
    tx: SyncSender<CapturedFrame>,
    stop_capture: &AtomicBool,
    queued_frames: &AtomicUsize,
    stream_id: u32,
    mode: RecordMode,
    codec: Codec,
    track_level: &AtomicBool,
) -> Result<()> {
    let mut sequence = 0u32;
    let mut first = true;
    let mut pcm = [0u8; PCM_BYTES];
    let mut adpcm_state = ImaAdpcmState::new();

    while !stop_capture.load(Ordering::Relaxed) {
        let mut frame = CapturedFrame::new(sequence);
        audio.read_exact(&mut pcm)?;
        if track_level.load(Ordering::Relaxed) {
            frame.level = pcm_peak_percent(&pcm);
        }

        let mut flags = mode_flags(mode);
        if first {
            flags |= FLAG_STREAM_START;
        }
        let payload_len = match codec {
            Codec::PcmS16Le => {
                frame.bytes[HEADER_LEN..HEADER_LEN + PCM_BYTES].copy_from_slice(&pcm);
                PCM_BYTES
            }
            Codec::ImaAdpcm4 => ima_adpcm4_encode(
                &pcm,
                &mut frame.bytes[HEADER_LEN..HEADER_LEN + ADPCM_BYTES],
                &mut adpcm_state,
            )
            .map_err(|err| anyhow!("encode adpcm frame: {err:?}"))?,
        };
        frame.len = HEADER_LEN + payload_len;

        let header = AudioFrameHeader::new(
            codec,
            CHANNELS,
            SAMPLE_RATE,
            sequence,
            esp_timer_us(),
            payload_len as u16,
            stream_id,
            flags,
        );
        header
            .encode_into(&mut frame.bytes[..HEADER_LEN])
            .map_err(|err| anyhow!("encode audio frame: {err:?}"))?;

        queued_frames.fetch_add(1, Ordering::Relaxed);
        if tx.send(frame).is_err() {
            decrement_queued_frames(queued_frames);
            break;
        }

        sequence = sequence.wrapping_add(1);
        first = false;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_captured_audio(
    client: &mut EspWebSocketClient<'static>,
    rx: Receiver<CapturedFrame>,
    queued_frames: &AtomicUsize,
    button_a: &PinDriver<Input>,
    button_b: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    mode: RecordMode,
    track_level: &AtomicBool,
    mut live_meters: bool,
    mut power_save_recording: bool,
    settings: &AppSettings,
    last_sequence: &mut u32,
    have_sent_audio: &mut bool,
    control_socket: &UdpSocket,
    ble_audio: Option<&ble::BleAudioServer>,
    control_lease: &mut ControlLease,
) -> Result<StreamStop> {
    let started_us = esp_timer_us();
    let mut last_elapsed_secs = 0;
    let mut next_battery_refresh_us = started_us.saturating_add(RECORDING_POWER_REFRESH_US);
    let mut next_level_refresh_us = started_us.saturating_add(LEVEL_REFRESH_US);
    let mut meter_level = 0u8;
    let mut last_buffer_frames = usize::MAX;
    let mut display_off = false;

    loop {
        if should_stop_stream(mode, button_a, control_socket, ble_audio, control_lease)? {
            return Ok(StreamStop::User);
        }
        if power_save_recording && consume_button_press(button_b) {
            display_off = !display_off;
            if display_off {
                display
                    .set_brightness(display::Brightness::Off)
                    .context("turn recording display off")?;
            } else {
                let elapsed_secs = esp_timer_us().saturating_sub(started_us) / 1_000_000;
                display
                    .set_brightness(display_brightness_for_power(display, settings))
                    .context("turn recording display on")?;
                display
                    .show_recording(
                        display::TransportView::Wifi,
                        elapsed_secs,
                        mode.display_mode(),
                        false,
                    )
                    .context("redraw recording screen")?;
                last_elapsed_secs = elapsed_secs;
            }
        }

        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(frame) => {
                decrement_queued_frames(queued_frames);
                if !client.is_connected() {
                    return Err(anyhow!("websocket disconnected"));
                }
                client
                    .send(FrameType::Binary(false), &frame.bytes[..frame.len])
                    .context("send audio frame")?;
                *last_sequence = frame.sequence;
                *have_sent_audio = true;

                meter_level = if frame.level > meter_level {
                    frame.level
                } else {
                    meter_level.saturating_sub(8).max(frame.level)
                };
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(StreamStop::CaptureEnded),
        }

        let now_us = esp_timer_us();
        let elapsed_secs = now_us.saturating_sub(started_us) / 1_000_000;
        if !display_off && elapsed_secs != last_elapsed_secs {
            display
                .update_recording_time(elapsed_secs)
                .context("draw recording time")?;
            last_elapsed_secs = elapsed_secs;
        }
        if now_us >= next_battery_refresh_us {
            if display_off {
                display.set_battery(read_battery_view(pm1_i2c));
            } else {
                refresh_battery(pm1_i2c, display).context("draw battery")?;
            }
            let external_power = display.external_power();
            if external_power {
                display_off = false;
            }
            let next_power_save_recording = !external_power && settings.recording_battery_saver;
            let next_live_meters = !next_power_save_recording;
            display
                .set_brightness(if display_off {
                    display::Brightness::Off
                } else {
                    display_brightness_for_power(display, settings)
                })
                .context("set recording brightness")?;

            if next_power_save_recording != power_save_recording || next_live_meters != live_meters
            {
                power_save_recording = next_power_save_recording;
                live_meters = next_live_meters;
                track_level.store(live_meters, Ordering::Relaxed);
                if !live_meters {
                    meter_level = 0;
                }
                if !display_off {
                    display
                        .show_recording(
                            display::TransportView::Wifi,
                            elapsed_secs,
                            mode.display_mode(),
                            live_meters,
                        )
                        .context("redraw recording screen")?;
                    last_elapsed_secs = elapsed_secs;
                }
                last_buffer_frames = usize::MAX;
            }

            next_battery_refresh_us = now_us.saturating_add(RECORDING_POWER_REFRESH_US);
        }
        if live_meters && !display_off && now_us >= next_level_refresh_us {
            display
                .update_level(meter_level)
                .context("draw recording level")?;
            next_level_refresh_us = now_us.saturating_add(LEVEL_REFRESH_US);
        }

        if live_meters && !display_off {
            let buffered = queued_frames
                .load(Ordering::Relaxed)
                .min(AUDIO_BUFFER_FRAMES);
            if buffered != last_buffer_frames {
                display
                    .update_buffer(buffered, AUDIO_BUFFER_FRAMES)
                    .context("draw buffer meter")?;
                last_buffer_frames = buffered;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn send_captured_audio_ble(
    ble_audio: &ble::BleAudioServer,
    rx: Receiver<CapturedFrame>,
    queued_frames: &AtomicUsize,
    button_a: &PinDriver<Input>,
    button_b: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    mode: RecordMode,
    track_level: &AtomicBool,
    mut live_meters: bool,
    mut power_save_recording: bool,
    settings: &AppSettings,
    last_sequence: &mut u32,
    have_sent_audio: &mut bool,
    control_socket: &UdpSocket,
    control_ble_audio: Option<&ble::BleAudioServer>,
    control_lease: &mut ControlLease,
) -> Result<StreamStop> {
    let started_us = esp_timer_us();
    let mut last_elapsed_secs = 0;
    let mut next_battery_refresh_us = started_us.saturating_add(RECORDING_POWER_REFRESH_US);
    let mut next_level_refresh_us = started_us.saturating_add(LEVEL_REFRESH_US);
    let mut meter_level = 0u8;
    let mut last_buffer_frames = usize::MAX;
    let mut display_off = false;

    loop {
        if should_stop_stream(
            mode,
            button_a,
            control_socket,
            control_ble_audio,
            control_lease,
        )? {
            return Ok(StreamStop::User);
        }
        if power_save_recording && consume_button_press(button_b) {
            display_off = !display_off;
            if display_off {
                display
                    .set_brightness(display::Brightness::Off)
                    .context("turn recording display off")?;
            } else {
                let elapsed_secs = esp_timer_us().saturating_sub(started_us) / 1_000_000;
                display
                    .set_brightness(display_brightness_for_power(display, settings))
                    .context("turn recording display on")?;
                display
                    .show_recording(
                        display::TransportView::Bluetooth,
                        elapsed_secs,
                        mode.display_mode(),
                        false,
                    )
                    .context("redraw recording screen")?;
                last_elapsed_secs = elapsed_secs;
            }
        }

        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(frame) => {
                decrement_queued_frames(queued_frames);
                if !ble_audio.is_ready() {
                    return Err(anyhow!("Bluetooth receiver disconnected"));
                }
                ble_audio
                    .notify_frame(frame.sequence, &frame.bytes[..frame.len])
                    .context("send Bluetooth audio frame")?;
                *last_sequence = frame.sequence;
                *have_sent_audio = true;

                meter_level = if frame.level > meter_level {
                    frame.level
                } else {
                    meter_level.saturating_sub(8).max(frame.level)
                };
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(StreamStop::CaptureEnded),
        }

        let now_us = esp_timer_us();
        let elapsed_secs = now_us.saturating_sub(started_us) / 1_000_000;
        if !display_off && elapsed_secs != last_elapsed_secs {
            display
                .update_recording_time(elapsed_secs)
                .context("draw recording time")?;
            last_elapsed_secs = elapsed_secs;
        }
        if now_us >= next_battery_refresh_us {
            if display_off {
                display.set_battery(read_battery_view(pm1_i2c));
            } else {
                refresh_battery(pm1_i2c, display).context("draw battery")?;
            }
            let external_power = display.external_power();
            if external_power {
                display_off = false;
            }
            let next_power_save_recording = !external_power && settings.recording_battery_saver;
            let next_live_meters = !next_power_save_recording;
            display
                .set_brightness(if display_off {
                    display::Brightness::Off
                } else {
                    display_brightness_for_power(display, settings)
                })
                .context("set recording brightness")?;

            if next_power_save_recording != power_save_recording || next_live_meters != live_meters
            {
                power_save_recording = next_power_save_recording;
                live_meters = next_live_meters;
                track_level.store(live_meters, Ordering::Relaxed);
                if !live_meters {
                    meter_level = 0;
                }
                if !display_off {
                    display
                        .show_recording(
                            display::TransportView::Bluetooth,
                            elapsed_secs,
                            mode.display_mode(),
                            live_meters,
                        )
                        .context("redraw recording screen")?;
                    last_elapsed_secs = elapsed_secs;
                }
                last_buffer_frames = usize::MAX;
            }

            next_battery_refresh_us = now_us.saturating_add(RECORDING_POWER_REFRESH_US);
        }
        if live_meters && !display_off && now_us >= next_level_refresh_us {
            display
                .update_level(meter_level)
                .context("draw recording level")?;
            next_level_refresh_us = now_us.saturating_add(LEVEL_REFRESH_US);
        }

        if live_meters && !display_off {
            let buffered = queued_frames
                .load(Ordering::Relaxed)
                .min(AUDIO_BUFFER_FRAMES);
            if buffered != last_buffer_frames {
                display
                    .update_buffer(buffered, AUDIO_BUFFER_FRAMES)
                    .context("draw buffer meter")?;
                last_buffer_frames = buffered;
            }
        }
    }
}

fn should_stop_stream(
    mode: RecordMode,
    button_a: &PinDriver<Input>,
    control_socket: &UdpSocket,
    ble_audio: Option<&ble::BleAudioServer>,
    control_lease: &mut ControlLease,
) -> Result<bool> {
    if match mode {
        RecordMode::Latched => consume_button_press(button_a),
        RecordMode::PushToTalk => !button_a.is_low(),
    } {
        return Ok(true);
    }

    let Some(event) = poll_accepted_control_event(control_socket, ble_audio, control_lease)? else {
        return Ok(false);
    };

    match event {
        ControlEvent::RecordStop => Ok(true),
        ControlEvent::RecordStart { .. } => Ok(false),
        ControlEvent::SetMode(command) => {
            info!(
                "mode command deferred while recording: {} priority {}",
                command.mode.label(),
                command.priority
            );
            Ok(false)
        }
        ControlEvent::ProvisionWifi(_) => {
            info!("Wi-Fi provisioning deferred while recording");
            Ok(false)
        }
    }
}

fn send_stream_end(
    client: &mut EspWebSocketClient<'static>,
    stream_id: u32,
    sequence: u32,
    mode: RecordMode,
    codec: Codec,
) -> Result<()> {
    let mut frame = [0u8; HEADER_LEN];
    let header = AudioFrameHeader::new(
        codec,
        CHANNELS,
        SAMPLE_RATE,
        sequence,
        esp_timer_us(),
        0,
        stream_id,
        FLAG_STREAM_END | mode_flags(mode),
    );
    header
        .encode_into(&mut frame)
        .map_err(|err| anyhow!("encode stream end frame: {err:?}"))?;
    client
        .send(FrameType::Binary(false), &frame)
        .context("send stream end frame")
}

fn send_stream_end_ble(
    ble_audio: &ble::BleAudioServer,
    stream_id: u32,
    sequence: u32,
    mode: RecordMode,
    codec: Codec,
) -> Result<()> {
    let mut frame = [0u8; HEADER_LEN];
    let header = AudioFrameHeader::new(
        codec,
        CHANNELS,
        SAMPLE_RATE,
        sequence,
        esp_timer_us(),
        0,
        stream_id,
        FLAG_STREAM_END | mode_flags(mode),
    );
    header
        .encode_into(&mut frame)
        .map_err(|err| anyhow!("encode Bluetooth stream end frame: {err:?}"))?;
    ble_audio
        .notify_frame(sequence, &frame)
        .context("send Bluetooth stream end frame")
}

fn mode_flags(mode: RecordMode) -> u16 {
    if mode.is_push_to_talk() {
        FLAG_PUSH_TO_TALK
    } else {
        0
    }
}

fn decrement_queued_frames(queued_frames: &AtomicUsize) {
    let mut current = queued_frames.load(Ordering::Relaxed);
    while current > 0 {
        match queued_frames.compare_exchange(
            current,
            current - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(next) => current = next,
        }
    }
}

fn next_stream_id() -> u32 {
    esp_timer_us() as u32
}

fn pcm_peak_percent(pcm: &[u8]) -> u8 {
    let mut peak = 0i32;
    for sample in pcm.chunks_exact(2) {
        let value = i16::from_le_bytes([sample[0], sample[1]]) as i32;
        let amplitude = if value < 0 { -value } else { value };
        if amplitude > peak {
            peak = amplitude;
        }
    }

    ((peak * 100) / 32_768).min(100) as u8
}

fn drain_i2s(audio: &usb_audio::UsbAudio, frames: usize) -> Result<()> {
    let mut scratch = [0u8; PCM_BYTES];
    for _ in 0..frames {
        audio.read_exact(&mut scratch)?;
    }
    Ok(())
}

fn set_battery_from_pm1(
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    display: &mut display::StickDisplay<'_>,
) {
    display.set_battery(read_battery_view(pm1_i2c));
}

fn refresh_battery(
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    display: &mut display::StickDisplay<'_>,
) -> Result<()> {
    display.update_battery(read_battery_view(pm1_i2c))
}

fn apply_idle_brightness(
    display: &mut display::StickDisplay<'_>,
    settings: &AppSettings,
) -> Result<()> {
    let brightness = display_brightness_for_power(display, settings);
    display.set_brightness(brightness)
}

fn display_brightness_for_power(
    display: &display::StickDisplay<'_>,
    settings: &AppSettings,
) -> display::Brightness {
    if display.external_power() || settings.battery_brightness == BatteryBrightness::Full {
        display::Brightness::Full
    } else {
        display::Brightness::Dim
    }
}

fn read_battery_view(pm1_i2c: &mut Option<i2c_bus::I2cDevice>) -> display::BatteryView {
    let Some(pm1_i2c) = pm1_i2c.as_mut() else {
        return display::BatteryView::unknown();
    };

    match power::read_battery_status(pm1_i2c) {
        Ok(status) => {
            let input_power = power::read_input_mv(pm1_i2c)
                .map(|(vin_mv, five_volt_mv)| vin_mv >= 4_300 || five_volt_mv >= 4_300)
                .unwrap_or(false);
            display::BatteryView {
                percent: Some(status.percent),
                external_power: input_power
                    || matches!(
                        status.power_source,
                        power::PowerSource::FiveVoltIn | power::PowerSource::FiveVoltInOut
                    ),
            }
        }
        Err(err) => {
            warn!("battery read failed: {err}");
            display::BatteryView::unknown()
        }
    }
}

fn wait_for_idle_action(
    button_a: &PinDriver<Input>,
    button_b: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    control_socket: &UdpSocket,
    ble_audio: Option<&ble::BleAudioServer>,
    control_lease: &mut ControlLease,
) -> Result<IdleAction> {
    let mut next_battery_refresh_us = esp_timer_us().saturating_add(BATTERY_REFRESH_US);
    loop {
        if let Some(mode) = consume_record_request(button_a) {
            return Ok(IdleAction::Record { mode, priority: 0 });
        }
        if let Some(action) = consume_button_b_action(button_b) {
            return Ok(match action {
                ButtonBAction::CycleMode => IdleAction::CycleMode,
                ButtonBAction::Setup => IdleAction::Setup,
            });
        }
        if let Some(event) = poll_accepted_control_event(control_socket, ble_audio, control_lease)?
        {
            return Ok(match event {
                ControlEvent::SetMode(command) => IdleAction::SetMode(command),
                ControlEvent::RecordStart { priority } => IdleAction::Record {
                    mode: RecordMode::Latched,
                    priority,
                },
                ControlEvent::RecordStop => continue,
                ControlEvent::ProvisionWifi(credentials) => IdleAction::ProvisionWifi(credentials),
            });
        }

        let now_us = esp_timer_us();
        if now_us >= next_battery_refresh_us {
            refresh_battery(pm1_i2c, display).context("draw battery")?;
            next_battery_refresh_us = now_us.saturating_add(BATTERY_REFRESH_US);
        }

        FreeRtos::delay_ms(20);
    }
}

fn wait_for_usb_action(
    button_b: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    audio: &usb_audio::UsbAudio,
    control_socket: &UdpSocket,
    ble_audio: Option<&ble::BleAudioServer>,
    control_lease: &mut ControlLease,
) -> Result<IdleAction> {
    let mut next_battery_refresh_us = esp_timer_us().saturating_add(BATTERY_REFRESH_US);
    let mut next_level_refresh_us = esp_timer_us().saturating_add(LEVEL_REFRESH_US);
    let mut last_level = u8::MAX;

    loop {
        if let Some(action) = consume_button_b_action(button_b) {
            return Ok(match action {
                ButtonBAction::CycleMode => IdleAction::CycleMode,
                ButtonBAction::Setup => IdleAction::Setup,
            });
        }
        if let Some(event) = poll_accepted_control_event(control_socket, ble_audio, control_lease)?
        {
            match event {
                ControlEvent::SetMode(command) => match command.mode {
                    ActiveMode::Usb => {}
                    _ => return Ok(IdleAction::SetMode(command)),
                },
                ControlEvent::RecordStart { .. } | ControlEvent::RecordStop => {}
                ControlEvent::ProvisionWifi(credentials) => {
                    return Ok(IdleAction::ProvisionWifi(credentials));
                }
            }
        }

        let now_us = esp_timer_us();
        if now_us >= next_battery_refresh_us {
            refresh_battery(pm1_i2c, display).context("draw battery")?;
            next_battery_refresh_us = now_us.saturating_add(BATTERY_REFRESH_US);
        }
        if now_us >= next_level_refresh_us {
            let level = audio.level();
            if level != last_level {
                display.update_level(level).context("draw USB level")?;
                last_level = level;
            }
            next_level_refresh_us = now_us.saturating_add(LEVEL_REFRESH_US);
        }

        FreeRtos::delay_ms(20);
    }
}

fn consume_record_request(button: &PinDriver<Input>) -> Option<RecordMode> {
    if !button.is_low() {
        return None;
    }

    FreeRtos::delay_ms(30);
    if !button.is_low() {
        return None;
    }

    let started_us = esp_timer_us();
    let hold_us = PUSH_TO_TALK_HOLD_MS as u64 * 1_000;
    while button.is_low() {
        if esp_timer_us().saturating_sub(started_us) >= hold_us {
            return Some(RecordMode::PushToTalk);
        }
        FreeRtos::delay_ms(20);
    }

    FreeRtos::delay_ms(30);
    Some(RecordMode::Latched)
}

fn consume_button_b_action(button: &PinDriver<Input>) -> Option<ButtonBAction> {
    if !button.is_low() {
        return None;
    }

    FreeRtos::delay_ms(30);
    if !button.is_low() {
        return None;
    }

    let started_us = esp_timer_us();
    let setup_hold_us = SETUP_IDLE_HOLD_MS as u64 * 1_000;
    while button.is_low() {
        if esp_timer_us().saturating_sub(started_us) >= setup_hold_us {
            wait_for_button_release(button);
            return Some(ButtonBAction::Setup);
        }
        FreeRtos::delay_ms(20);
    }

    FreeRtos::delay_ms(30);
    Some(ButtonBAction::CycleMode)
}

fn button_held(button: &PinDriver<Input>, hold_ms: u32) -> bool {
    if !button.is_low() {
        return false;
    }

    let started_us = esp_timer_us();
    let hold_us = hold_ms as u64 * 1_000;
    while button.is_low() {
        if esp_timer_us().saturating_sub(started_us) >= hold_us {
            return true;
        }
        FreeRtos::delay_ms(20);
    }
    false
}

fn wait_for_button_release(button: &PinDriver<Input>) {
    while button.is_low() {
        FreeRtos::delay_ms(20);
    }
    FreeRtos::delay_ms(30);
}

fn consume_button_press(button: &PinDriver<Input>) -> bool {
    if !button.is_low() {
        return false;
    }

    FreeRtos::delay_ms(30);
    if !button.is_low() {
        return false;
    }

    wait_for_button_release(button);
    true
}

fn esp_timer_us() -> u64 {
    unsafe { esp_idf_sys::esp_timer_get_time() as u64 }
}
