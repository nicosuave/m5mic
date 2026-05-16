use std::{
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    str,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::mdns::{EspMdns, Interface, Protocol, QueryResult};
use m5mic_protocol::{
    discovery_response_url, DISCOVERY_PORT, DISCOVERY_REQUEST, MDNS_PROTO, MDNS_SERVICE, WS_PATH,
};

pub fn discover_server(mdns: &EspMdns, mut on_progress: impl FnMut(usize)) -> Result<String> {
    if let Some(url) = option_env!("M5MIC_SERVER_URL") {
        return Ok(url.to_string());
    }

    if let Ok(url) = discover_mdns(mdns, &mut on_progress) {
        return Ok(url);
    }

    discover_udp(on_progress)
}

fn discover_mdns(mdns: &EspMdns, on_progress: &mut impl FnMut(usize)) -> Result<String> {
    for step in 0..6 {
        on_progress(step);
        let mut results = [
            empty_query_result(),
            empty_query_result(),
            empty_query_result(),
            empty_query_result(),
        ];
        let count = mdns
            .query_ptr(
                MDNS_SERVICE,
                MDNS_PROTO,
                Duration::from_millis(500),
                results.len(),
                &mut results,
            )
            .context("query mDNS")?;

        for result in results.iter().take(count) {
            if result.port == 0 {
                continue;
            }

            let path = txt_value(result, "path").unwrap_or(WS_PATH);
            if let Some(addr) = result
                .addr
                .iter()
                .find(|addr| matches!(addr, embedded_svc::ipv4::IpAddr::V4(_)))
            {
                return Ok(format!("ws://{}:{}{}", addr, result.port, path));
            }
        }
    }

    Err(anyhow!("no mDNS receiver found"))
}

fn discover_udp(mut on_progress: impl FnMut(usize)) -> Result<String> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .context("bind udp discovery socket")?;
    socket.set_broadcast(true).context("enable udp broadcast")?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .context("set udp discovery timeout")?;

    let target = SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT);

    let mut buf = [0u8; 256];
    for step in 6..14 {
        on_progress(step);
        socket
            .send_to(DISCOVERY_REQUEST, target)
            .context("send udp discovery")?;

        match socket.recv_from(&mut buf) {
            Ok((len, _)) => {
                let text = str::from_utf8(&buf[..len]).context("decode udp discovery response")?;
                return discovery_response_url(text)
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("invalid udp discovery response"));
            }
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(err) => return Err(err).context("receive udp discovery"),
        }
    }

    Err(anyhow!("no udp receiver found"))
}

fn txt_value<'a>(result: &'a QueryResult, key: &str) -> Option<&'a str> {
    result
        .txt
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_str())
}

fn empty_query_result() -> QueryResult {
    QueryResult {
        instance_name: None,
        hostname: None,
        port: 0,
        txt: Vec::new(),
        addr: Vec::new(),
        interface: Interface::STA,
        ip_protocol: Protocol::V4,
    }
}
