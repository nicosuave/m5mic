use std::{path::PathBuf, process::Command};

use anyhow::{Context, Result};
use clap::Parser;
use m5mic_protocol::{DISCOVERY_PORT, WS_PORT};
use m5mic_receiver::{run, ReceiverConfig, ReceiverStatus};
use tokio::{runtime::Runtime, sync::watch};
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

    let status_item = MenuItem::new("Status: starting", false, None);
    let settings_item = MenuItem::with_id("sound-settings", "Open Sound Settings", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit m5mic", true, None);
    let separator = PredefinedMenuItem::separator();
    let menu = Menu::with_items(&[&status_item, &separator, &settings_item, &quit_item])
        .context("create tray menu")?;

    let tray = TrayIconBuilder::new()
        .with_tooltip("m5mic")
        .with_title("m5mic")
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
        Event::UserEvent(UserEvent::Menu(event)) => {
            if event.id == MenuId::from("sound-settings") {
                open_sound_settings();
            } else if event.id == MenuId::from("quit") {
                event_loop.exit();
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
    let color = match status {
        ReceiverStatus::Receiving { .. } => [0x23, 0xd1, 0x8b, 0xff],
        ReceiverStatus::Connected => [0x26, 0xb9, 0xf4, 0xff],
        ReceiverStatus::Error(_) => [0xff, 0x45, 0x45, 0xff],
        ReceiverStatus::Starting | ReceiverStatus::Waiting | ReceiverStatus::Stopped => {
            [0xc7, 0xd2, 0xfe, 0xff]
        }
    };
    make_icon(color)
}

fn fallback_icon() -> Icon {
    make_icon([0xc7, 0xd2, 0xfe, 0xff]).expect("fallback icon")
}

fn make_icon(color: [u8; 4]) -> Result<Icon> {
    const SIZE: u32 = 18;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let pixel = ((y * SIZE + x) * 4) as usize;
            if distance <= 7.0 {
                rgba[pixel..pixel + 4].copy_from_slice(&color);
            } else {
                rgba[pixel + 3] = 0;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("build tray icon")
}

fn open_sound_settings() {
    let _ = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.Sound-Settings.extension")
        .status();
}
