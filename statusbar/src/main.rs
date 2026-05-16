use std::{
    ffi::CStr,
    mem::size_of,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    path::PathBuf,
    process::Command,
    ptr, thread,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use coreaudio_sys::*;
use m5mic_protocol::{
    CONTROL_MODE_USB, CONTROL_MODE_WIRELESS, CONTROL_PORT, DISCOVERY_PORT, WS_PORT,
};
use m5mic_receiver::{run, ReceiverConfig, ReceiverStatus};
use tokio::{runtime::Runtime, sync::watch, time};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};
use winit::{
    event::Event,
    event_loop::{ControlFlow, EventLoop},
};

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
    Usb(UsbStatus),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UsbStatus {
    Connected,
    Disconnected,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputMode {
    Wireless,
    Usb,
}

impl InputMode {
    const fn menu_label(self) -> &'static str {
        match self {
            Self::Wireless => "wireless",
            Self::Usb => "USB",
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
    let (status_tx, status_rx) = watch::channel(ReceiverStatus::Starting);

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

    let initial_usb_status = usb_status();
    let status_item = MenuItem::new("Status: starting", false, None);
    let usb_status_item = MenuItem::new("USB: checking", false, None);
    let wireless_mode_item = MenuItem::with_id("mode-wireless", "Use Wireless Mode", true, None);
    let usb_mode_item = MenuItem::with_id("mode-usb", "Use USB Mode", true, None);
    let settings_item = MenuItem::with_id("sound-settings", "Open Sound Settings", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit m5mic", true, None);
    let separator = PredefinedMenuItem::separator();
    let menu = Menu::new();
    menu.append(&status_item).context("add status menu item")?;
    menu.append(&usb_status_item)
        .context("add USB status menu item")?;
    menu.append(&wireless_mode_item)
        .context("add wireless mode menu item")?;

    let mut usb_mode_visible = matches!(initial_usb_status, UsbStatus::Connected);
    if usb_mode_visible {
        menu.append(&usb_mode_item)
            .context("add USB mode menu item")?;
    }
    menu.append(&separator).context("add menu separator")?;
    menu.append(&settings_item)
        .context("add sound settings menu item")?;
    menu.append(&quit_item).context("add quit menu item")?;
    let menu_handle = menu.clone();

    let tray = TrayIconBuilder::new()
        .with_tooltip("m5mic")
        .with_icon(icon_for_status(&ReceiverStatus::Starting)?)
        .with_menu(Box::new(menu))
        .build()
        .context("create tray icon")?;

    #[allow(deprecated)]
    let run_result = event_loop.run(move |event, event_loop| match event {
        Event::UserEvent(UserEvent::Status(status)) => {
            let text = status_text(&status);
            status_item.set_text(format!("Status: {text}"));
            let _ = tray.set_tooltip(Some(format!("m5mic: {text}")));
            let _ = tray.set_icon(Some(
                icon_for_status(&status).unwrap_or_else(|_| fallback_icon()),
            ));
        }
        Event::UserEvent(UserEvent::Usb(status)) => {
            usb_status_item.set_text(format!("USB: {}", usb_status_text(status)));
            sync_usb_mode_item(&menu_handle, &usb_mode_item, status, &mut usb_mode_visible);
        }
        Event::UserEvent(UserEvent::Menu(event)) => {
            if event.id == MenuId::from("sound-settings") {
                open_sound_settings();
            } else if event.id == MenuId::from("quit") {
                event_loop.exit();
            } else if event.id == MenuId::from("mode-wireless") {
                set_menu_input_mode(InputMode::Wireless, &status_item);
            } else if event.id == MenuId::from("mode-usb") {
                set_menu_input_mode(InputMode::Usb, &status_item);
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
        menu.insert(usb_mode_item, 3)
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

fn switch_input_mode(mode: InputMode) -> Result<()> {
    let input_device = find_m5mic_input(mode)?;
    set_default_input_device(input_device).context("set default input device")?;
    send_device_mode(mode).context("send device mode")?;
    Ok(())
}

fn find_m5mic_input(mode: InputMode) -> Result<AudioObjectID> {
    let target_transport = match mode {
        InputMode::Wireless => kAudioDeviceTransportTypeVirtual,
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
        InputMode::Wireless => CONTROL_MODE_WIRELESS,
        InputMode::Usb => CONTROL_MODE_USB,
    };
    let target = SocketAddrV4::new(Ipv4Addr::BROADCAST, CONTROL_PORT);
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .context("bind mode control socket")?;
    socket.set_broadcast(true).context("enable broadcast")?;

    for _ in 0..3 {
        socket
            .send_to(payload, target)
            .context("send mode control packet")?;
        thread::sleep(Duration::from_millis(25));
    }

    Ok(())
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
