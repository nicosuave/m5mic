use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc, Mutex as StdMutex,
};

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use esp32_nimble::{
    utilities::mutex::Mutex, uuid128, BLEAdvertisementData, BLECharacteristic, BLEDevice,
    NimbleProperties,
};
use esp_idf_hal::delay::FreeRtos;
use hkdf::Hkdf;
use log::{info, warn};
use m5mic_protocol::{
    ble_audio_fragment_payload_capacity, BleAudioFragmentHeader, BLE_AUDIO_FRAGMENT_HEADER_LEN,
    BLE_PROVISION_INFO_MAGIC, BLE_PROVISION_NONCE_LEN, BLE_PROVISION_SALT_LEN,
    BLE_PROVISION_WIFI_MAGIC, CONTROL_MODE_BLE, CONTROL_MODE_USB, CONTROL_MODE_WIFI,
    CONTROL_MODE_WIRELESS,
};
use sha2::Sha256;

const NOTIFICATION_BYTES: usize = 180;
const SERVICE_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000001");
const AUDIO_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000002");
const CONTROL_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000003");
const STATUS_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000004");
const PROVISION_UUID: esp32_nimble::utilities::BleUuid =
    uuid128!("6d356d69-6321-4d35-8000-000000000005");
const MODE_NONE: u8 = 0;
const MODE_USB: u8 = 1;
const MODE_WIFI: u8 = 2;
const MODE_BLUETOOTH: u8 = 3;
const PROVISION_KEY_INFO: &[u8] = b"m5mic ble wifi v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BleModeCommand {
    Usb,
    Wifi,
    Bluetooth,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvisionedWifi {
    pub ssid: String,
    pub password: String,
}

pub struct BleAudioServer {
    audio: Arc<Mutex<BLECharacteristic>>,
    status: Arc<Mutex<BLECharacteristic>>,
    connected: Arc<AtomicBool>,
    subscribed: Arc<AtomicBool>,
    mode_command: Arc<AtomicU8>,
    provision_code: u32,
    provisioned_wifi: Arc<StdMutex<Option<ProvisionedWifi>>>,
}

impl BleAudioServer {
    pub fn start() -> Result<Self> {
        let device = BLEDevice::take();
        let advertising = device.get_advertising();
        let server = device.get_server();

        let connected = Arc::new(AtomicBool::new(false));
        let subscribed = Arc::new(AtomicBool::new(false));
        let mode_command = Arc::new(AtomicU8::new(MODE_NONE));
        let provision_code = random_provision_code();
        let provision_salt = random_provision_salt();
        let provisioned_wifi = Arc::new(StdMutex::new(None));

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

        let provision = service.lock().create_characteristic(
            PROVISION_UUID,
            NimbleProperties::READ | NimbleProperties::WRITE,
        );
        provision
            .lock()
            .set_value(&provision_info_payload(&provision_salt));
        provision.lock().on_write({
            let provisioned_wifi = provisioned_wifi.clone();
            move |args| {
                let data = args.recv_data();
                match decrypt_wifi_payload(data, provision_code, &provision_salt) {
                    Ok(credentials) => {
                        info!(
                            "Bluetooth Wi-Fi provisioning received for SSID {}",
                            credentials.ssid
                        );
                        match provisioned_wifi.lock() {
                            Ok(mut pending) => *pending = Some(credentials),
                            Err(err) => warn!("Bluetooth provisioning lock poisoned: {err}"),
                        }
                    }
                    Err(err) => warn!("Bluetooth Wi-Fi provisioning failed: {err:#}"),
                }
            }
        });

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
            provision_code,
            provisioned_wifi,
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

    pub fn provision_code(&self) -> u32 {
        self.provision_code
    }

    pub fn take_provisioned_wifi(&self) -> Option<ProvisionedWifi> {
        self.provisioned_wifi
            .lock()
            .ok()
            .and_then(|mut pending| pending.take())
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

fn random_provision_code() -> u32 {
    unsafe { esp_idf_sys::esp_random() % 100_000_000 }
}

fn random_provision_salt() -> [u8; BLE_PROVISION_SALT_LEN] {
    let mut salt = [0u8; BLE_PROVISION_SALT_LEN];
    for chunk in salt.chunks_mut(4) {
        let random = unsafe { esp_idf_sys::esp_random() }.to_le_bytes();
        chunk.copy_from_slice(&random[..chunk.len()]);
    }
    salt
}

fn provision_info_payload(
    salt: &[u8; BLE_PROVISION_SALT_LEN],
) -> [u8; BLE_PROVISION_INFO_MAGIC.len() + BLE_PROVISION_SALT_LEN] {
    let mut payload = [0u8; BLE_PROVISION_INFO_MAGIC.len() + BLE_PROVISION_SALT_LEN];
    payload[..BLE_PROVISION_INFO_MAGIC.len()].copy_from_slice(BLE_PROVISION_INFO_MAGIC);
    payload[BLE_PROVISION_INFO_MAGIC.len()..].copy_from_slice(salt);
    payload
}

fn decrypt_wifi_payload(
    payload: &[u8],
    code: u32,
    salt: &[u8; BLE_PROVISION_SALT_LEN],
) -> Result<ProvisionedWifi> {
    let header_len = BLE_PROVISION_WIFI_MAGIC.len() + BLE_PROVISION_NONCE_LEN;
    if payload.len() <= header_len {
        return Err(anyhow!("provisioning payload is too short"));
    }
    if &payload[..BLE_PROVISION_WIFI_MAGIC.len()] != BLE_PROVISION_WIFI_MAGIC {
        return Err(anyhow!("bad provisioning magic"));
    }

    let nonce_start = BLE_PROVISION_WIFI_MAGIC.len();
    let nonce_end = nonce_start + BLE_PROVISION_NONCE_LEN;
    let nonce = Nonce::from_slice(&payload[nonce_start..nonce_end]);
    let encrypted = &payload[nonce_end..];
    let mut key = [0u8; 32];
    derive_provisioning_key(code, salt, &mut key)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| anyhow!("create provisioning cipher"))?;
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: encrypted,
                aad: BLE_PROVISION_WIFI_MAGIC,
            },
        )
        .map_err(|_| anyhow!("decrypt provisioning payload"))?;
    parse_wifi_plaintext(&plaintext)
}

fn derive_provisioning_key(
    code: u32,
    salt: &[u8; BLE_PROVISION_SALT_LEN],
    out: &mut [u8; 32],
) -> Result<()> {
    let code = provision_code_bytes(code);
    let hkdf = Hkdf::<Sha256>::new(Some(salt), &code);
    hkdf.expand(PROVISION_KEY_INFO, out)
        .map_err(|_| anyhow!("derive provisioning key"))
}

fn provision_code_bytes(code: u32) -> [u8; 8] {
    let mut out = *b"00000000";
    let mut value = code % 100_000_000;
    for index in (0..out.len()).rev() {
        out[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    out
}

fn parse_wifi_plaintext(plaintext: &[u8]) -> Result<ProvisionedWifi> {
    if plaintext.len() < 2 {
        return Err(anyhow!("Wi-Fi plaintext is too short"));
    }
    let ssid_len = plaintext[0] as usize;
    let password_len = plaintext[1] as usize;
    if ssid_len == 0 || ssid_len > 32 {
        return Err(anyhow!("Wi-Fi SSID length is invalid"));
    }
    if password_len > 64 {
        return Err(anyhow!("Wi-Fi password length is invalid"));
    }
    if plaintext.len() != 2 + ssid_len + password_len {
        return Err(anyhow!("Wi-Fi plaintext length mismatch"));
    }

    let ssid_start = 2;
    let ssid_end = ssid_start + ssid_len;
    let password_end = ssid_end + password_len;
    let ssid = core::str::from_utf8(&plaintext[ssid_start..ssid_end])
        .context("decode provisioned SSID")?
        .to_string();
    let password = core::str::from_utf8(&plaintext[ssid_end..password_end])
        .context("decode provisioned password")?
        .to_string();
    Ok(ProvisionedWifi { ssid, password })
}
