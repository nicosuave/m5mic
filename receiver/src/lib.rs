use std::{
    fs,
    net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::StreamExt;
use hound::{SampleFormat, WavSpec, WavWriter};
use m5mic_protocol::{
    discovery_response_url, AudioFrameHeader, Codec, DISCOVERY_PORT, DISCOVERY_REQUEST,
    DISCOVERY_RESPONSE_PREFIX, MDNS_TYPE_DOMAIN, WS_PATH, WS_PORT,
};
use m5mic_virtual_mic::VirtualMicWriter;
use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceInfo};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::watch,
};
use tracing::{debug, error, info, warn};

use m5mic_protocol::{ima_adpcm4_decode, ImaAdpcmState};

#[derive(Clone, Debug)]
pub struct ReceiverConfig {
    pub listen: String,
    pub ws_port: u16,
    pub discovery_port: u16,
    pub output_dir: Option<PathBuf>,
    pub instance: String,
    pub virtual_mic: bool,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0".to_string(),
            ws_port: WS_PORT,
            discovery_port: DISCOVERY_PORT,
            output_dir: Some(PathBuf::from("captures")),
            instance: "M5Mic Receiver".to_string(),
            virtual_mic: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiverStatus {
    Starting,
    Waiting,
    Connected,
    Receiving { stream_id: u32 },
    Stopped,
    Error(String),
}

#[derive(Clone)]
struct AppState {
    output_dir: Option<PathBuf>,
    virtual_mic: Option<Arc<Mutex<VirtualMicWriter>>>,
    status_tx: Option<watch::Sender<ReceiverStatus>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveAudioStatus {
    Started { stream_id: u32 },
    Audio { stream_id: u32 },
    Ended { stream_id: u32 },
}

pub struct LiveAudioOutput {
    virtual_mic: VirtualMicWriter,
    virtual_buffer: Vec<f32>,
    decoded_pcm: Vec<u8>,
    adpcm_state: ImaAdpcmState,
    active_stream_id: Option<u32>,
}

impl LiveAudioOutput {
    pub fn open_default() -> Result<Self> {
        let mut virtual_mic = VirtualMicWriter::open_default().context("open virtual mic ring")?;
        virtual_mic.set_idle();
        Ok(Self {
            virtual_mic,
            virtual_buffer: Vec::with_capacity(1_920),
            decoded_pcm: Vec::with_capacity(1_280),
            adpcm_state: ImaAdpcmState::new(),
            active_stream_id: None,
        })
    }

    pub fn handle_frame(&mut self, frame: &[u8]) -> Result<LiveAudioStatus> {
        let header = AudioFrameHeader::decode(frame)
            .map_err(|err| anyhow::anyhow!("decode audio header: {err:?}"))?;
        let stream_started =
            header.is_stream_start() || self.active_stream_id != Some(header.stream_id);
        if stream_started {
            self.active_stream_id = Some(header.stream_id);
            self.adpcm_state.reset();
        }

        if header.is_stream_end() {
            self.set_idle();
            return Ok(LiveAudioStatus::Ended {
                stream_id: header.stream_id,
            });
        }

        let payload = header
            .payload(frame)
            .map_err(|err| anyhow::anyhow!("read audio payload: {err:?}"))?;
        if !payload.is_empty() {
            let pcm_payload = decode_audio_payload(
                &header,
                payload,
                &mut self.decoded_pcm,
                &mut self.adpcm_state,
            )?;
            write_virtual_mic_writer(&mut self.virtual_mic, pcm_payload, &mut self.virtual_buffer);
        }

        if stream_started {
            Ok(LiveAudioStatus::Started {
                stream_id: header.stream_id,
            })
        } else {
            Ok(LiveAudioStatus::Audio {
                stream_id: header.stream_id,
            })
        }
    }

    pub fn set_idle(&mut self) {
        self.virtual_mic.set_idle();
        self.active_stream_id = None;
        self.adpcm_state.reset();
    }
}

pub async fn run(
    config: ReceiverConfig,
    status_tx: Option<watch::Sender<ReceiverStatus>>,
) -> Result<()> {
    if let Some(output_dir) = &config.output_dir {
        fs::create_dir_all(output_dir).context("create output directory")?;
    }

    let virtual_mic = if config.virtual_mic {
        let mut writer = VirtualMicWriter::open_default().context("open virtual mic ring")?;
        writer.set_idle();
        Some(Arc::new(Mutex::new(writer)))
    } else {
        None
    };

    let _mdns = advertise_mdns(&config)?;
    tokio::spawn(udp_discovery(config.discovery_port, config.ws_port));

    let state = Arc::new(AppState {
        output_dir: config.output_dir.clone(),
        virtual_mic,
        status_tx,
    });
    publish_status(&state, ReceiverStatus::Waiting);

    let app = Router::new()
        .route("/", get(root))
        .route(WS_PATH, get(ws_handler))
        .with_state(state.clone());

    let bind_addr = format!("{}:{}", config.listen, config.ws_port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("bind {bind_addr}"))?;

    info!("receiver listening on ws://{bind_addr}{WS_PATH}");
    axum::serve(listener, app).await.context("serve receiver")?;
    publish_status(&state, ReceiverStatus::Stopped);
    Ok(())
}

async fn root() -> &'static str {
    "m5mic receiver\n"
}

async fn ws_handler(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    publish_status(&state, ReceiverStatus::Connected);
    let peer_started = now_unix_secs();
    let mut writer: Option<WavWriter<std::io::BufWriter<std::fs::File>>> = None;
    let mut output_path: Option<PathBuf> = None;
    let mut frames = 0u64;
    let mut samples = 0u64;
    let mut dropped_frames = 0u64;
    let mut active_stream_id: Option<u32> = None;
    let mut expected_sequence: Option<u32> = None;
    let mut virtual_buffer = Vec::with_capacity(1_920);
    let mut adpcm_state = ImaAdpcmState::new();
    let mut decoded_pcm = Vec::with_capacity(1_280);

    while let Some(message) = socket.next().await {
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                warn!(%err, "websocket receive failed");
                break;
            }
        };

        match message {
            Message::Binary(bytes) => {
                let header = match AudioFrameHeader::decode(bytes.as_ref()) {
                    Ok(header) => header,
                    Err(err) => {
                        warn!(?err, "dropping malformed frame");
                        continue;
                    }
                };

                if header.is_stream_start() || active_stream_id != Some(header.stream_id) {
                    if let Some(previous) = active_stream_id {
                        if previous != header.stream_id {
                            warn!(
                                previous_stream_id = previous,
                                stream_id = header.stream_id,
                                "stream id changed inside one websocket"
                            );
                        }
                    }
                    active_stream_id = Some(header.stream_id);
                    expected_sequence = None;
                    adpcm_state.reset();
                    publish_status(
                        &state,
                        ReceiverStatus::Receiving {
                            stream_id: header.stream_id,
                        },
                    );
                }

                if let Some(expected) = expected_sequence {
                    if header.sequence != expected {
                        let missed = if header.sequence > expected {
                            header.sequence - expected
                        } else {
                            0
                        };
                        dropped_frames += missed as u64;
                        warn!(
                            stream_id = header.stream_id,
                            expected,
                            received = header.sequence,
                            missed,
                            "audio frame gap"
                        );
                    }
                }
                expected_sequence = Some(header.sequence.wrapping_add(1));

                if header.is_stream_end() {
                    info!(
                        stream_id = header.stream_id,
                        sequence = header.sequence,
                        dropped_frames,
                        "stream ended by device"
                    );
                    break;
                }

                let payload = match header.payload(bytes.as_ref()) {
                    Ok(payload) => payload,
                    Err(err) => {
                        warn!(?err, "dropping frame with bad payload");
                        continue;
                    }
                };

                if payload.is_empty() {
                    debug!(
                        stream_id = header.stream_id,
                        sequence = header.sequence,
                        "ignored empty audio frame"
                    );
                    continue;
                }

                let pcm_payload = match decode_audio_payload(
                    &header,
                    payload,
                    &mut decoded_pcm,
                    &mut adpcm_state,
                ) {
                    Ok(payload) => payload,
                    Err(err) => {
                        warn!(%err, "dropping bad audio frame");
                        continue;
                    }
                };

                write_virtual_mic(&state, pcm_payload, &mut virtual_buffer);

                if state.output_dir.is_some() && writer.is_none() {
                    let output_dir = state.output_dir.as_ref().expect("checked output dir");
                    let path = output_dir
                        .join(format!("m5mic-{peer_started}-{:08x}.wav", header.stream_id));
                    let mode = if header.is_push_to_talk() {
                        "push-to-talk"
                    } else {
                        "latched"
                    };
                    match create_writer(&path, &header) {
                        Ok(created) => {
                            info!(
                                path = %path.display(),
                                sample_rate = header.sample_rate,
                                channels = header.channels,
                                stream_id = header.stream_id,
                                mode,
                                "recording started"
                            );
                            output_path = Some(path);
                            writer = Some(created);
                        }
                        Err(err) => {
                            error!(%err, "failed to create wav file");
                            break;
                        }
                    }
                }

                if let Some(writer) = writer.as_mut() {
                    match write_pcm_payload(writer, pcm_payload) {
                        Ok(written) => {
                            frames += 1;
                            samples += written;
                            debug!(
                                sequence = header.sequence,
                                timestamp_us = header.timestamp_us,
                                bytes = payload.len(),
                                "audio frame"
                            );
                        }
                        Err(err) => {
                            error!(%err, "failed to write wav samples");
                            break;
                        }
                    }
                } else {
                    frames += 1;
                    samples += (pcm_payload.len() / 2) as u64;
                }
            }
            Message::Close(reason) => {
                info!(?reason, "websocket closed by device");
                break;
            }
            Message::Text(text) => {
                debug!(%text, "text message");
                if discovery_response_url(text.as_ref()).is_some() {
                    debug!("ignored discovery response sent over websocket");
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    set_virtual_mic_idle(&state);

    if let Some(writer) = writer {
        if let Err(err) = writer.finalize() {
            warn!(%err, "failed to finalize wav");
        }
    }

    if let Some(path) = output_path {
        info!(
            path = %path.display(),
            frames,
            samples,
            dropped_frames,
            "recording finished"
        );
    } else {
        info!(frames, samples, dropped_frames, "stream finished");
    }

    publish_status(&state, ReceiverStatus::Waiting);
}

fn create_writer(
    path: &Path,
    header: &AudioFrameHeader,
) -> Result<WavWriter<std::io::BufWriter<std::fs::File>>> {
    let spec = WavSpec {
        channels: header.channels as u16,
        sample_rate: header.sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    WavWriter::create(path, spec).context("create wav writer")
}

fn write_pcm_payload(
    writer: &mut WavWriter<std::io::BufWriter<std::fs::File>>,
    payload: &[u8],
) -> Result<u64> {
    if payload.len() % 2 != 0 {
        warn!(bytes = payload.len(), "pcm payload has trailing byte");
    }

    let mut written = 0u64;
    for sample in payload.chunks_exact(2) {
        writer
            .write_sample(i16::from_le_bytes([sample[0], sample[1]]))
            .context("write wav sample")?;
        written += 1;
    }
    Ok(written)
}

fn decode_audio_payload<'a>(
    header: &AudioFrameHeader,
    payload: &'a [u8],
    decoded_pcm: &'a mut Vec<u8>,
    adpcm_state: &mut ImaAdpcmState,
) -> Result<&'a [u8]> {
    match header.codec {
        Codec::PcmS16Le => Ok(payload),
        Codec::ImaAdpcm4 => {
            let sample_count = payload.len() * 2;
            decoded_pcm.resize(sample_count * 2, 0);
            let decoded_len = ima_adpcm4_decode(payload, sample_count, decoded_pcm, adpcm_state)
                .map_err(|err| anyhow::anyhow!("decode adpcm frame: {err:?}"))?;
            Ok(&decoded_pcm[..decoded_len])
        }
    }
}

fn write_virtual_mic(state: &AppState, payload: &[u8], buffer: &mut Vec<f32>) {
    let Some(virtual_mic) = &state.virtual_mic else {
        return;
    };

    if let Ok(mut writer) = virtual_mic.lock() {
        write_virtual_mic_writer(&mut writer, payload, buffer);
    }
}

fn write_virtual_mic_writer(
    virtual_mic: &mut VirtualMicWriter,
    payload: &[u8],
    buffer: &mut Vec<f32>,
) {
    pcm_s16le_16k_mono_to_f32_48k(payload, buffer);
    virtual_mic.write_f32(buffer);
}

fn set_virtual_mic_idle(state: &AppState) {
    if let Some(virtual_mic) = &state.virtual_mic {
        if let Ok(mut writer) = virtual_mic.lock() {
            writer.set_idle();
        }
    }
}

fn pcm_s16le_16k_mono_to_f32_48k(payload: &[u8], out: &mut Vec<f32>) {
    out.clear();
    let sample_count = payload.len() / 2;
    out.reserve(sample_count * 3);

    for index in 0..sample_count {
        let byte_index = index * 2;
        let current =
            i16::from_le_bytes([payload[byte_index], payload[byte_index + 1]]) as f32 / 32768.0;
        let next = if index + 1 < sample_count {
            let next_index = byte_index + 2;
            i16::from_le_bytes([payload[next_index], payload[next_index + 1]]) as f32 / 32768.0
        } else {
            current
        };

        out.push(current);
        out.push(current + (next - current) / 3.0);
        out.push(current + (next - current) * 2.0 / 3.0);
    }
}

fn advertise_mdns(config: &ReceiverConfig) -> Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new().context("create mdns daemon")?;
    let monitor = mdns.monitor().context("monitor mdns daemon")?;
    std::thread::spawn(move || {
        while let Ok(event) = monitor.recv() {
            if let DaemonEvent::Error(error) = event {
                error!(%error, "mdns daemon error");
            }
        }
    });

    let hostname = format!("{}.local.", sanitize_hostname(&config.instance));
    let props = [
        ("path", WS_PATH),
        ("codec", "pcm_s16le"),
        ("codecs", "pcm_s16le,ima_adpcm4"),
        ("sample_rate", "16000"),
        ("channels", "1"),
        ("udp_discovery_port", "47777"),
    ];
    let service = ServiceInfo::new(
        MDNS_TYPE_DOMAIN,
        &config.instance,
        &hostname,
        "",
        config.ws_port,
        &props[..],
    )
    .context("build mdns service")?
    .enable_addr_auto();

    let fullname = service.get_fullname().to_string();
    mdns.register(service).context("register mdns service")?;
    info!(%fullname, "advertised mdns service");
    Ok(mdns)
}

async fn udp_discovery(discovery_port: u16, ws_port: u16) {
    let bind_addr = SocketAddr::from(([0, 0, 0, 0], discovery_port));
    let socket = match UdpSocket::bind(bind_addr).await {
        Ok(socket) => socket,
        Err(err) => {
            error!(%err, %bind_addr, "failed to bind udp discovery");
            return;
        }
    };

    info!(port = discovery_port, "udp discovery listening");
    let mut buf = [0u8; 512];

    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(result) => result,
            Err(err) => {
                warn!(%err, "udp discovery receive failed");
                continue;
            }
        };

        if !buf[..len].starts_with(DISCOVERY_REQUEST) {
            debug!(%peer, bytes = len, "ignoring unknown udp discovery payload");
            continue;
        }

        let local_ip = local_ip_for(peer).unwrap_or(IpAddr::from([127, 0, 0, 1]));
        let response = format!("{DISCOVERY_RESPONSE_PREFIX}ws://{local_ip}:{ws_port}{WS_PATH}\n");
        if let Err(err) = socket.send_to(response.as_bytes(), peer).await {
            warn!(%err, %peer, "udp discovery response failed");
        } else {
            info!(%peer, url = response.trim(), "sent udp discovery response");
        }
    }
}

fn local_ip_for(peer: SocketAddr) -> Option<IpAddr> {
    let bind = match peer {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    let socket = StdUdpSocket::bind(bind).ok()?;
    socket.connect(peer).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn sanitize_hostname(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == '_' || ch.is_whitespace() {
            out.push('-');
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

fn publish_status(state: &AppState, status: ReceiverStatus) {
    if let Some(status_tx) = &state.status_tx {
        let _ = status_tx.send(status);
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use m5mic_protocol::{ima_adpcm4_encode, FLAG_STREAM_START};

    #[test]
    fn resamples_16k_s16_mono_to_48k_f32() {
        let payload = [0x00, 0x00, 0x00, 0x40];
        let mut out = Vec::new();

        pcm_s16le_16k_mono_to_f32_48k(&payload, &mut out);

        assert_eq!(out.len(), 6);
        assert_eq!(out[0], 0.0);
        assert!(out[1] > 0.16 && out[1] < 0.17);
        assert!(out[2] > 0.33 && out[2] < 0.34);
        assert_eq!(out[3], 0.5);
    }

    #[test]
    fn decodes_adpcm_payload_to_pcm_for_audio_pipeline() {
        let mut pcm = [0u8; 1_280];
        for (index, sample) in pcm.chunks_exact_mut(2).enumerate() {
            let value = (((index as i32 % 96) - 48) * 300) as i16;
            sample.copy_from_slice(&value.to_le_bytes());
        }

        let mut adpcm = [0u8; 320];
        let mut encode_state = ImaAdpcmState::new();
        let adpcm_len = ima_adpcm4_encode(&pcm, &mut adpcm, &mut encode_state).unwrap();
        let header = AudioFrameHeader::new(
            Codec::ImaAdpcm4,
            1,
            16_000,
            0,
            0,
            adpcm_len as u16,
            1,
            FLAG_STREAM_START,
        );

        let mut decoded = Vec::new();
        let mut decode_state = ImaAdpcmState::new();
        let payload = decode_audio_payload(
            &header,
            &adpcm[..adpcm_len],
            &mut decoded,
            &mut decode_state,
        )
        .unwrap();

        assert_eq!(payload.len(), pcm.len());
        assert!(payload.chunks_exact(2).any(|sample| sample != [0, 0]));
    }
}
