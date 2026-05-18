use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use btleplug::{
    api::{Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter, WriteType},
    platform::{Adapter, Manager, Peripheral},
};
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use futures_util::StreamExt;
use hkdf::Hkdf;
use m5mic_protocol::{
    BleAudioFragmentHeader, BLE_AUDIO_CHARACTERISTIC_UUID, BLE_CONTROL_CHARACTERISTIC_UUID,
    BLE_PROVISION_CHARACTERISTIC_UUID, BLE_PROVISION_CODE_DIGITS, BLE_PROVISION_INFO_MAGIC,
    BLE_PROVISION_NONCE_LEN, BLE_PROVISION_SALT_LEN, BLE_PROVISION_WIFI_MAGIC, BLE_SERVICE_UUID,
};
use m5mic_receiver::{LiveAudioOutput, LiveAudioStatus};
use sha2::Sha256;
use tokio::{sync::watch, time};
use uuid::Uuid;

const PROVISION_KEY_INFO: &[u8] = b"m5mic ble wifi v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BleReceiverStatus {
    Disabled,
    Scanning,
    Connecting,
    Connected,
    Receiving { stream_id: u32 },
    Error(String),
}

pub async fn run(
    status_tx: watch::Sender<BleReceiverStatus>,
    mut enabled_rx: watch::Receiver<bool>,
) {
    loop {
        if !receiver_enabled(&enabled_rx) {
            let _ = status_tx.send(BleReceiverStatus::Disabled);
            if enabled_rx.changed().await.is_err() {
                break;
            }
            continue;
        }

        if let Err(err) = run_once(&status_tx, &mut enabled_rx).await {
            let message = err.to_string();
            tracing::warn!(%message, "Bluetooth receiver failed");
            let _ = status_tx.send(BleReceiverStatus::Error(message));
            time::sleep(Duration::from_secs(2)).await;
        }
    }
}

pub async fn send_control_command(payload: &'static [u8]) -> Result<()> {
    let service_uuid = Uuid::parse_str(BLE_SERVICE_UUID).context("parse BLE service UUID")?;
    let control_uuid =
        Uuid::parse_str(BLE_CONTROL_CHARACTERISTIC_UUID).context("parse BLE control UUID")?;
    let manager = Manager::new().await.context("create Bluetooth manager")?;
    let adapter = first_adapter(&manager).await?;
    let peripheral = find_m5mic(&adapter, service_uuid).await?;

    let was_connected = peripheral
        .is_connected()
        .await
        .context("check Bluetooth connection")?;
    if !was_connected {
        peripheral
            .connect()
            .await
            .context("connect Bluetooth m5mic")?;
    }

    let result = async {
        peripheral
            .discover_services()
            .await
            .context("discover Bluetooth services")?;

        let control_characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|characteristic| {
                characteristic.uuid == control_uuid
                    && (characteristic.properties.contains(CharPropFlags::WRITE)
                        || characteristic
                            .properties
                            .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE))
            })
            .ok_or_else(|| anyhow!("m5mic Bluetooth control characteristic not found"))?;

        peripheral
            .write(&control_characteristic, payload, WriteType::WithResponse)
            .await
            .context("write Bluetooth control command")
    }
    .await;

    if !was_connected {
        let _ = peripheral.disconnect().await;
    }

    result
}

pub async fn provision_wifi(ssid: &str, password: &str, setup_code: &str) -> Result<()> {
    validate_wifi_credentials(ssid, password)?;
    let code = normalize_setup_code(setup_code)?;
    let service_uuid = Uuid::parse_str(BLE_SERVICE_UUID).context("parse BLE service UUID")?;
    let provision_uuid =
        Uuid::parse_str(BLE_PROVISION_CHARACTERISTIC_UUID).context("parse BLE provision UUID")?;
    let manager = Manager::new().await.context("create Bluetooth manager")?;
    let adapter = first_adapter(&manager).await?;
    let peripheral = find_m5mic(&adapter, service_uuid).await?;

    let was_connected = peripheral
        .is_connected()
        .await
        .context("check Bluetooth connection")?;
    if !was_connected {
        peripheral
            .connect()
            .await
            .context("connect Bluetooth m5mic")?;
    }

    let result = async {
        peripheral
            .discover_services()
            .await
            .context("discover Bluetooth services")?;

        let provision_characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|characteristic| {
                characteristic.uuid == provision_uuid
                    && characteristic.properties.contains(CharPropFlags::READ)
                    && (characteristic.properties.contains(CharPropFlags::WRITE)
                        || characteristic
                            .properties
                            .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE))
            })
            .ok_or_else(|| anyhow!("m5mic Bluetooth provisioning characteristic not found"))?;

        let info = peripheral
            .read(&provision_characteristic)
            .await
            .context("read Bluetooth provisioning info")?;
        let salt = parse_provisioning_info(&info)?;
        let payload = encrypted_wifi_payload(ssid, password, &code, salt)?;
        peripheral
            .write(&provision_characteristic, &payload, WriteType::WithResponse)
            .await
            .context("write Bluetooth Wi-Fi provisioning payload")
    }
    .await;

    if !was_connected {
        let _ = peripheral.disconnect().await;
    }

    result
}

async fn run_once(
    status_tx: &watch::Sender<BleReceiverStatus>,
    enabled_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let service_uuid = Uuid::parse_str(BLE_SERVICE_UUID).context("parse BLE service UUID")?;
    let audio_uuid =
        Uuid::parse_str(BLE_AUDIO_CHARACTERISTIC_UUID).context("parse BLE audio UUID")?;
    let manager = Manager::new().await.context("create Bluetooth manager")?;
    let adapter = first_adapter(&manager).await?;

    let _ = status_tx.send(BleReceiverStatus::Scanning);
    let Some(peripheral) = find_m5mic_while_enabled(&adapter, service_uuid, enabled_rx).await?
    else {
        let _ = status_tx.send(BleReceiverStatus::Disabled);
        return Ok(());
    };
    let _ = status_tx.send(BleReceiverStatus::Connecting);

    if !peripheral
        .is_connected()
        .await
        .context("check Bluetooth connection")?
    {
        peripheral
            .connect()
            .await
            .context("connect Bluetooth m5mic")?;
    }
    peripheral
        .discover_services()
        .await
        .context("discover Bluetooth services")?;
    if !receiver_enabled(enabled_rx) {
        let _ = peripheral.disconnect().await;
        let _ = status_tx.send(BleReceiverStatus::Disabled);
        return Ok(());
    }

    let audio_characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|characteristic| {
            characteristic.uuid == audio_uuid
                && characteristic.properties.contains(CharPropFlags::NOTIFY)
        })
        .ok_or_else(|| anyhow!("m5mic Bluetooth audio characteristic not found"))?;

    let mut notifications = peripheral
        .notifications()
        .await
        .context("open Bluetooth notification stream")?;
    peripheral
        .subscribe(&audio_characteristic)
        .await
        .context("subscribe to Bluetooth audio")?;

    tracing::info!(mtu = peripheral.mtu(), "Bluetooth m5mic connected");
    let _ = status_tx.send(BleReceiverStatus::Connected);

    let mut live_audio = LiveAudioOutput::open_default().context("open live audio output")?;
    let mut reassembler = BleFrameReassembler::default();

    loop {
        let notification = tokio::select! {
            changed = enabled_rx.changed() => {
                if changed.is_err() || !receiver_enabled(enabled_rx) {
                    break;
                }
                continue;
            }
            notification = notifications.next() => notification,
        };

        let Some(notification) = notification else {
            break;
        };

        if notification.uuid != audio_uuid {
            continue;
        }

        let Some(frame) = reassembler
            .push(&notification.value)
            .context("reassemble Bluetooth audio frame")?
        else {
            continue;
        };

        match live_audio
            .handle_frame(&frame)
            .context("process Bluetooth audio frame")?
        {
            LiveAudioStatus::Started { stream_id } | LiveAudioStatus::Audio { stream_id } => {
                let _ = status_tx.send(BleReceiverStatus::Receiving { stream_id });
            }
            LiveAudioStatus::Ended { .. } => {
                let _ = status_tx.send(BleReceiverStatus::Connected);
            }
        }
    }

    live_audio.set_idle();
    let _ = peripheral.disconnect().await;
    if !receiver_enabled(enabled_rx) {
        let _ = status_tx.send(BleReceiverStatus::Disabled);
    }
    Ok(())
}

fn validate_wifi_credentials(ssid: &str, password: &str) -> Result<()> {
    if ssid.is_empty() {
        return Err(anyhow!("Wi-Fi name is required"));
    }
    if ssid.len() > 32 {
        return Err(anyhow!("Wi-Fi name is too long"));
    }
    if password.len() > 64 {
        return Err(anyhow!("Wi-Fi password is too long"));
    }
    Ok(())
}

fn normalize_setup_code(input: &str) -> Result<[u8; BLE_PROVISION_CODE_DIGITS]> {
    let mut code = [0u8; BLE_PROVISION_CODE_DIGITS];
    let mut count = 0;
    for byte in input.bytes() {
        if byte == b' ' || byte == b'-' {
            continue;
        }
        if !byte.is_ascii_digit() || count == code.len() {
            return Err(anyhow!("Bluetooth setup code must be 8 digits"));
        }
        code[count] = byte;
        count += 1;
    }
    if count != code.len() {
        return Err(anyhow!("Bluetooth setup code must be 8 digits"));
    }
    Ok(code)
}

fn parse_provisioning_info(info: &[u8]) -> Result<&[u8; BLE_PROVISION_SALT_LEN]> {
    let expected_len = BLE_PROVISION_INFO_MAGIC.len() + BLE_PROVISION_SALT_LEN;
    if info.len() != expected_len {
        return Err(anyhow!("bad Bluetooth provisioning info length"));
    }
    if &info[..BLE_PROVISION_INFO_MAGIC.len()] != BLE_PROVISION_INFO_MAGIC {
        return Err(anyhow!("bad Bluetooth provisioning info magic"));
    }
    info[BLE_PROVISION_INFO_MAGIC.len()..]
        .try_into()
        .map_err(|_| anyhow!("bad Bluetooth provisioning salt"))
}

fn encrypted_wifi_payload(
    ssid: &str,
    password: &str,
    code: &[u8; BLE_PROVISION_CODE_DIGITS],
    salt: &[u8; BLE_PROVISION_SALT_LEN],
) -> Result<Vec<u8>> {
    let mut plaintext = Vec::with_capacity(2 + ssid.len() + password.len());
    plaintext.push(ssid.len() as u8);
    plaintext.push(password.len() as u8);
    plaintext.extend_from_slice(ssid.as_bytes());
    plaintext.extend_from_slice(password.as_bytes());

    let mut key = [0u8; 32];
    let hkdf = Hkdf::<Sha256>::new(Some(salt), code);
    hkdf.expand(PROVISION_KEY_INFO, &mut key)
        .map_err(|_| anyhow!("derive Bluetooth provisioning key"))?;
    let cipher = ChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| anyhow!("create Bluetooth provisioning cipher"))?;
    let mut nonce = [0u8; BLE_PROVISION_NONCE_LEN];
    getrandom::fill(&mut nonce).context("generate Bluetooth provisioning nonce")?;
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plaintext,
                aad: BLE_PROVISION_WIFI_MAGIC,
            },
        )
        .map_err(|_| anyhow!("encrypt Wi-Fi credentials"))?;

    let mut payload = Vec::with_capacity(
        BLE_PROVISION_WIFI_MAGIC.len() + BLE_PROVISION_NONCE_LEN + encrypted.len(),
    );
    payload.extend_from_slice(BLE_PROVISION_WIFI_MAGIC);
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&encrypted);
    Ok(payload)
}

async fn first_adapter(manager: &Manager) -> Result<Adapter> {
    manager
        .adapters()
        .await
        .context("list Bluetooth adapters")?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Bluetooth adapters found"))
}

async fn find_m5mic(adapter: &Adapter, service_uuid: Uuid) -> Result<Peripheral> {
    find_m5mic_inner(adapter, service_uuid, None)
        .await?
        .ok_or_else(|| anyhow!("Bluetooth m5mic not found"))
}

async fn find_m5mic_while_enabled(
    adapter: &Adapter,
    service_uuid: Uuid,
    enabled_rx: &mut watch::Receiver<bool>,
) -> Result<Option<Peripheral>> {
    find_m5mic_inner(adapter, service_uuid, Some(enabled_rx)).await
}

async fn find_m5mic_inner(
    adapter: &Adapter,
    service_uuid: Uuid,
    mut enabled_rx: Option<&mut watch::Receiver<bool>>,
) -> Result<Option<Peripheral>> {
    adapter
        .start_scan(ScanFilter {
            services: vec![service_uuid],
        })
        .await
        .context("scan for Bluetooth m5mic")?;

    for _ in 0..20 {
        if enabled_rx
            .as_ref()
            .map(|rx| !receiver_enabled(rx))
            .unwrap_or(false)
        {
            return Ok(None);
        }

        for peripheral in adapter.peripherals().await.context("list peripherals")? {
            let Some(properties) = peripheral.properties().await.context("read properties")? else {
                continue;
            };
            let has_service = properties.services.contains(&service_uuid);
            let has_name = properties
                .local_name
                .as_deref()
                .map(|name| name.eq_ignore_ascii_case("m5mic"))
                .unwrap_or(false);
            if has_service || has_name {
                return Ok(Some(peripheral));
            }
        }

        if let Some(enabled_rx) = enabled_rx.as_deref_mut() {
            tokio::select! {
                changed = enabled_rx.changed() => {
                    if changed.is_err() || !receiver_enabled(enabled_rx) {
                        return Ok(None);
                    }
                }
                _ = time::sleep(Duration::from_millis(500)) => {}
            }
        } else {
            time::sleep(Duration::from_millis(500)).await;
        }
    }

    Ok(None)
}

fn receiver_enabled(enabled_rx: &watch::Receiver<bool>) -> bool {
    *enabled_rx.borrow()
}

#[derive(Default)]
struct BleFrameReassembler {
    frame_sequence: Option<u32>,
    fragment_count: u8,
    received_count: u8,
    fragments: Vec<Option<Vec<u8>>>,
}

impl BleFrameReassembler {
    fn push(&mut self, fragment: &[u8]) -> Result<Option<Vec<u8>>> {
        let header = BleAudioFragmentHeader::decode(fragment)
            .map_err(|err| anyhow!("decode BLE fragment: {err:?}"))?;
        let payload = header
            .payload(fragment)
            .map_err(|err| anyhow!("read BLE fragment payload: {err:?}"))?;

        if self.frame_sequence != Some(header.frame_sequence)
            || self.fragment_count != header.fragment_count
        {
            self.frame_sequence = Some(header.frame_sequence);
            self.fragment_count = header.fragment_count;
            self.received_count = 0;
            self.fragments.clear();
            self.fragments
                .resize_with(header.fragment_count as usize, || None);
        }

        let slot = self
            .fragments
            .get_mut(header.fragment_index as usize)
            .ok_or_else(|| anyhow!("BLE fragment index out of range"))?;
        if slot.is_none() {
            self.received_count = self.received_count.saturating_add(1);
        }
        *slot = Some(payload.to_vec());

        if self.received_count != self.fragment_count {
            return Ok(None);
        }

        let mut frame = Vec::new();
        for fragment in &mut self.fragments {
            let fragment = fragment
                .take()
                .ok_or_else(|| anyhow!("BLE frame missing fragment"))?;
            frame.extend_from_slice(&fragment);
        }
        self.frame_sequence = None;
        self.fragment_count = 0;
        self.received_count = 0;
        self.fragments.clear();
        Ok(Some(frame))
    }
}
