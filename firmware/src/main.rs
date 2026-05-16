mod audio;
mod codec;
mod discovery;
mod display;
mod i2c_bus;
mod power;
mod setup;
mod usb_audio;
mod wifi_config;

use std::{
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
    peripherals::Peripherals,
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
    AudioFrameHeader, Codec, FLAG_PUSH_TO_TALK, FLAG_STREAM_END, FLAG_STREAM_START, HEADER_LEN,
};
use usb_audio::TransportMode;

const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u8 = 1;
const FRAME_MS: usize = 40;
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize * FRAME_MS / 1_000;
const PCM_BYTES: usize = FRAME_SAMPLES * 2;
const FRAME_BYTES: usize = HEADER_LEN + PCM_BYTES;
const AUDIO_BUFFER_FRAMES: usize = 8;
const DRAIN_FRAMES: usize = 8;
const BATTERY_REFRESH_US: u64 = 30_000_000;
const LEVEL_REFRESH_US: u64 = 200_000;
const SETUP_BOOT_HOLD_MS: u32 = 1_200;
const SETUP_IDLE_HOLD_MS: u32 = 2_000;
const PUSH_TO_TALK_HOLD_MS: u32 = 450;
const CAPTURE_THREAD_STACK: usize = 6_144;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdleAction {
    Record(RecordMode),
    Setup,
    ToggleTransport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ButtonBAction {
    ToggleTransport,
    Setup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordMode {
    Latched,
    PushToTalk,
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
    level: u8,
    sequence: u32,
}

impl CapturedFrame {
    fn new(sequence: u32) -> Self {
        Self {
            bytes: [0; FRAME_BYTES],
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

    let mut display = display::StickDisplay::new(
        peripherals.spi3,
        peripherals.pins.gpio40,
        peripherals.pins.gpio39,
        peripherals.pins.gpio41,
        peripherals.pins.gpio45,
        peripherals.pins.gpio21,
        peripherals.pins.gpio38,
    )
    .context("create display")?;
    set_battery_from_pm1(&mut pm1_i2c, &mut display);
    display
        .show_wifi_connecting()
        .context("draw wifi connecting")?;

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
        setup::run(&mut wifi, wifi_store.clone(), &mut display, &setup_ssid)?;
        return Ok(());
    }

    if let Err(err) = connect_wifi(&mut wifi, &wifi_store) {
        warn!("wifi connection failed: {err:#}");
        display
            .show_error("WIFI FAIL", "HOLD B SETUP")
            .context("draw wifi setup hint")?;
        wait_for_setup_hold(&button_b);
        setup::run(&mut wifi, wifi_store.clone(), &mut display, &setup_ssid)?;
        return Ok(());
    }
    display.show_ready().context("draw ready screen")?;

    let mut mdns = EspMdns::take().context("start mdns")?;
    mdns.set_hostname("m5sticks3-mic")
        .context("set mdns hostname")?;
    mdns.set_instance_name("M5StickS3 Mic")
        .context("set mdns instance")?;

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

    info!("press BtnA to start recording");
    let mut cached_receiver = None;
    let mut transport = TransportMode::Wireless;
    loop {
        if transport == TransportMode::Usb {
            usb_audio.set_transport(TransportMode::Usb);
            display.show_usb_ready().context("draw USB mic screen")?;
            match wait_for_usb_action(&button_b, &mut display, &mut pm1_i2c, &usb_audio)
                .context("wait for USB mode action")?
            {
                IdleAction::ToggleTransport => {
                    transport = TransportMode::Wireless;
                    usb_audio.set_transport(TransportMode::Wireless);
                    refresh_battery(&mut pm1_i2c, &mut display).context("draw battery")?;
                    display.show_ready().context("draw ready screen")?;
                    info!("transport switched to wireless");
                    continue;
                }
                IdleAction::Setup => {
                    info!("BtnB held while idle; entering setup portal");
                    setup::run(&mut wifi, wifi_store.clone(), &mut display, &setup_ssid)?;
                    return Ok(());
                }
                IdleAction::Record(_) => continue,
            }
        }

        let mode = match wait_for_idle_action(&button_a, &button_b, &mut display, &mut pm1_i2c)
            .context("wait for idle action")?
        {
            IdleAction::Record(mode) => mode,
            IdleAction::ToggleTransport => {
                transport = TransportMode::Usb;
                usb_audio.set_transport(TransportMode::Usb);
                refresh_battery(&mut pm1_i2c, &mut display).context("draw battery")?;
                display.show_usb_ready().context("draw USB mic screen")?;
                info!("transport switched to USB");
                continue;
            }
            IdleAction::Setup => {
                info!("BtnB held while idle; entering setup portal");
                setup::run(&mut wifi, wifi_store.clone(), &mut display, &setup_ssid)?;
                return Ok(());
            }
        };
        info!("recording requested: {mode:?}");

        match record_once(
            &mdns,
            &mut cached_receiver,
            &usb_audio,
            &button_a,
            &mut display,
            &mut pm1_i2c,
            mode,
        ) {
            Ok(()) => {
                info!("recording stopped");
                refresh_battery(&mut pm1_i2c, &mut display).context("draw battery")?;
                display.show_ready().context("draw ready screen")?;
            }
            Err(err) => {
                warn!("recording failed: {err:#}");
                display
                    .show_error("STREAM", "CHECK SERVER")
                    .context("draw stream error")?;
            }
        }

        FreeRtos::delay_ms(750);
        refresh_battery(&mut pm1_i2c, &mut display).context("draw battery")?;
        display.show_ready().context("draw ready screen")?;
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

fn record_once(
    mdns: &EspMdns,
    cached_receiver: &mut Option<String>,
    audio: &usb_audio::UsbAudio,
    button_a: &PinDriver<Input>,
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    mode: RecordMode,
) -> Result<()> {
    if let Some(server_url) = cached_receiver.clone() {
        info!("using cached receiver: {server_url}");
        match connect_audio(&server_url) {
            Ok(client) => {
                info!("recording started; press BtnA to stop");
                let result =
                    stream_audio_connected(client, audio, button_a, display, pm1_i2c, mode);
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
        .show_finding_receiver()
        .context("draw finding receiver")?;
    let server_url = discovery::discover_server(mdns, |phase| {
        if let Err(err) = display.show_finding_receiver_phase(phase) {
            warn!("failed to animate receiver discovery: {err:#}");
        }
    })
    .context("discover receiver")?;
    info!("receiver: {server_url}");
    let client = connect_audio(&server_url).context("connect discovered receiver")?;
    *cached_receiver = Some(server_url);

    info!("recording started; press BtnA to stop");
    let result = stream_audio_connected(client, audio, button_a, display, pm1_i2c, mode);
    if result.is_err() {
        *cached_receiver = None;
    }
    result
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
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    mode: RecordMode,
) -> Result<()> {
    drain_i2s(audio, DRAIN_FRAMES).context("drain pre-stream audio")?;
    refresh_battery(pm1_i2c, display).context("draw battery")?;
    display
        .show_recording(0, mode.display_mode())
        .context("draw recording screen")?;
    display
        .update_buffer(0, AUDIO_BUFFER_FRAMES)
        .context("draw buffer meter")?;

    let stream_id = next_stream_id();
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
                capture_audio_frames(audio, tx, &stop_capture, &queued_frames, stream_id, mode)
            })
            .map_err(|err| anyhow!("spawn audio capture thread: {err}"))?;

        let send_result = send_captured_audio(
            &mut client,
            rx,
            &queued_frames,
            button_a,
            display,
            pm1_i2c,
            mode,
            &mut last_sequence,
            &mut have_sent_audio,
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
        send_stream_end(&mut client, stream_id, last_sequence.wrapping_add(1), mode)
            .context("send stream end")?;
    }

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
) -> Result<()> {
    let mut sequence = 0u32;
    let mut first = true;

    while !stop_capture.load(Ordering::Relaxed) {
        let mut frame = CapturedFrame::new(sequence);
        audio.read_exact(&mut frame.bytes[HEADER_LEN..])?;
        frame.level = pcm_peak_percent(&frame.bytes[HEADER_LEN..]);

        let mut flags = mode_flags(mode);
        if first {
            flags |= FLAG_STREAM_START;
        }
        let header = AudioFrameHeader::new(
            Codec::PcmS16Le,
            CHANNELS,
            SAMPLE_RATE,
            sequence,
            esp_timer_us(),
            PCM_BYTES as u16,
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
    display: &mut display::StickDisplay<'_>,
    pm1_i2c: &mut Option<i2c_bus::I2cDevice>,
    mode: RecordMode,
    last_sequence: &mut u32,
    have_sent_audio: &mut bool,
) -> Result<StreamStop> {
    let started_us = esp_timer_us();
    let mut last_elapsed_secs = 0;
    let mut next_battery_refresh_us = started_us.saturating_add(BATTERY_REFRESH_US);
    let mut next_level_refresh_us = started_us.saturating_add(LEVEL_REFRESH_US);
    let mut meter_level = 0u8;
    let mut last_buffer_frames = usize::MAX;

    loop {
        if should_stop_stream(mode, button_a) {
            return Ok(StreamStop::User);
        }

        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(frame) => {
                decrement_queued_frames(queued_frames);
                if !client.is_connected() {
                    return Err(anyhow!("websocket disconnected"));
                }
                client
                    .send(FrameType::Binary(false), &frame.bytes)
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
        if elapsed_secs != last_elapsed_secs {
            display
                .update_recording_time(elapsed_secs)
                .context("draw recording time")?;
            last_elapsed_secs = elapsed_secs;
        }
        if now_us >= next_battery_refresh_us {
            refresh_battery(pm1_i2c, display).context("draw battery")?;
            next_battery_refresh_us = now_us.saturating_add(BATTERY_REFRESH_US);
        }
        if now_us >= next_level_refresh_us {
            display
                .update_level(meter_level)
                .context("draw recording level")?;
            next_level_refresh_us = now_us.saturating_add(LEVEL_REFRESH_US);
        }

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

fn should_stop_stream(mode: RecordMode, button_a: &PinDriver<Input>) -> bool {
    match mode {
        RecordMode::Latched => consume_button_press(button_a),
        RecordMode::PushToTalk => !button_a.is_low(),
    }
}

fn send_stream_end(
    client: &mut EspWebSocketClient<'static>,
    stream_id: u32,
    sequence: u32,
    mode: RecordMode,
) -> Result<()> {
    let mut frame = [0u8; HEADER_LEN];
    let header = AudioFrameHeader::new(
        Codec::PcmS16Le,
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

fn read_battery_view(pm1_i2c: &mut Option<i2c_bus::I2cDevice>) -> display::BatteryView {
    let Some(pm1_i2c) = pm1_i2c.as_mut() else {
        return display::BatteryView::unknown();
    };

    match power::read_battery_status(pm1_i2c) {
        Ok(status) => display::BatteryView {
            percent: Some(status.percent),
            external_power: matches!(
                status.power_source,
                power::PowerSource::FiveVoltIn | power::PowerSource::FiveVoltInOut
            ),
        },
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
) -> Result<IdleAction> {
    let mut next_battery_refresh_us = esp_timer_us().saturating_add(BATTERY_REFRESH_US);
    loop {
        if let Some(mode) = consume_record_request(button_a) {
            return Ok(IdleAction::Record(mode));
        }
        if let Some(action) = consume_button_b_action(button_b) {
            return Ok(match action {
                ButtonBAction::ToggleTransport => IdleAction::ToggleTransport,
                ButtonBAction::Setup => IdleAction::Setup,
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
) -> Result<IdleAction> {
    let mut next_battery_refresh_us = esp_timer_us().saturating_add(BATTERY_REFRESH_US);
    let mut next_level_refresh_us = esp_timer_us().saturating_add(LEVEL_REFRESH_US);
    let mut last_level = u8::MAX;

    loop {
        if let Some(action) = consume_button_b_action(button_b) {
            return Ok(match action {
                ButtonBAction::ToggleTransport => IdleAction::ToggleTransport,
                ButtonBAction::Setup => IdleAction::Setup,
            });
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

fn wait_for_setup_hold(button: &PinDriver<Input>) {
    loop {
        if button_held(button, SETUP_IDLE_HOLD_MS) {
            wait_for_button_release(button);
            return;
        }
        FreeRtos::delay_ms(20);
    }
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
    Some(ButtonBAction::ToggleTransport)
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
