mod ble;

use std::{
    collections::BTreeSet,
    env,
    ffi::CStr,
    mem::size_of,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    path::{Path, PathBuf},
    process::Command,
    ptr, thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use coreaudio_sys::*;
use if_addrs::{get_if_addrs, IfAddr};
use m5mic_protocol::{
    CONTROL_MODE_BLE, CONTROL_MODE_USB, CONTROL_MODE_WIFI, CONTROL_PORT, CONTROL_RECORD_START,
    CONTROL_RECORD_STOP, DISCOVERY_PORT, WS_PORT,
};
use m5mic_receiver::{run, ReceiverConfig, ReceiverStatus};
use objc2::rc::Retained;
use objc2_core_location::{CLAuthorizationStatus, CLLocationManager};
use objc2_core_wlan::{CWKeychainDomain, CWKeychainFindWiFiPassword};
use objc2_foundation::{NSData, NSString};
use security_framework::{
    item::{ItemClass, ItemSearchOptions, SearchResult},
    os::macos::keychain::SecKeychain,
};
use serde_json::Value;
use tokio::{
    runtime::{Handle, Runtime},
    sync::watch,
    time,
};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};
use winit::{
    event::Event,
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
};

const INSTALLED_DRIVER_PATH: &str = "/Library/Audio/Plug-Ins/HAL/m5mic.driver";

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0")]
    listen: String,

    #[arg(long, default_value_t = WS_PORT)]
    ws_port: u16,

    #[arg(long, default_value_t = DISCOVERY_PORT)]
    discovery_port: u16,

    #[arg(long)]
    output_dir: Option<PathBuf>,

    #[arg(long, default_value = "M5Mic Receiver")]
    instance: String,
}

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    Status(ReceiverStatus),
    Ble(ble::BleReceiverStatus),
    Usb(UsbStatus),
    Driver(DriverStatus),
    DriverInstall(DriverInstallResult),
    WifiProvision(WifiProvisionResult),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UsbStatus {
    Connected,
    Disconnected,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriverStatus {
    Installed,
    Missing,
    Unavailable,
}

#[derive(Debug)]
enum DriverInstallResult {
    Installed,
    Skipped,
    Failed(String),
}

#[derive(Debug)]
enum WifiProvisionResult {
    Sent,
    Skipped,
    Failed(String),
}

#[derive(Debug)]
struct WifiProvisionRequest {
    ssid: String,
    password: String,
    setup_code: String,
}

#[derive(Debug)]
struct WifiNetwork {
    ssid: String,
    ssid_bytes: Option<Vec<u8>>,
}

impl WifiNetwork {
    fn named(ssid: String) -> Self {
        Self {
            ssid,
            ssid_bytes: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputMode {
    Wifi,
    Bluetooth,
    Usb,
}

impl InputMode {
    const fn menu_label(self) -> &'static str {
        match self {
            Self::Wifi => "Wi-Fi",
            Self::Bluetooth => "Bluetooth",
            Self::Usb => "USB",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingCommand {
    Start,
    Stop,
}

impl RecordingCommand {
    const fn menu_label(self) -> &'static str {
        match self {
            Self::Start => "start recording",
            Self::Stop => "stop recording",
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "m5mic_statusbar=info".to_string()),
        )
        .init();

    let args = Args::parse();
    let runtime = Runtime::new().context("start tokio runtime")?;
    let runtime_handle = runtime.handle().clone();
    let (status_tx, status_rx) = watch::channel(ReceiverStatus::Starting);
    let (ble_status_tx, ble_status_rx) = watch::channel(ble::BleReceiverStatus::Disabled);
    let (ble_enabled_tx, ble_enabled_rx) = watch::channel(false);

    let config = ReceiverConfig {
        listen: args.listen,
        ws_port: args.ws_port,
        discovery_port: args.discovery_port,
        output_dir: args.output_dir,
        instance: args.instance,
        virtual_mic: true,
    };

    let receiver_status_tx = status_tx.clone();
    runtime.spawn(async move {
        if let Err(err) = run(config, Some(receiver_status_tx.clone())).await {
            let _ = receiver_status_tx.send(ReceiverStatus::Error(err.to_string()));
        }
    });
    runtime.spawn(ble::run(ble_status_tx, ble_enabled_rx));

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("create event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event| {
            let _ = proxy.send_event(UserEvent::Menu(event));
        }
    }));

    runtime.spawn({
        let proxy = proxy.clone();
        async move {
            let mut status_rx = status_rx;
            loop {
                if status_rx.changed().await.is_err() {
                    break;
                }
                let _ = proxy.send_event(UserEvent::Status(status_rx.borrow().clone()));
            }
        }
    });

    runtime.spawn({
        let proxy = proxy.clone();
        async move {
            let mut ble_status_rx = ble_status_rx;
            loop {
                if ble_status_rx.changed().await.is_err() {
                    break;
                }
                let _ = proxy.send_event(UserEvent::Ble(ble_status_rx.borrow().clone()));
            }
        }
    });

    runtime.spawn({
        let proxy = proxy.clone();
        async move {
            let mut last_status = None;
            loop {
                let status = usb_status();
                if last_status != Some(status) {
                    last_status = Some(status);
                    let _ = proxy.send_event(UserEvent::Usb(status));
                }
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    runtime.spawn({
        let proxy = proxy.clone();
        async move {
            let mut last_status = None;
            loop {
                let status = driver_status();
                if last_status != Some(status) {
                    last_status = Some(status);
                    let _ = proxy.send_event(UserEvent::Driver(status));
                }
                time::sleep(Duration::from_secs(10)).await;
            }
        }
    });

    let initial_usb_status = usb_status();
    let initial_driver_status = driver_status();
    let status_item = MenuItem::new("Status: starting", false, None);
    let driver_status_item = MenuItem::new("Driver: checking", false, None);
    let install_driver_item =
        MenuItem::with_id("install-driver", "Install Audio Driver...", true, None);
    let usb_status_item = MenuItem::new("USB: checking", false, None);
    let bluetooth_status_item = MenuItem::new("Bluetooth: off", false, None);
    let provision_wifi_item =
        MenuItem::with_id("provision-wifi", "Send Wi-Fi over Bluetooth...", true, None);
    let wifi_mode_item = MenuItem::with_id("mode-wifi", "Use Wi-Fi Mode", true, None);
    let bluetooth_mode_item = MenuItem::with_id("mode-bluetooth", "Use Bluetooth Mode", true, None);
    let usb_mode_item = MenuItem::with_id("mode-usb", "Use USB Mode", true, None);
    let start_recording_item = MenuItem::with_id("record-start", "Start Recording", true, None);
    let stop_recording_item = MenuItem::with_id("record-stop", "Stop Recording", true, None);
    let settings_item = MenuItem::with_id("sound-settings", "Open Sound Settings", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit m5mic", true, None);
    let separator = PredefinedMenuItem::separator();
    let menu = Menu::new();
    menu.append(&status_item).context("add status menu item")?;
    menu.append(&driver_status_item)
        .context("add driver status menu item")?;
    menu.append(&install_driver_item)
        .context("add driver install menu item")?;
    menu.append(&usb_status_item)
        .context("add USB status menu item")?;
    menu.append(&bluetooth_status_item)
        .context("add Bluetooth status menu item")?;
    menu.append(&provision_wifi_item)
        .context("add Bluetooth Wi-Fi provisioning item")?;
    menu.append(&wifi_mode_item)
        .context("add Wi-Fi mode menu item")?;
    menu.append(&bluetooth_mode_item)
        .context("add Bluetooth mode menu item")?;

    let mut usb_mode_visible = matches!(initial_usb_status, UsbStatus::Connected);
    if usb_mode_visible {
        menu.append(&usb_mode_item)
            .context("add USB mode menu item")?;
    }
    menu.append(&start_recording_item)
        .context("add recording start menu item")?;
    menu.append(&stop_recording_item)
        .context("add recording stop menu item")?;
    menu.append(&separator).context("add menu separator")?;
    menu.append(&settings_item)
        .context("add sound settings menu item")?;
    menu.append(&quit_item).context("add quit menu item")?;
    let menu_handle = menu.clone();
    let mut current_driver_status = initial_driver_status;
    let mut latest_receiver_status = ReceiverStatus::Starting;
    let mut latest_ble_status = ble::BleReceiverStatus::Disabled;
    let mut driver_install_running = false;
    let mut driver_install_prompted = false;
    let mut location_manager: Option<Retained<CLLocationManager>> = None;
    sync_driver_menu(
        &driver_status_item,
        &install_driver_item,
        current_driver_status,
        driver_install_running,
    );

    let tray = TrayIconBuilder::new()
        .with_tooltip("m5mic")
        .with_icon(icon_for_status(&ReceiverStatus::Starting)?)
        .with_menu(Box::new(menu))
        .build()
        .context("create tray icon")?;

    let event_proxy = proxy.clone();
    if matches!(current_driver_status, DriverStatus::Missing) {
        driver_install_running = true;
        driver_install_prompted = true;
        status_item.set_text("Status: audio driver required");
        sync_driver_menu(
            &driver_status_item,
            &install_driver_item,
            current_driver_status,
            driver_install_running,
        );
        spawn_driver_install_prompt(event_proxy.clone());
    }

    #[allow(deprecated)]
    let run_result = event_loop.run(move |event, event_loop| match event {
        Event::UserEvent(UserEvent::Status(status)) => {
            latest_receiver_status = status;
            sync_status_menu(
                &status_item,
                &tray,
                &latest_receiver_status,
                &latest_ble_status,
            );
        }
        Event::UserEvent(UserEvent::Ble(status)) => {
            latest_ble_status = status;
            bluetooth_status_item.set_text(format!(
                "Bluetooth: {}",
                bluetooth_status_text(&latest_ble_status)
            ));
            sync_status_menu(
                &status_item,
                &tray,
                &latest_receiver_status,
                &latest_ble_status,
            );
        }
        Event::UserEvent(UserEvent::Usb(status)) => {
            usb_status_item.set_text(format!("USB: {}", usb_status_text(status)));
            sync_usb_mode_item(&menu_handle, &usb_mode_item, status, &mut usb_mode_visible);
        }
        Event::UserEvent(UserEvent::Driver(status)) => {
            current_driver_status = status;
            sync_driver_menu(
                &driver_status_item,
                &install_driver_item,
                current_driver_status,
                driver_install_running,
            );
            if !driver_install_prompted && matches!(current_driver_status, DriverStatus::Missing) {
                driver_install_running = true;
                driver_install_prompted = true;
                status_item.set_text("Status: audio driver required");
                sync_driver_menu(
                    &driver_status_item,
                    &install_driver_item,
                    current_driver_status,
                    driver_install_running,
                );
                spawn_driver_install_prompt(event_proxy.clone());
            }
        }
        Event::UserEvent(UserEvent::DriverInstall(result)) => {
            driver_install_running = false;
            current_driver_status = driver_status();
            match result {
                DriverInstallResult::Installed => {
                    status_item.set_text("Status: audio driver installed");
                }
                DriverInstallResult::Skipped => {
                    status_item.set_text("Status: audio driver not installed");
                }
                DriverInstallResult::Failed(err) => {
                    tracing::warn!(?err, "failed to install audio driver");
                    status_item.set_text("Status: audio driver install failed");
                }
            }
            sync_driver_menu(
                &driver_status_item,
                &install_driver_item,
                current_driver_status,
                driver_install_running,
            );
        }
        Event::UserEvent(UserEvent::WifiProvision(result)) => match result {
            WifiProvisionResult::Sent => {
                status_item.set_text("Status: Wi-Fi sent over Bluetooth");
            }
            WifiProvisionResult::Skipped => {
                status_item.set_text("Status: Wi-Fi setup canceled");
            }
            WifiProvisionResult::Failed(err) => {
                tracing::warn!(?err, "Bluetooth Wi-Fi provisioning failed");
                status_item.set_text("Status: Bluetooth Wi-Fi setup failed");
            }
        },
        Event::UserEvent(UserEvent::Menu(event)) => {
            if event.id == MenuId::from("sound-settings") {
                open_sound_settings();
            } else if event.id == MenuId::from("quit") {
                event_loop.exit();
            } else if event.id == MenuId::from("mode-wifi") {
                let _ = ble_enabled_tx.send(false);
                set_menu_input_mode(InputMode::Wifi, &status_item);
                spawn_ble_mode_command(&runtime_handle, InputMode::Wifi);
            } else if event.id == MenuId::from("mode-bluetooth") {
                let _ = ble_enabled_tx.send(true);
                set_menu_input_mode(InputMode::Bluetooth, &status_item);
                spawn_ble_mode_command(&runtime_handle, InputMode::Bluetooth);
            } else if event.id == MenuId::from("mode-usb") {
                let _ = ble_enabled_tx.send(false);
                set_menu_input_mode(InputMode::Usb, &status_item);
                spawn_ble_mode_command(&runtime_handle, InputMode::Usb);
            } else if event.id == MenuId::from("record-start") {
                set_menu_recording_command(RecordingCommand::Start, &status_item);
                spawn_ble_recording_command(&runtime_handle, RecordingCommand::Start);
            } else if event.id == MenuId::from("record-stop") {
                set_menu_recording_command(RecordingCommand::Stop, &status_item);
                spawn_ble_recording_command(&runtime_handle, RecordingCommand::Stop);
            } else if event.id == MenuId::from("provision-wifi") {
                status_item.set_text("Status: setting up Wi-Fi over Bluetooth");
                let wait_for_location =
                    request_location_authorization_if_needed(&mut location_manager);
                spawn_wifi_provision_prompt(
                    event_proxy.clone(),
                    runtime_handle.clone(),
                    wait_for_location,
                );
            } else if event.id == MenuId::from("install-driver") && !driver_install_running {
                driver_install_running = true;
                status_item.set_text("Status: installing audio driver");
                sync_driver_menu(
                    &driver_status_item,
                    &install_driver_item,
                    current_driver_status,
                    driver_install_running,
                );
                spawn_driver_install_prompt(event_proxy.clone());
            }
        }
        _ => {}
    });
    run_result.context("run event loop")?;

    drop(runtime);
    Ok(())
}

fn status_text(status: &ReceiverStatus) -> String {
    match status {
        ReceiverStatus::Starting => "starting".to_string(),
        ReceiverStatus::Waiting => "waiting".to_string(),
        ReceiverStatus::Connected => "connected".to_string(),
        ReceiverStatus::Receiving { stream_id } => format!("recording {stream_id:08x}"),
        ReceiverStatus::Stopped => "stopped".to_string(),
        ReceiverStatus::Error(err) => format!("error: {err}"),
    }
}

fn bluetooth_status_text(status: &ble::BleReceiverStatus) -> String {
    match status {
        ble::BleReceiverStatus::Disabled => "off".to_string(),
        ble::BleReceiverStatus::Scanning => "scanning".to_string(),
        ble::BleReceiverStatus::Connecting => "connecting".to_string(),
        ble::BleReceiverStatus::Connected => "connected".to_string(),
        ble::BleReceiverStatus::Receiving { stream_id } => format!("recording {stream_id:08x}"),
        ble::BleReceiverStatus::Error(err) => format!("error: {err}"),
    }
}

fn sync_status_menu(
    status_item: &MenuItem,
    tray: &tray_icon::TrayIcon,
    receiver_status: &ReceiverStatus,
    ble_status: &ble::BleReceiverStatus,
) {
    let text = if matches!(ble_status, ble::BleReceiverStatus::Receiving { .. }) {
        format!("Bluetooth {}", bluetooth_status_text(ble_status))
    } else {
        status_text(receiver_status)
    };
    status_item.set_text(format!("Status: {text}"));
    let _ = tray.set_tooltip(Some(format!("m5mic: {text}")));
    let _ = tray.set_icon(Some(
        icon_for_combined_status(receiver_status, ble_status).unwrap_or_else(|_| fallback_icon()),
    ));
}

fn icon_for_combined_status(
    receiver_status: &ReceiverStatus,
    ble_status: &ble::BleReceiverStatus,
) -> Result<Icon> {
    if matches!(receiver_status, ReceiverStatus::Receiving { .. })
        || matches!(ble_status, ble::BleReceiverStatus::Receiving { .. })
    {
        make_recording_icon()
    } else if matches!(receiver_status, ReceiverStatus::Error(_))
        || matches!(ble_status, ble::BleReceiverStatus::Error(_))
    {
        make_error_icon()
    } else {
        make_idle_icon()
    }
}

fn icon_for_status(status: &ReceiverStatus) -> Result<Icon> {
    match status {
        ReceiverStatus::Receiving { .. } => make_recording_icon(),
        ReceiverStatus::Error(_) => make_error_icon(),
        ReceiverStatus::Starting
        | ReceiverStatus::Waiting
        | ReceiverStatus::Connected
        | ReceiverStatus::Stopped => make_idle_icon(),
    }
}

fn fallback_icon() -> Icon {
    make_idle_icon().expect("fallback icon")
}

fn make_idle_icon() -> Result<Icon> {
    make_dot_icon(DotIcon::Idle)
}

fn make_recording_icon() -> Result<Icon> {
    make_dot_icon(DotIcon::Recording)
}

fn make_error_icon() -> Result<Icon> {
    make_dot_icon(DotIcon::Error)
}

enum DotIcon {
    Idle,
    Recording,
    Error,
}

fn make_dot_icon(kind: DotIcon) -> Result<Icon> {
    const SIZE: u32 = 22;
    const RED: [u8; 4] = [0xff, 0x3b, 0x30, 0xff];
    const ORANGE: [u8; 4] = [0xff, 0x9f, 0x0a, 0xff];

    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let pixel = ((y * SIZE + x) * 4) as usize;

            let (color, alpha) = match kind {
                DotIcon::Recording => (RED, coverage(7.2 - distance)),
                DotIcon::Idle => (RED, ring_coverage(distance, 5.4, 7.4)),
                DotIcon::Error => (ORANGE, coverage(7.2 - distance)),
            };

            if alpha > 0 {
                rgba[pixel..pixel + 3].copy_from_slice(&color[..3]);
                rgba[pixel + 3] = alpha;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("build tray icon")
}

fn coverage(edge_distance: f32) -> u8 {
    ((edge_distance + 0.5).clamp(0.0, 1.0) * 255.0).round() as u8
}

fn ring_coverage(distance: f32, inner_radius: f32, outer_radius: f32) -> u8 {
    coverage(outer_radius - distance).min(coverage(distance - inner_radius))
}

fn usb_status_text(status: UsbStatus) -> &'static str {
    match status {
        UsbStatus::Connected => "connected",
        UsbStatus::Disconnected => "not connected",
        UsbStatus::Unknown => "unknown",
    }
}

fn driver_status() -> DriverStatus {
    if installed_driver_path().exists() {
        DriverStatus::Installed
    } else if bundled_driver_path().is_some() {
        DriverStatus::Missing
    } else {
        DriverStatus::Unavailable
    }
}

fn installed_driver_path() -> PathBuf {
    PathBuf::from(INSTALLED_DRIVER_PATH)
}

fn bundled_driver_path() -> Option<PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    let macos_dir = exe_path.parent()?;
    let contents_dir = macos_dir.parent()?;
    let app_driver = contents_dir.join("Resources").join("m5mic.driver");
    if app_driver.exists() {
        return Some(app_driver);
    }

    let dev_driver = contents_dir.join("m5mic.driver");
    dev_driver.exists().then_some(dev_driver)
}

fn sync_driver_menu(
    driver_status_item: &MenuItem,
    install_driver_item: &MenuItem,
    status: DriverStatus,
    install_running: bool,
) {
    if install_running {
        driver_status_item.set_text("Driver: installing");
        install_driver_item.set_text("Installing Audio Driver...");
        install_driver_item.set_enabled(false);
        return;
    }

    match status {
        DriverStatus::Installed => {
            driver_status_item.set_text("Driver: installed");
            install_driver_item.set_text("Audio Driver Installed");
            install_driver_item.set_enabled(false);
        }
        DriverStatus::Missing => {
            driver_status_item.set_text("Driver: install required");
            install_driver_item.set_text("Install Audio Driver...");
            install_driver_item.set_enabled(true);
        }
        DriverStatus::Unavailable => {
            driver_status_item.set_text("Driver: bundled copy missing");
            install_driver_item.set_text("Audio Driver Unavailable");
            install_driver_item.set_enabled(false);
        }
    }
}

fn spawn_driver_install_prompt(proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let result = match run_driver_install_prompt() {
            Ok(true) => DriverInstallResult::Installed,
            Ok(false) => DriverInstallResult::Skipped,
            Err(err) => DriverInstallResult::Failed(err.to_string()),
        };
        let _ = proxy.send_event(UserEvent::DriverInstall(result));
        let _ = proxy.send_event(UserEvent::Driver(driver_status()));
    });
}

fn spawn_wifi_provision_prompt(
    proxy: EventLoopProxy<UserEvent>,
    runtime: Handle,
    wait_for_location: bool,
) {
    thread::spawn(move || {
        wait_for_location_authorization_if_needed(wait_for_location);
        let result = match run_wifi_provision_prompt() {
            Ok(Some(request)) => match runtime.block_on(ble::provision_wifi(
                &request.ssid,
                &request.password,
                &request.setup_code,
            )) {
                Ok(()) => WifiProvisionResult::Sent,
                Err(err) => WifiProvisionResult::Failed(err.to_string()),
            },
            Ok(None) => WifiProvisionResult::Skipped,
            Err(err) => WifiProvisionResult::Failed(err.to_string()),
        };
        let _ = proxy.send_event(UserEvent::WifiProvision(result));
    });
}

fn request_location_authorization_if_needed(
    manager: &mut Option<Retained<CLLocationManager>>,
) -> bool {
    let location_services_enabled = unsafe { CLLocationManager::locationServicesEnabled_class() };
    if !location_services_enabled {
        tracing::debug!("location services disabled; current Wi-Fi may be unavailable");
        return false;
    }

    let manager = manager.get_or_insert_with(|| unsafe { CLLocationManager::new() });
    let status = unsafe { manager.authorizationStatus() };
    if status == CLAuthorizationStatus::NotDetermined {
        unsafe { manager.requestWhenInUseAuthorization() };
        return true;
    }

    tracing::debug!(?status, "location authorization already determined");
    false
}

fn wait_for_location_authorization_if_needed(wait_for_location: bool) {
    if !wait_for_location {
        return;
    }

    let manager = unsafe { CLLocationManager::new() };
    for _ in 0..120 {
        let status = unsafe { manager.authorizationStatus() };
        if status != CLAuthorizationStatus::NotDetermined {
            tracing::debug!(?status, "location authorization resolved");
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }

    tracing::debug!("location authorization still pending; continuing Wi-Fi setup");
}

fn run_wifi_provision_prompt() -> Result<Option<WifiProvisionRequest>> {
    let current_network = current_wifi_network();
    if let Some(network) = current_network.as_ref() {
        if let Some(password) = current_keychain_wifi_password(network) {
            return prompt_current_wifi_provision(&network.ssid, password);
        }

        return prompt_current_wifi_password_provision(&network.ssid);
    }

    prompt_manual_wifi_provision(
        "Could not read the current Wi-Fi network from macOS. Enter the Wi-Fi network name for the M5StickS3:",
        "",
    )
}

fn prompt_current_wifi_provision(
    ssid: &str,
    password: String,
) -> Result<Option<WifiProvisionRequest>> {
    let script = format!(
        "set codeResult to text returned of (display dialog {code_message} with title {title} default answer \"\")\nreturn codeResult",
        code_message = applescript_string(&format!(
            "Share this Mac's current Wi-Fi network ({ssid}) with the M5StickS3.\n\nPut the Stick in setup mode, then enter the large 8-digit code shown on its screen:"
        )),
        title = applescript_string("m5mic Bluetooth Wi-Fi Setup"),
    );
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .context("run Bluetooth Wi-Fi setup prompt")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("User canceled") {
            return Ok(None);
        }
        let message = stderr.trim();
        if message.is_empty() {
            anyhow::bail!("osascript exited with {}", output.status);
        }
        anyhow::bail!("{message}");
    }

    let stdout = String::from_utf8(output.stdout).context("decode Wi-Fi setup prompt output")?;
    let setup_code = stdout.trim().to_string();
    Ok(Some(WifiProvisionRequest {
        ssid: ssid.to_string(),
        password,
        setup_code,
    }))
}

fn prompt_current_wifi_password_provision(ssid: &str) -> Result<Option<WifiProvisionRequest>> {
    let script = format!(
        "set passResult to text returned of (display dialog {password_message} with title {title} default answer \"\" with hidden answer)\nset codeResult to text returned of (display dialog {code_message} with title {title} default answer \"\")\nreturn passResult & linefeed & codeResult",
        password_message = applescript_string(&format!(
            "Could not read the Wi-Fi password for {ssid} from Keychain.\n\nEnter the password to send this network to the M5StickS3:"
        )),
        code_message = applescript_string(
            "Put the Stick in setup mode, then enter the large 8-digit code shown on its screen:",
        ),
        title = applescript_string("m5mic Bluetooth Wi-Fi Setup"),
    );
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .context("run current Wi-Fi password setup prompt")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("User canceled") || stderr.contains("-128") {
            return Ok(None);
        }
        let message = stderr.trim();
        if message.is_empty() {
            anyhow::bail!("osascript exited with {}", output.status);
        }
        anyhow::bail!("{message}");
    }

    let stdout = String::from_utf8(output.stdout).context("decode current Wi-Fi setup output")?;
    let mut lines = stdout.trim_end_matches('\n').splitn(2, '\n');
    let password = lines.next().unwrap_or_default().to_string();
    let setup_code = lines.next().unwrap_or_default().trim().to_string();
    Ok(Some(WifiProvisionRequest {
        ssid: ssid.to_string(),
        password,
        setup_code,
    }))
}

fn prompt_manual_wifi_provision(
    ssid_message: &str,
    default_ssid: &str,
) -> Result<Option<WifiProvisionRequest>> {
    let script = format!(
        "set ssidResult to text returned of (display dialog {ssid_message} with title {title} default answer {default_ssid})\nset passResult to text returned of (display dialog {password_message} with title {title} default answer \"\" with hidden answer)\nset codeResult to text returned of (display dialog {code_message} with title {title} default answer \"\")\nreturn ssidResult & linefeed & passResult & linefeed & codeResult",
        ssid_message = applescript_string(ssid_message),
        password_message = applescript_string("Wi-Fi password:"),
        code_message = applescript_string(
            "Put the Stick in setup mode, then enter the large 8-digit code shown on its screen:",
        ),
        title = applescript_string("m5mic Bluetooth Wi-Fi Setup"),
        default_ssid = applescript_string(default_ssid),
    );
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .context("run manual Bluetooth Wi-Fi setup prompt")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("User canceled") || stderr.contains("-128") {
            return Ok(None);
        }
        let message = stderr.trim();
        if message.is_empty() {
            anyhow::bail!("osascript exited with {}", output.status);
        }
        anyhow::bail!("{message}");
    }

    let stdout = String::from_utf8(output.stdout).context("decode manual Wi-Fi setup output")?;
    let mut lines = stdout.trim_end_matches('\n').splitn(3, '\n');
    let ssid = lines.next().unwrap_or_default().trim().to_string();
    let password = lines.next().unwrap_or_default().to_string();
    let setup_code = lines.next().unwrap_or_default().trim().to_string();
    if ssid.is_empty() {
        anyhow::bail!("Wi-Fi name is required");
    }
    Ok(Some(WifiProvisionRequest {
        ssid,
        password,
        setup_code,
    }))
}

fn current_wifi_network() -> Option<WifiNetwork> {
    current_wifi_network_corewlan()
        .or_else(|| current_wifi_ssid_system_profiler().map(WifiNetwork::named))
        .or_else(|| current_wifi_ssid_networksetup().map(WifiNetwork::named))
}

fn current_wifi_network_corewlan() -> Option<WifiNetwork> {
    let client = unsafe { objc2_core_wlan::CWWiFiClient::sharedWiFiClient() };
    let interface = unsafe { client.interface()? };
    let ssid = unsafe { interface.ssid()? };
    let ssid = clean_wifi_ssid(&ssid.to_string())?;
    let ssid_bytes = unsafe { interface.ssidData() }.map(|data| data.to_vec());
    Some(WifiNetwork { ssid, ssid_bytes })
}

fn current_wifi_ssid_system_profiler() -> Option<String> {
    let output = Command::new("/usr/sbin/system_profiler")
        .args(["SPAirPortDataType", "-json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value: Value = serde_json::from_slice(&output.stdout).ok()?;
    let ssid = system_profiler_current_ssid(&value)?;
    clean_wifi_ssid(ssid)
}

fn system_profiler_current_ssid(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) => {
            if let Some(current_network) = map
                .get("spairport_current_network_information")
                .and_then(Value::as_object)
            {
                if let Some(ssid) = current_network.get("_name").and_then(Value::as_str) {
                    return Some(ssid);
                }
            }

            map.values().find_map(system_profiler_current_ssid)
        }
        Value::Array(values) => values.iter().find_map(system_profiler_current_ssid),
        _ => None,
    }
}

fn current_wifi_ssid_networksetup() -> Option<String> {
    let device = wifi_hardware_device().unwrap_or_else(|| "en0".to_string());
    let output = Command::new("networksetup")
        .args(["-getairportnetwork", &device])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let ssid = stdout.split_once(": ")?.1.trim();
    clean_wifi_ssid(ssid)
}

fn clean_wifi_ssid(ssid: &str) -> Option<String> {
    let ssid = ssid.trim();
    if ssid.is_empty()
        || ssid.eq_ignore_ascii_case("You are not associated with an AirPort network.")
    {
        return None;
    }

    Some(ssid.to_string())
}

fn current_keychain_wifi_password(network: &WifiNetwork) -> Option<String> {
    if network.ssid.is_empty() {
        return None;
    }

    let _interaction_lock = SecKeychain::disable_user_interaction().ok();
    current_corewlan_wifi_password(network.ssid_bytes.as_deref())
        .or_else(|| current_user_keychain_wifi_password(&network.ssid))
}

fn current_corewlan_wifi_password(ssid_bytes: Option<&[u8]>) -> Option<String> {
    let ssid_data = NSData::with_bytes(ssid_bytes?);

    let mut password_ptr: *mut NSString = ptr::null_mut();
    let status = unsafe {
        CWKeychainFindWiFiPassword(CWKeychainDomain::User, &ssid_data, &mut password_ptr)
    };
    if status == 0 && !password_ptr.is_null() {
        let password: Retained<NSString> = unsafe { Retained::from_raw(password_ptr)? };
        return Some(password.to_string());
    }

    tracing::debug!(
        status,
        "CoreWLAN user Wi-Fi password lookup did not return a password"
    );

    None
}

fn current_user_keychain_wifi_password(ssid: &str) -> Option<String> {
    let keychain = user_login_keychain()?;
    let mut search = ItemSearchOptions::new();
    search
        .class(ItemClass::generic_password())
        .service("AirPort")
        .account(ssid)
        .keychains(&[keychain])
        .load_data(true)
        .limit(1);

    search
        .search()
        .ok()?
        .into_iter()
        .find_map(|result| match result {
            SearchResult::Data(bytes) => password_from_bytes(bytes),
            _ => None,
        })
}

fn user_login_keychain() -> Option<SecKeychain> {
    let home = env::var_os("HOME")?;
    for name in ["login.keychain-db", "login.keychain"] {
        let path = PathBuf::from(&home)
            .join("Library")
            .join("Keychains")
            .join(name);
        if path.exists() {
            if let Ok(keychain) = SecKeychain::open(&path) {
                return Some(keychain);
            }
        }
    }

    None
}

fn password_from_bytes(bytes: Vec<u8>) -> Option<String> {
    String::from_utf8(bytes)
        .ok()
        .map(|password| password.trim_end_matches(['\r', '\n']).to_string())
}

fn wifi_hardware_device() -> Option<String> {
    let output = Command::new("networksetup")
        .arg("-listallhardwareports")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut in_wifi_section = false;
    for line in stdout.lines() {
        if let Some(port) = line.strip_prefix("Hardware Port: ") {
            in_wifi_section = port == "Wi-Fi" || port == "AirPort";
            continue;
        }
        if in_wifi_section {
            if let Some(device) = line.strip_prefix("Device: ") {
                return Some(device.trim().to_string());
            }
        }
    }

    None
}

fn run_driver_install_prompt() -> Result<bool> {
    let source = bundled_driver_path().context("bundled m5mic.driver was not found")?;
    let destination = installed_driver_path();
    let driver_dir = destination
        .parent()
        .ok_or_else(|| anyhow!("invalid driver install path"))?;
    let install_command = format!(
        "/bin/mkdir -p {driver_dir} && /bin/rm -rf {destination} && /bin/cp -R {source} {destination} && /usr/sbin/chown -R root:wheel {destination} && (/usr/bin/killall coreaudiod >/dev/null 2>&1 || true)",
        driver_dir = shell_quote(driver_dir),
        destination = shell_quote(&destination),
        source = shell_quote(&source),
    );
    let script = format!(
        "set buttonChoice to button returned of (display dialog {message} with title {title} buttons {{\"Later\", \"Install\"}} default button \"Install\")\nif buttonChoice is \"Install\" then\n    do shell script {command} with administrator privileges\nend if",
        message = applescript_string("m5mic needs to install its CoreAudio driver before the virtual microphone can appear in Sound Settings."),
        title = applescript_string("m5mic"),
        command = applescript_string(&install_command),
    );

    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .context("run driver install prompt")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            anyhow::bail!("osascript exited with {}", output.status);
        }
        anyhow::bail!("{message}");
    }

    Ok(installed_driver_path().exists())
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn sync_usb_mode_item(
    menu: &Menu,
    usb_mode_item: &MenuItem,
    status: UsbStatus,
    usb_mode_visible: &mut bool,
) {
    let should_show = matches!(status, UsbStatus::Connected);
    if should_show == *usb_mode_visible {
        return;
    }

    let result = if should_show {
        menu.insert(usb_mode_item, 8)
    } else {
        menu.remove(usb_mode_item)
    };

    match result {
        Ok(()) => *usb_mode_visible = should_show,
        Err(err) => tracing::warn!(?err, "failed to update USB mode menu item"),
    }
}

fn set_menu_input_mode(mode: InputMode, status_item: &MenuItem) {
    match switch_input_mode(mode) {
        Ok(()) => status_item.set_text(format!("Status: {} mode selected", mode.menu_label())),
        Err(err) => {
            tracing::warn!(?err, ?mode, "failed to switch input mode");
            status_item.set_text(format!("Status: {} mode failed", mode.menu_label()));
        }
    }
}

fn set_menu_recording_command(command: RecordingCommand, status_item: &MenuItem) {
    match send_recording_command(command) {
        Ok(()) => status_item.set_text(format!("Status: {} sent", command.menu_label())),
        Err(err) => {
            tracing::warn!(?err, ?command, "failed to send recording command");
            status_item.set_text(format!("Status: {} failed", command.menu_label()));
        }
    }
}

fn switch_input_mode(mode: InputMode) -> Result<()> {
    let input_device = find_m5mic_input(mode)?;
    set_default_input_device(input_device).context("set default input device")?;
    send_device_mode(mode).context("send device mode")?;
    Ok(())
}

fn find_m5mic_input(mode: InputMode) -> Result<AudioObjectID> {
    let target_transport = match mode {
        InputMode::Wifi | InputMode::Bluetooth => kAudioDeviceTransportTypeVirtual,
        InputMode::Usb => kAudioDeviceTransportTypeUSB,
    };

    for device_id in audio_devices().context("read CoreAudio devices")? {
        let name = audio_object_string(device_id, kAudioObjectPropertyName).unwrap_or_default();
        let transport = audio_object_u32(device_id, kAudioDevicePropertyTransportType)
            .unwrap_or(kAudioDeviceTransportTypeUnknown);

        if transport == target_transport && name.to_ascii_lowercase().contains("m5mic") {
            return Ok(device_id);
        }
    }

    anyhow::bail!("m5mic {} input is not available", mode.menu_label())
}

fn set_default_input_device(device_id: AudioObjectID) -> Result<()> {
    let address = audio_property_address(kAudioHardwarePropertyDefaultInputDevice);
    let mut device_id = device_id;
    let status = unsafe {
        AudioObjectSetPropertyData(
            kAudioObjectSystemObject,
            &address,
            0,
            ptr::null(),
            size_of::<AudioObjectID>() as UInt32,
            (&mut device_id as *mut AudioObjectID).cast(),
        )
    };
    ensure_audio_status(status, "set default input")
}

fn send_device_mode(mode: InputMode) -> Result<()> {
    let payload = match mode {
        InputMode::Wifi => CONTROL_MODE_WIFI,
        InputMode::Bluetooth => CONTROL_MODE_BLE,
        InputMode::Usb => CONTROL_MODE_USB,
    };
    send_control_payload(payload, "mode")
}

fn send_recording_command(command: RecordingCommand) -> Result<()> {
    send_control_payload(recording_command_payload(command), "recording")
}

fn send_control_payload(payload: &'static [u8], label: &str) -> Result<()> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .context("bind control socket")?;
    socket.set_broadcast(true).context("enable broadcast")?;

    let targets = mode_control_targets();
    for _ in 0..8 {
        for target in &targets {
            socket
                .send_to(payload, target)
                .with_context(|| format!("send {label} control packet to {target}"))?;
        }
        thread::sleep(Duration::from_millis(75));
    }

    Ok(())
}

fn spawn_ble_mode_command(runtime: &Handle, mode: InputMode) {
    let payload = match mode {
        InputMode::Wifi => CONTROL_MODE_WIFI,
        InputMode::Bluetooth => CONTROL_MODE_BLE,
        InputMode::Usb => CONTROL_MODE_USB,
    };
    runtime.spawn(async move {
        if let Err(err) = ble::send_control_command(payload).await {
            tracing::debug!(?err, ?mode, "Bluetooth mode command failed");
        }
    });
}

fn spawn_ble_recording_command(runtime: &Handle, command: RecordingCommand) {
    let payload = recording_command_payload(command);
    runtime.spawn(async move {
        if let Err(err) = ble::send_control_command(payload).await {
            tracing::debug!(?err, ?command, "Bluetooth recording command failed");
        }
    });
}

const fn recording_command_payload(command: RecordingCommand) -> &'static [u8] {
    match command {
        RecordingCommand::Start => CONTROL_RECORD_START,
        RecordingCommand::Stop => CONTROL_RECORD_STOP,
    }
}

fn mode_control_targets() -> Vec<SocketAddrV4> {
    let mut targets = BTreeSet::from([SocketAddrV4::new(Ipv4Addr::BROADCAST, CONTROL_PORT)]);

    match get_if_addrs() {
        Ok(addrs) => {
            for iface in addrs {
                if iface.is_loopback() {
                    continue;
                }
                let IfAddr::V4(addr) = iface.addr else {
                    continue;
                };

                targets.insert(SocketAddrV4::new(
                    ipv4_broadcast(addr.ip, addr.netmask),
                    CONTROL_PORT,
                ));
                if let Some(broadcast) = addr.broadcast {
                    targets.insert(SocketAddrV4::new(broadcast, CONTROL_PORT));
                }
            }
        }
        Err(err) => tracing::debug!(?err, "failed to enumerate network interfaces"),
    }

    targets.into_iter().collect()
}

fn ipv4_broadcast(ip: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    let ip = u32::from(ip);
    let mask = u32::from(netmask);
    Ipv4Addr::from(ip | !mask)
}

fn usb_status() -> UsbStatus {
    match usb_m5mic_connected() {
        Ok(true) => UsbStatus::Connected,
        Ok(false) => UsbStatus::Disconnected,
        Err(err) => {
            tracing::debug!(?err, "failed to query USB audio devices");
            UsbStatus::Unknown
        }
    }
}

fn usb_m5mic_connected() -> Result<bool> {
    let devices = audio_devices().context("read CoreAudio devices")?;
    for device_id in devices {
        let name = audio_object_string(device_id, kAudioObjectPropertyName).unwrap_or_default();
        let transport = audio_object_u32(device_id, kAudioDevicePropertyTransportType)
            .unwrap_or(kAudioDeviceTransportTypeUnknown);

        if transport == kAudioDeviceTransportTypeUSB && name.to_ascii_lowercase().contains("m5mic")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn audio_devices() -> Result<Vec<AudioObjectID>> {
    let address = audio_property_address(kAudioHardwarePropertyDevices);
    let mut data_size: UInt32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject,
            &address,
            0,
            ptr::null(),
            &mut data_size,
        )
    };
    ensure_audio_status(status, "get device list size")?;

    let count = data_size as usize / size_of::<AudioObjectID>();
    let mut devices = vec![0; count];
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &address,
            0,
            ptr::null(),
            &mut data_size,
            devices.as_mut_ptr().cast(),
        )
    };
    ensure_audio_status(status, "get device list")?;
    Ok(devices)
}

fn audio_object_string(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Result<String> {
    let address = audio_property_address(selector);
    let mut string_ref: CFStringRef = ptr::null();
    let mut data_size = size_of::<CFStringRef>() as UInt32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            &address,
            0,
            ptr::null(),
            &mut data_size,
            (&mut string_ref as *mut CFStringRef).cast(),
        )
    };
    ensure_audio_status(status, "get string property")?;

    if string_ref.is_null() {
        return Ok(String::new());
    }

    let mut buffer = [0i8; 256];
    let ok = unsafe {
        CFStringGetCString(
            string_ref,
            buffer.as_mut_ptr(),
            buffer.len() as CFIndex,
            kCFStringEncodingUTF8,
        )
    };
    unsafe {
        CFRelease(string_ref.cast());
    }

    if ok == 0 {
        return Ok(String::new());
    }

    let string = unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(string)
}

fn audio_object_u32(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Result<UInt32> {
    let address = audio_property_address(selector);
    let mut value: UInt32 = 0;
    let mut data_size = size_of::<UInt32>() as UInt32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            &address,
            0,
            ptr::null(),
            &mut data_size,
            (&mut value as *mut UInt32).cast(),
        )
    };
    ensure_audio_status(status, "get u32 property")?;
    Ok(value)
}

fn audio_property_address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn ensure_audio_status(status: OSStatus, context: &str) -> Result<()> {
    if status == kAudioHardwareNoError as OSStatus {
        Ok(())
    } else {
        anyhow::bail!("{context}: CoreAudio status {status}")
    }
}

fn open_sound_settings() {
    let _ = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.Sound-Settings.extension")
        .status();
}
