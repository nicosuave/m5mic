use std::{
    fmt::Write as FmtWrite,
    net::UdpSocket,
    str,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};

use anyhow::{anyhow, Context, Result};
use embedded_svc::{
    http::{Headers, Method},
    io::{Read, Write},
    ipv4,
    wifi::{AccessPointConfiguration, AuthMethod, Configuration},
};
use esp_idf_hal::{
    delay::FreeRtos,
    gpio::{Input, PinDriver},
};
use esp_idf_svc::{
    http::server::{Configuration as HttpServerConfiguration, EspHttpServer},
    netif::{EspNetif, NetifConfiguration, NetifStack},
    wifi::{BlockingWifi, EspWifi},
};
use log::{info, warn};

use crate::{
    ble::BleAudioServer,
    display::StickDisplay,
    wifi_config::{AppSettings, BatteryBrightness, WifiCredentials, WifiStore, WirelessCodec},
};

const AP_CHANNEL: u8 = 6;
const AP_IP: [u8; 4] = [192, 168, 71, 1];
const MAX_FORM_BYTES: usize = 2048;
const HTTP_STACK_SIZE: usize = 8192;

const SAVED_HTML: &str = r#"<!doctype html>
<html>
<head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Saved</title></head>
<body style="font-family:system-ui;background:#07101d;color:#edf4ff;display:grid;place-items:center;min-height:100vh;margin:0">
<main style="width:min(360px,calc(100vw - 32px));text-align:center">
<h1>Saved</h1>
<p>The StickS3 is rebooting into mic mode.</p>
</main>
</body>
</html>
"#;

const REBOOT_HTML: &str = r#"<!doctype html>
<html>
<head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Rebooting</title></head>
<body style="font-family:system-ui;background:#07101d;color:#edf4ff;display:grid;place-items:center;min-height:100vh;margin:0">
<main style="width:min(360px,calc(100vw - 32px));text-align:center">
<h1>Rebooting</h1>
<p>The StickS3 is returning to mic mode.</p>
</main>
</body>
</html>
"#;

pub fn ap_netif_configuration() -> NetifConfiguration {
    let mut config = NetifStack::Ap.default_configuration();
    config.ip_configuration = Some(ipv4::Configuration::Router(ipv4::RouterConfiguration {
        subnet: ipv4::Subnet {
            gateway: portal_ip(),
            mask: ipv4::Mask(24),
        },
        dhcp_enabled: true,
        dns: Some(portal_ip()),
        secondary_dns: None,
    }));
    config
}

pub fn create_ap_netif() -> Result<EspNetif> {
    EspNetif::new_with_conf(&ap_netif_configuration()).context("create setup AP netif")
}

pub fn ap_ssid(wifi: &BlockingWifi<EspWifi<'static>>) -> String {
    match wifi.wifi().ap_netif().get_mac() {
        Ok(mac) => format!("M5Mic-{:02X}{:02X}", mac[4], mac[5]),
        Err(err) => {
            warn!("failed to read AP mac for setup SSID: {err}");
            "M5Mic-Setup".to_string()
        }
    }
}

pub fn run(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    store: WifiStore,
    display: &mut StickDisplay<'_>,
    ssid: &str,
    button_b: &PinDriver<Input>,
    ble_audio: Option<&BleAudioServer>,
) -> Result<()> {
    display
        .show_setup_portal(ssid, ble_audio.map(BleAudioServer::provision_code))
        .context("draw setup portal screen")?;
    start_ap(wifi, ssid).context("start setup AP")?;
    spawn_dns_server();
    let saved = Arc::new(AtomicBool::new(false));
    let reboot = Arc::new(AtomicBool::new(false));
    let _server = start_http_server(store.clone(), saved.clone(), reboot.clone())
        .context("start setup http server")?;
    wait_for_button_release(button_b);

    info!("setup portal listening on http://192.168.71.1 with SSID {ssid}");
    loop {
        if consume_button_click(button_b) {
            reboot.store(true, Ordering::SeqCst);
        }
        if let Some(credentials) = ble_audio.and_then(BleAudioServer::take_provisioned_wifi) {
            info!(
                "Bluetooth Wi-Fi provisioning received in setup for SSID {}",
                credentials.ssid
            );
            store
                .save(&WifiCredentials {
                    ssid: credentials.ssid,
                    password: credentials.password,
                })
                .context("save Bluetooth-provisioned Wi-Fi")?;
            saved.store(true, Ordering::SeqCst);
        }
        if saved.load(Ordering::SeqCst) {
            display
                .show_setup_saved()
                .context("draw setup saved screen")?;
            FreeRtos::delay_ms(1500);
            unsafe { esp_idf_sys::esp_restart() };
        }
        if reboot.load(Ordering::SeqCst) {
            display
                .show_setup_rebooting()
                .context("draw setup reboot screen")?;
            FreeRtos::delay_ms(800);
            unsafe { esp_idf_sys::esp_restart() };
        }
        FreeRtos::delay_ms(100);
    }
}

fn portal_ip() -> ipv4::Ipv4Addr {
    ipv4::Ipv4Addr::new(AP_IP[0], AP_IP[1], AP_IP[2], AP_IP[3])
}

fn start_ap(wifi: &mut BlockingWifi<EspWifi<'static>>, ssid: &str) -> Result<()> {
    let _ = wifi.disconnect();
    let _ = wifi.stop();

    let wifi_configuration = Configuration::AccessPoint(AccessPointConfiguration {
        ssid: ssid
            .try_into()
            .map_err(|_| anyhow!("setup ssid too long"))?,
        ssid_hidden: false,
        auth_method: AuthMethod::None,
        password: Default::default(),
        channel: AP_CHANNEL,
        max_connections: 4,
        ..Default::default()
    });

    wifi.set_configuration(&wifi_configuration)
        .context("set setup AP wifi config")?;
    wifi.start().context("start setup AP wifi")?;
    wifi.wait_netif_up().context("wait for setup AP netif")?;
    Ok(())
}

fn start_http_server(
    store: WifiStore,
    saved: Arc<AtomicBool>,
    reboot: Arc<AtomicBool>,
) -> Result<EspHttpServer<'static>> {
    let mut server = EspHttpServer::new(&HttpServerConfiguration {
        stack_size: HTTP_STACK_SIZE,
        ..Default::default()
    })
    .context("create setup http server")?;

    for path in [
        "/",
        "/generate_204",
        "/hotspot-detect.html",
        "/connecttest.txt",
        "/ncsi.txt",
        "/fwlink",
    ] {
        let store = store.clone();
        server.fn_handler::<anyhow::Error, _>(path, Method::Get, move |req| {
            let html = settings_html(&store);
            let mut resp = req.into_response(
                200,
                Some("OK"),
                &[("Content-Type", "text/html; charset=utf-8")],
            )?;
            resp.write_all(html.as_bytes())?;
            Ok(())
        })?;
    }

    let wifi_store = store.clone();
    server.fn_handler::<anyhow::Error, _>("/save", Method::Post, move |mut req| {
        let len = req.content_len().unwrap_or(0) as usize;
        if len > MAX_FORM_BYTES {
            req.into_status_response(413)?
                .write_all(b"Request too large")?;
            return Ok(());
        }

        let mut body = [0u8; MAX_FORM_BYTES];
        req.read_exact(&mut body[..len])?;
        let body = str::from_utf8(&body[..len]).context("decode setup form")?;
        let settings = parse_settings(body);
        wifi_store
            .save_settings(settings)
            .context("save setup settings")?;

        let mut credentials = parse_credentials(body);
        if !credentials.ssid.is_empty() {
            if credentials.password.is_empty() {
                if let Some(existing) = wifi_store.load().context("load saved wifi config")? {
                    if existing.ssid == credentials.ssid {
                        credentials.password = existing.password;
                    }
                }
            }
            wifi_store.save(&credentials).context("save setup wifi")?;
        }
        saved.store(true, Ordering::SeqCst);

        let mut resp = req.into_response(
            200,
            Some("OK"),
            &[("Content-Type", "text/html; charset=utf-8")],
        )?;
        resp.write_all(SAVED_HTML.as_bytes())?;
        Ok(())
    })?;

    let settings_store = store.clone();
    let settings_reboot = reboot.clone();
    server.fn_handler::<anyhow::Error, _>("/settings", Method::Post, move |mut req| {
        let len = req.content_len().unwrap_or(0) as usize;
        if len > MAX_FORM_BYTES {
            req.into_status_response(413)?
                .write_all(b"Request too large")?;
            return Ok(());
        }

        let mut body = [0u8; MAX_FORM_BYTES];
        req.read_exact(&mut body[..len])?;
        let body = str::from_utf8(&body[..len]).context("decode settings form")?;
        let settings = parse_settings(body);
        settings_store
            .save_settings(settings)
            .context("save setup settings")?;
        settings_reboot.store(true, Ordering::SeqCst);

        let mut resp = req.into_response(
            200,
            Some("OK"),
            &[("Content-Type", "text/html; charset=utf-8")],
        )?;
        resp.write_all(REBOOT_HTML.as_bytes())?;
        Ok(())
    })?;

    server.fn_handler::<anyhow::Error, _>("/reboot", Method::Post, move |req| {
        reboot.store(true, Ordering::SeqCst);
        let mut resp = req.into_response(
            200,
            Some("OK"),
            &[("Content-Type", "text/html; charset=utf-8")],
        )?;
        resp.write_all(REBOOT_HTML.as_bytes())?;
        Ok(())
    })?;

    Ok(server)
}

fn settings_html(store: &WifiStore) -> String {
    let saved = store.load().ok().flatten();
    let settings = store.load_settings().unwrap_or_default();
    let mut html = String::with_capacity(4096);
    let (status, ssid_value, password_hint) = match saved {
        Some(credentials) => (
            format!("Saved: {}", escape_html(&credentials.ssid)),
            escape_html(&credentials.ssid),
            "Leave blank to keep the current password when the Wi-Fi name is unchanged.",
        ),
        None => (
            "Not configured".to_string(),
            String::new(),
            "Password is optional for open networks.",
        ),
    };
    let dim_checked = if settings.battery_brightness == BatteryBrightness::Dim {
        " checked"
    } else {
        ""
    };
    let full_checked = if settings.battery_brightness == BatteryBrightness::Full {
        " checked"
    } else {
        ""
    };
    let saver_checked = if settings.recording_battery_saver {
        " checked"
    } else {
        ""
    };
    let pcm_checked = if settings.wireless_codec == WirelessCodec::PcmS16Le {
        " checked"
    } else {
        ""
    };
    let adpcm_checked = if settings.wireless_codec == WirelessCodec::ImaAdpcm4 {
        " checked"
    } else {
        ""
    };
    let battery_mode = match settings.battery_brightness {
        BatteryBrightness::Dim => "dim",
        BatteryBrightness::Full => "full",
    };
    let saver_mode = if settings.recording_battery_saver {
        "on"
    } else {
        "off"
    };
    let codec_mode = settings.wireless_codec.label();
    let modes = "Wi-Fi BT USB";

    let _ = write!(
        html,
        r#"<!doctype html>
<html>
<head>
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>M5 Mic Settings</title>
<style>
*{{box-sizing:border-box}}
html{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#070b12;color:#eef4ff;-webkit-font-smoothing:antialiased}}
body{{margin:0;min-height:100vh;padding:18px;display:flex;justify-content:center;background:#070b12}}
main{{width:min(440px,100%);align-self:flex-start;padding:22px 20px 24px;border:1px solid #23314b;background:#0d1422;border-radius:18px;box-shadow:0 18px 60px rgba(0,0,0,.38)}}
button,input{{touch-action:manipulation}}
.top{{display:flex;align-items:flex-start;justify-content:space-between;gap:18px;margin-bottom:18px}}
.eyebrow{{margin:0 0 4px;color:#7f91ad;font-size:12px;font-weight:700;letter-spacing:.08em;text-transform:uppercase}}
h1{{font-size:32px;line-height:1;margin:0}}
h2{{font-size:13px;line-height:1;margin:24px 0 12px;color:#9fb0c7;font-weight:800;letter-spacing:.08em;text-transform:uppercase}}
p{{color:#9fb0c7;margin:0;line-height:1.45}}
.pill{{padding:7px 10px;border:1px solid #334765;border-radius:999px;color:#40c4ff;font-size:12px;font-weight:800;background:#0a111d}}
.status{{display:flex;align-items:center;justify-content:space-between;gap:14px;padding:12px 0;border-top:1px solid #23314b;border-bottom:1px solid #23314b;color:#9fb0c7;font-size:13px}}
.status strong{{color:#eef4ff;font-size:14px;text-align:right;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
.metrics{{display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:12px}}
.metric{{padding:13px 12px;border:1px solid #23314b;border-radius:12px;background:#09101b}}
.metric span{{display:block;color:#7f91ad;font-size:12px;margin-bottom:7px}}
.metric strong{{display:block;color:#eef4ff;font-size:20px;line-height:1}}
form{{margin:0;padding:0}}
.field{{margin-top:14px}}
label.label{{display:block;font-size:13px;color:#9fb0c7;margin:0 0 7px;font-weight:700}}
input[type=text],input[type=password]{{width:100%;font:inherit;color:#eef4ff;background:#070b12;border:1px solid #334765;border-radius:12px;padding:14px 13px;outline:none}}
input[type=text]:focus,input[type=password]:focus{{border-color:#40c4ff;box-shadow:0 0 0 3px rgba(64,196,255,.14)}}
.seg{{display:grid;grid-template-columns:1fr 1fr;padding:4px;border:1px solid #334765;border-radius:14px;background:#070b12;gap:4px}}
.seg input{{position:absolute;opacity:0;pointer-events:none}}
.seg span{{display:block;text-align:center;padding:11px 8px;border-radius:10px;color:#9fb0c7;font-weight:800}}
.seg input:checked+span{{background:#ef2e46;color:white}}
.seg input:focus-visible+span{{box-shadow:0 0 0 3px rgba(255,255,255,.22)}}
.switch{{display:flex;align-items:center;justify-content:space-between;gap:14px;margin-top:14px;padding:13px 0;border-top:1px solid #23314b;border-bottom:1px solid #23314b;color:#eef4ff;font-weight:700}}
.switch small{{display:block;margin-top:4px;color:#7f91ad;font-weight:500;line-height:1.35}}
.switch input{{position:absolute;opacity:0;pointer-events:none}}
.track{{width:48px;height:28px;border-radius:999px;background:#263856;position:relative;flex:0 0 auto;transition:background-color .15s ease}}
.track:before{{content:"";position:absolute;width:22px;height:22px;left:3px;top:3px;border-radius:50%;background:#9fb0c7;transition:transform .15s ease,background-color .15s ease}}
.switch input:checked+.track{{background:#1f9f62}}
.switch input:checked+.track:before{{transform:translateX(20px);background:white}}
.switch input:focus-visible+.track{{box-shadow:0 0 0 3px rgba(255,255,255,.22)}}
button{{width:100%;min-height:46px;margin-top:16px;border:0;border-radius:12px;padding:14px;font:800 15px system-ui;background:#ef2e46;color:white}}
button:active{{transform:scale(.985)}}
button.secondary{{margin-top:10px;background:#172338;color:#eef4ff;border:1px solid #334765}}
.hint{{font-size:12px;margin-top:9px;color:#7f91ad}}
</style>
</head>
<body>
<main>
<div class="top">
<div>
<p class="eyebrow">M5StickS3</p>
<h1>m5mic</h1>
</div>
<div class="pill">Setup</div>
</div>
<div class="status"><span>Network</span><strong>{status}</strong></div>
<h2>Modes</h2>
<div class="metrics">
<div class="metric"><span>BtnB cycle</span><strong>{modes}</strong></div>
<div class="metric"><span>Wi-Fi codec</span><strong>{codec_mode}</strong></div>
<div class="metric"><span>Battery screen</span><strong>{battery_mode}</strong></div>
<div class="metric"><span>Rec saver</span><strong>{saver_mode}</strong></div>
</div>
<form method="post" action="/save">
<div class="field">
<label class="label">Wi-Fi audio codec</label>
<div class="seg">
<label><input type="radio" name="wireless_codec" value="pcm_s16le"{pcm_checked}><span>PCM</span></label>
<label><input type="radio" name="wireless_codec" value="ima_adpcm4"{adpcm_checked}><span>ADPCM</span></label>
</div>
</div>
<div class="field">
<label class="label">Battery screen brightness</label>
<div class="seg">
<label><input type="radio" name="battery_brightness" value="dim"{dim_checked}><span>Dim</span></label>
<label><input type="radio" name="battery_brightness" value="full"{full_checked}><span>Full</span></label>
</div>
</div>
<label class="switch">
<span>Recording saver<small>Pause meters on battery. BtnB toggles screen off while recording.</small></span>
<input type="checkbox" name="recording_battery_saver" value="1"{saver_checked}><span class="track"></span>
</label>
<h2>Wi-Fi</h2>
<div class="field">
<label class="label">Wi-Fi name</label>
<input type="text" name="ssid" maxlength="32" autocomplete="off" autocapitalize="none" spellcheck="false" value="{ssid_value}">
</div>
<div class="field">
<label class="label">Password</label>
<input name="password" maxlength="64" type="password" autocomplete="current-password">
</div>
<p class="hint">{password_hint}</p>
<button type="submit">Save and Reboot</button>
</form>
<form method="post" action="/reboot">
<button class="secondary" type="submit">Reboot to Mic Mode</button>
</form>
<p class="hint">Open http://192.168.71.1 if the page does not appear automatically.</p>
</main>
</body>
</html>
"#
    );
    html
}

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn parse_credentials(body: &str) -> WifiCredentials {
    let ssid = form_value(body, "ssid")
        .unwrap_or_default()
        .trim()
        .to_string();
    let password = form_value(body, "password").unwrap_or_default();
    WifiCredentials { ssid, password }
}

fn parse_settings(body: &str) -> AppSettings {
    let battery_brightness = match form_value(body, "battery_brightness").as_deref() {
        Some("full") => BatteryBrightness::Full,
        _ => BatteryBrightness::Dim,
    };
    let recording_battery_saver = form_value(body, "recording_battery_saver").is_some();
    let wireless_codec = match form_value(body, "wireless_codec").as_deref() {
        Some("ima_adpcm4") => WirelessCodec::ImaAdpcm4,
        _ => WirelessCodec::PcmS16Le,
    };
    AppSettings {
        battery_brightness,
        recording_battery_saver,
        wireless_codec,
    }
}

fn form_value(body: &str, key: &str) -> Option<String> {
    for pair in body.split('&') {
        let (candidate, value) = pair.split_once('=').unwrap_or((pair, ""));
        if candidate == key {
            return Some(url_decode(value));
        }
    }
    None
}

fn wait_for_button_release(button: &PinDriver<Input>) {
    while button.is_low() {
        FreeRtos::delay_ms(20);
    }
    FreeRtos::delay_ms(30);
}

fn consume_button_click(button: &PinDriver<Input>) -> bool {
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

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::new();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                if let Some(decoded) = hex_byte(bytes[index + 1], bytes[index + 2]) {
                    out.push(decoded as char);
                    index += 3;
                } else {
                    out.push('%');
                    index += 1;
                }
            }
            byte => {
                out.push(byte as char);
                index += 1;
            }
        }
    }

    out
}

fn hex_byte(high: u8, low: u8) -> Option<u8> {
    Some(hex_nibble(high)? << 4 | hex_nibble(low)?)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn spawn_dns_server() {
    let result = thread::Builder::new()
        .name("setup-dns".to_string())
        .stack_size(4096)
        .spawn(|| {
            if let Err(err) = dns_server() {
                warn!("setup dns server stopped: {err:#}");
            }
        });

    if let Err(err) = result {
        warn!("failed to spawn setup dns thread: {err}");
    }
}

fn dns_server() -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:53").context("bind setup dns socket")?;
    let mut query = [0u8; 512];
    let mut response = [0u8; 576];

    loop {
        let (len, peer) = socket.recv_from(&mut query).context("receive dns query")?;
        let Some(response_len) = build_dns_response(&query[..len], &mut response) else {
            continue;
        };
        if let Err(err) = socket.send_to(&response[..response_len], peer) {
            warn!("setup dns response failed: {err}");
        }
    }
}

fn build_dns_response(query: &[u8], response: &mut [u8]) -> Option<usize> {
    if query.len() < 12 || response.len() < query.len() + 16 {
        return None;
    }

    let question_end = dns_question_end(query)?;
    response[..2].copy_from_slice(&query[..2]);
    response[2] = 0x81;
    response[3] = 0x80;
    response[4] = 0x00;
    response[5] = 0x01;
    response[6] = 0x00;
    response[7] = 0x01;
    response[8] = 0x00;
    response[9] = 0x00;
    response[10] = 0x00;
    response[11] = 0x00;
    response[12..question_end].copy_from_slice(&query[12..question_end]);

    let mut offset = question_end;
    let answer = [
        0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, AP_IP[0], AP_IP[1],
        AP_IP[2], AP_IP[3],
    ];
    response[offset..offset + answer.len()].copy_from_slice(&answer);
    offset += answer.len();
    Some(offset)
}

fn dns_question_end(query: &[u8]) -> Option<usize> {
    let mut offset = 12;
    loop {
        let len = *query.get(offset)? as usize;
        offset += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        offset = offset.checked_add(len)?;
        if offset >= query.len() {
            return None;
        }
    }

    offset.checked_add(4).filter(|end| *end <= query.len())
}
