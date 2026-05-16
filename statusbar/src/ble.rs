use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use btleplug::{
    api::{Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter, WriteType},
    platform::{Adapter, Manager, Peripheral},
};
use futures_util::StreamExt;
use m5mic_protocol::{
    BleAudioFragmentHeader, BLE_AUDIO_CHARACTERISTIC_UUID, BLE_CONTROL_CHARACTERISTIC_UUID,
    BLE_SERVICE_UUID,
};
use m5mic_receiver::{LiveAudioOutput, LiveAudioStatus};
use tokio::{sync::watch, time};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BleReceiverStatus {
    Starting,
    Scanning,
    Connecting,
    Connected,
    Receiving { stream_id: u32 },
    Error(String),
}

pub async fn run(status_tx: watch::Sender<BleReceiverStatus>) {
    loop {
        if let Err(err) = run_once(&status_tx).await {
            let message = err.to_string();
            tracing::warn!(%message, "Bluetooth receiver failed");
            let _ = status_tx.send(BleReceiverStatus::Error(message));
            time::sleep(Duration::from_secs(2)).await;
        }
    }
}

pub async fn send_mode_command(payload: &'static [u8]) -> Result<()> {
    let service_uuid = Uuid::parse_str(BLE_SERVICE_UUID).context("parse BLE service UUID")?;
    let control_uuid =
        Uuid::parse_str(BLE_CONTROL_CHARACTERISTIC_UUID).context("parse BLE control UUID")?;
    let manager = Manager::new().await.context("create Bluetooth manager")?;
    let adapter = first_adapter(&manager).await?;
    let peripheral = find_m5mic(&adapter, service_uuid).await?;

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
        .context("write Bluetooth mode command")
}

async fn run_once(status_tx: &watch::Sender<BleReceiverStatus>) -> Result<()> {
    let service_uuid = Uuid::parse_str(BLE_SERVICE_UUID).context("parse BLE service UUID")?;
    let audio_uuid =
        Uuid::parse_str(BLE_AUDIO_CHARACTERISTIC_UUID).context("parse BLE audio UUID")?;
    let manager = Manager::new().await.context("create Bluetooth manager")?;
    let adapter = first_adapter(&manager).await?;

    let _ = status_tx.send(BleReceiverStatus::Scanning);
    let peripheral = find_m5mic(&adapter, service_uuid).await?;
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

    while let Some(notification) = notifications.next().await {
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
    Ok(())
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
    adapter
        .start_scan(ScanFilter {
            services: vec![service_uuid],
        })
        .await
        .context("scan for Bluetooth m5mic")?;

    for _ in 0..20 {
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
                return Ok(peripheral);
            }
        }
        time::sleep(Duration::from_millis(500)).await;
    }

    Err(anyhow!("Bluetooth m5mic not found"))
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
