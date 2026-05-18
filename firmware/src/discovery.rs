use std::{
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    str,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::mdns::{EspMdns, Interface, Protocol, QueryResult};
use m5mic_protocol::{
    discovery_response_priority, discovery_response_url, DISCOVERY_PORT, DISCOVERY_REQUEST,
    MDNS_PROTO, MDNS_SERVICE, RECEIVER_PRIORITY_LEGACY, RECEIVER_PRIORITY_PHONE, WS_PATH,
};

pub fn discover_server(mdns: &EspMdns, mut on_progress: impl FnMut(usize)) -> Result<String> {
    if let Some(url) = option_env!("M5MIC_SERVER_URL") {
        return Ok(url.to_string());
    }

    if let Ok(url) = discover_udp(&mut on_progress, 0) {
        return Ok(url);
    }

    discover_mdns(mdns, &mut on_progress, 8)
}

fn discover_mdns(
    mdns: &EspMdns,
    on_progress: &mut impl FnMut(usize),
    start_step: usize,
) -> Result<String> {
    let mut best: Option<ReceiverCandidate> = None;

    for step in 0..6 {
        on_progress(start_step + step);
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
                let candidate = ReceiverCandidate {
                    url: format!("ws://{}:{}{}", addr, result.port, path),
                    priority: txt_priority(result),
                };
                if should_replace(&best, &candidate) {
                    best = Some(candidate);
                }
            }
        }

        if best
            .as_ref()
            .map(|candidate| candidate.priority >= RECEIVER_PRIORITY_PHONE)
            .unwrap_or(false)
        {
            break;
        }
    }

    best.map(|candidate| candidate.url)
        .ok_or_else(|| anyhow!("no mDNS receiver found"))
}

fn discover_udp(on_progress: &mut impl FnMut(usize), start_step: usize) -> Result<String> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .context("bind udp discovery socket")?;
    socket.set_broadcast(true).context("enable udp broadcast")?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .context("set udp discovery timeout")?;

    let target = SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT);

    let mut buf = [0u8; 256];
    let mut best: Option<ReceiverCandidate> = None;
    for step in 0..8 {
        on_progress(start_step + step);
        socket
            .send_to(DISCOVERY_REQUEST, target)
            .context("send udp discovery")?;

        loop {
            match socket.recv_from(&mut buf) {
                Ok((len, _)) => {
                    let text =
                        str::from_utf8(&buf[..len]).context("decode udp discovery response")?;
                    let Some(url) = discovery_response_url(text) else {
                        continue;
                    };
                    let candidate = ReceiverCandidate {
                        url: url.to_string(),
                        priority: discovery_response_priority(text),
                    };
                    if should_replace(&best, &candidate) {
                        best = Some(candidate);
                    }
                    if best
                        .as_ref()
                        .map(|candidate| candidate.priority >= RECEIVER_PRIORITY_PHONE)
                        .unwrap_or(false)
                    {
                        return Ok(best.expect("checked best receiver").url);
                    }
                }
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if let Some(candidate) = best {
                        return Ok(candidate.url);
                    }
                    break;
                }
                Err(err) => return Err(err).context("receive udp discovery"),
            }
        }
    }

    Err(anyhow!("no udp receiver found"))
}

struct ReceiverCandidate {
    url: String,
    priority: u8,
}

fn should_replace(current: &Option<ReceiverCandidate>, candidate: &ReceiverCandidate) -> bool {
    current
        .as_ref()
        .map(|current| candidate.priority >= current.priority)
        .unwrap_or(true)
}

fn txt_priority(result: &QueryResult) -> u8 {
    txt_value(result, "priority")
        .and_then(parse_u8)
        .unwrap_or(RECEIVER_PRIORITY_LEGACY)
}

fn parse_u8(input: &str) -> Option<u8> {
    let mut value = 0u16;
    for byte in input.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u16::from(byte - b'0'))?;
        if value > u16::from(u8::MAX) {
            return None;
        }
    }
    Some(value as u8)
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
