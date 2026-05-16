use std::{
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
use esp_idf_hal::delay::FreeRtos;
use esp_idf_svc::{
    http::server::{Configuration as HttpServerConfiguration, EspHttpServer},
    netif::{EspNetif, NetifConfiguration, NetifStack},
    wifi::{BlockingWifi, EspWifi},
};
use log::{info, warn};

use crate::{
    display::StickDisplay,
    wifi_config::{WifiCredentials, WifiStore},
};

const AP_CHANNEL: u8 = 6;
const AP_IP: [u8; 4] = [192, 168, 71, 1];
const MAX_FORM_BYTES: usize = 256;
const HTTP_STACK_SIZE: usize = 8192;

const INDEX_HTML: &str = r#"<!doctype html>
<html>
<head>
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>M5 Mic Setup</title>
<style>
html{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#07101d;color:#edf4ff}
body{margin:0;min-height:100vh;display:grid;place-items:center}
main{width:min(390px,calc(100vw - 32px));padding:26px 20px 22px;border:1px solid #263856;background:#0d1728;border-radius:16px}
h1{font-size:28px;margin:0 0 8px}
p{color:#9fb0c7;margin:0 0 22px;line-height:1.4}
label{display:block;font-size:13px;color:#9fb0c7;margin:14px 0 6px}
input{box-sizing:border-box;width:100%;font:inherit;color:#edf4ff;background:#07101d;border:1px solid #334765;border-radius:10px;padding:13px}
button{width:100%;margin-top:20px;border:0;border-radius:10px;padding:14px;font:700 16px system-ui;background:#ef2e46;color:white}
.hint{font-size:12px;margin-top:16px}
</style>
</head>
<body>
<main>
<h1>M5 Mic Setup</h1>
<p>Connect this StickS3 to Wi-Fi. It will reboot back into mic mode after saving.</p>
<form method="post" action="/save">
<label>Wi-Fi name</label>
<input name="ssid" maxlength="32" autocomplete="off" autocapitalize="none" required>
<label>Password</label>
<input name="password" maxlength="64" type="password" autocomplete="current-password">
<button type="submit">Save Wi-Fi</button>
</form>
<p class="hint">Open http://192.168.71.1 if the page does not appear automatically.</p>
</main>
</body>
</html>
"#;

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
) -> Result<()> {
    display
        .show_setup_portal(ssid)
        .context("draw setup portal screen")?;
    start_ap(wifi, ssid).context("start setup AP")?;
    spawn_dns_server();
    let saved = Arc::new(AtomicBool::new(false));
    let _server = start_http_server(store, saved.clone()).context("start setup http server")?;

    info!("setup portal listening on http://192.168.71.1 with SSID {ssid}");
    loop {
        if saved.load(Ordering::SeqCst) {
            display
                .show_setup_saved()
                .context("draw setup saved screen")?;
            FreeRtos::delay_ms(1500);
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

fn start_http_server(store: WifiStore, saved: Arc<AtomicBool>) -> Result<EspHttpServer<'static>> {
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
        server.fn_handler::<anyhow::Error, _>(path, Method::Get, |req| {
            let mut resp = req.into_response(
                200,
                Some("OK"),
                &[("Content-Type", "text/html; charset=utf-8")],
            )?;
            resp.write_all(INDEX_HTML.as_bytes())?;
            Ok(())
        })?;
    }

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
        let credentials = parse_credentials(body).context("parse setup form")?;
        store.save(&credentials).context("save setup wifi")?;
        saved.store(true, Ordering::SeqCst);

        let mut resp = req.into_response(
            200,
            Some("OK"),
            &[("Content-Type", "text/html; charset=utf-8")],
        )?;
        resp.write_all(SAVED_HTML.as_bytes())?;
        Ok(())
    })?;

    Ok(server)
}

fn parse_credentials(body: &str) -> Result<WifiCredentials> {
    let ssid = form_value(body, "ssid").ok_or_else(|| anyhow!("ssid missing"))?;
    let password = form_value(body, "password").unwrap_or_default();
    Ok(WifiCredentials { ssid, password })
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
