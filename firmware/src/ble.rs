use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc,
};

use anyhow::{anyhow, Context, Result};
use esp32_nimble::{
    utilities::mutex::Mutex, uuid128, BLEAdvertisementData, BLECharacteristic, BLEDevice,
    NimbleProperties,
};
use esp_idf_hal::delay::FreeRtos;
use log::{info, warn};
use m5mic_protocol::{
    ble_audio_fragment_payload_capacity, BleAudioFragmentHeader, BLE_AUDIO_FRAGMENT_HEADER_LEN,
    CONTROL_MODE_BLE, CONTROL_MODE_USB, CONTROL_MODE_WIFI, CONTROL_MODE_WIRELESS,
};

const NOTIFICATION_BYTES: usize = 180;
const SERVICE_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000001");
const AUDIO_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000002");
const CONTROL_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000003");
const STATUS_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000004");
const MODE_NONE: u8 = 0;
const MODE_USB: u8 = 1;
const MODE_WIFI: u8 = 2;
const MODE_BLUETOOTH: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BleModeCommand {
    Usb,
    Wifi,
    Bluetooth,
}

pub struct BleAudioServer {
    audio: Arc<Mutex<BLECharacteristic>>,
    status: Arc<Mutex<BLECharacteristic>>,
    connected: Arc<AtomicBool>,
    subscribed: Arc<AtomicBool>,
    mode_command: Arc<AtomicU8>,
}

impl BleAudioServer {
    pub fn start() -> Result<Self> {
        let device = BLEDevice::take();
        let advertising = device.get_advertising();
        let server = device.get_server();

        let connected = Arc::new(AtomicBool::new(false));
        let subscribed = Arc::new(AtomicBool::new(false));
        let mode_command = Arc::new(AtomicU8::new(MODE_NONE));

        server.on_connect({
            let connected = connected.clone();
            move |server, desc| {
                connected.store(true, Ordering::Relaxed);
                info!("Bluetooth client connected: {:?}", desc);
                if let Err(err) = server.update_conn_params(desc.conn_handle(), 12, 24, 0, 60) {
                    warn!("Bluetooth connection parameter update failed: {err:?}");
                }
            }
        });

        server.on_disconnect({
            let connected = connected.clone();
            let subscribed = subscribed.clone();
            move |desc, reason| {
                connected.store(false, Ordering::Relaxed);
                subscribed.store(false, Ordering::Relaxed);
                info!("Bluetooth client disconnected: {:?} ({:?})", desc, reason);
            }
        });

        let service = server.create_service(SERVICE_UUID);
        let audio = service.lock().create_characteristic(
            AUDIO_UUID,
            NimbleProperties::READ | NimbleProperties::NOTIFY,
        );
        audio.lock().set_value(b"");
        audio.lock().on_subscribe({
            let subscribed = subscribed.clone();
            move |_, desc, subscription| {
                let active = !subscription.is_empty();
                subscribed.store(active, Ordering::Relaxed);
                info!(
                    "Bluetooth audio subscription changed: conn_handle={} active={}",
                    desc.conn_handle(),
                    active
                );
            }
        });

        let control = service.lock().create_characteristic(
            CONTROL_UUID,
            NimbleProperties::READ | NimbleProperties::WRITE,
        );
        control.lock().set_value(b"idle");
        control.lock().on_write({
            let mode_command = mode_command.clone();
            move |args| {
                let data = args.recv_data();
                if data == CONTROL_MODE_USB {
                    mode_command.store(MODE_USB, Ordering::Relaxed);
                    info!("Bluetooth mode command: USB");
                } else if data == CONTROL_MODE_WIFI || data == CONTROL_MODE_WIRELESS {
                    mode_command.store(MODE_WIFI, Ordering::Relaxed);
                    info!("Bluetooth mode command: Wi-Fi");
                } else if data == CONTROL_MODE_BLE {
                    mode_command.store(MODE_BLUETOOTH, Ordering::Relaxed);
                    info!("Bluetooth mode command: Bluetooth");
                } else {
                    warn!("unknown Bluetooth control write: {:?}", data);
                }
            }
        });

        let status = service.lock().create_characteristic(
            STATUS_UUID,
            NimbleProperties::READ | NimbleProperties::NOTIFY,
        );
        status.lock().set_value(b"idle");

        advertising
            .lock()
            .set_data(
                BLEAdvertisementData::new()
                    .name("m5mic")
                    .add_service_uuid(SERVICE_UUID),
            )
            .context("set Bluetooth advertising data")?;
        advertising
            .lock()
            .start()
            .context("start Bluetooth advertising")?;

        info!("Bluetooth m5mic GATT server advertising");
        Ok(Self {
            audio,
            status,
            connected,
            subscribed,
            mode_command,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.connected.load(Ordering::Relaxed) && self.subscribed.load(Ordering::Relaxed)
    }

    pub fn set_status(&self, value: &'static [u8]) {
        let mut status = self.status.lock();
        status.set_value(value);
        status.notify();
    }

    pub fn take_mode_command(&self) -> Option<BleModeCommand> {
        match self.mode_command.swap(MODE_NONE, Ordering::Relaxed) {
            MODE_USB => Some(BleModeCommand::Usb),
            MODE_WIFI => Some(BleModeCommand::Wifi),
            MODE_BLUETOOTH => Some(BleModeCommand::Bluetooth),
            _ => None,
        }
    }

    pub fn notify_frame(&self, frame_sequence: u32, frame: &[u8]) -> Result<()> {
        let capacity = ble_audio_fragment_payload_capacity(NOTIFICATION_BYTES);
        if capacity == 0 {
            return Err(anyhow!("Bluetooth notification payload is too small"));
        }
        let fragment_count = frame.len().div_ceil(capacity);
        if fragment_count > u8::MAX as usize {
            return Err(anyhow!("Bluetooth frame needs too many fragments"));
        }

        let mut notification = [0u8; NOTIFICATION_BYTES];
        for fragment_index in 0..fragment_count {
            let start = fragment_index * capacity;
            let end = (start + capacity).min(frame.len());
            let payload = &frame[start..end];
            let header = BleAudioFragmentHeader::new(
                frame_sequence,
                fragment_index as u8,
                fragment_count as u8,
                payload.len() as u16,
            )
            .map_err(|err| anyhow!("build Bluetooth fragment header: {err:?}"))?;
            header
                .encode_into(&mut notification[..BLE_AUDIO_FRAGMENT_HEADER_LEN])
                .map_err(|err| anyhow!("encode Bluetooth fragment header: {err:?}"))?;
            notification
                [BLE_AUDIO_FRAGMENT_HEADER_LEN..BLE_AUDIO_FRAGMENT_HEADER_LEN + payload.len()]
                .copy_from_slice(payload);

            self.audio
                .lock()
                .set_value(&notification[..BLE_AUDIO_FRAGMENT_HEADER_LEN + payload.len()])
                .notify();
            FreeRtos::delay_ms(2);
        }

        Ok(())
    }
}
