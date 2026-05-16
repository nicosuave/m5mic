use std::{
    ffi::c_void,
    ptr, slice,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Mutex,
    },
};

use anyhow::{anyhow, Context, Result};
use esp_idf_hal::{
    delay::TickType,
    i2s::{I2sDriver, I2sRx},
};
use esp_idf_sys::{esp_err_t, EspError, ESP_OK};
use log::warn;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TransportMode {
    Wireless = 0,
    Usb = 1,
}

pub struct UsbAudio {
    context: &'static UsbAudioContext,
}

pub struct UsbAudioContext {
    i2s: Mutex<I2sDriver<'static, I2sRx>>,
    transport: AtomicU8,
    enabled: AtomicBool,
    level: AtomicU8,
}

type UacOutputCb = unsafe extern "C" fn(*mut u8, usize, *mut c_void) -> esp_err_t;
type UacInputCb = unsafe extern "C" fn(*mut u8, usize, *mut usize, *mut c_void) -> esp_err_t;
type UacSetMuteCb = unsafe extern "C" fn(u32, *mut c_void);
type UacSetVolumeCb = unsafe extern "C" fn(u32, *mut c_void);

#[repr(C)]
struct UacDeviceConfig {
    skip_tinyusb_init: bool,
    output_cb: Option<UacOutputCb>,
    input_cb: Option<UacInputCb>,
    set_mute_cb: Option<UacSetMuteCb>,
    set_volume_cb: Option<UacSetVolumeCb>,
    cb_ctx: *mut c_void,
}

extern "C" {
    fn uac_device_init(config: *mut UacDeviceConfig) -> esp_err_t;
}

impl UsbAudio {
    pub fn new(i2s: I2sDriver<'static, I2sRx>) -> Result<Self> {
        let context = Box::leak(Box::new(UsbAudioContext {
            i2s: Mutex::new(i2s),
            transport: AtomicU8::new(TransportMode::Wireless as u8),
            enabled: AtomicBool::new(false),
            level: AtomicU8::new(0),
        }));

        let mut config = UacDeviceConfig {
            skip_tinyusb_init: false,
            output_cb: None,
            input_cb: Some(uac_input_cb),
            set_mute_cb: None,
            set_volume_cb: None,
            cb_ctx: context as *mut UsbAudioContext as *mut c_void,
        };

        unsafe { EspError::convert(uac_device_init(&mut config)) }
            .context("initialize USB Audio Class device")?;

        Ok(Self { context })
    }

    pub fn set_transport(&self, transport: TransportMode) {
        self.context
            .transport
            .store(transport as u8, Ordering::Relaxed);
        self.context
            .enabled
            .store(matches!(transport, TransportMode::Usb), Ordering::Relaxed);
        if matches!(transport, TransportMode::Wireless) {
            self.context.level.store(0, Ordering::Relaxed);
        }
    }

    pub fn level(&self) -> u8 {
        self.context.level.load(Ordering::Relaxed)
    }

    pub fn read_exact(&self, out: &mut [u8]) -> Result<()> {
        let mut i2s = self
            .context
            .i2s
            .lock()
            .map_err(|_| anyhow!("I2S mutex poisoned"))?;
        read_exact_i2s(&mut i2s, out).context("read I2S")
    }
}

unsafe extern "C" fn uac_input_cb(
    buf: *mut u8,
    len: usize,
    bytes_read: *mut usize,
    cb_ctx: *mut c_void,
) -> esp_err_t {
    if buf.is_null() || bytes_read.is_null() || cb_ctx.is_null() {
        return ESP_OK;
    }

    let context = &*(cb_ctx as *const UsbAudioContext);
    let out = slice::from_raw_parts_mut(buf, len);
    let usb_enabled = context.transport.load(Ordering::Relaxed) == TransportMode::Usb as u8
        && context.enabled.load(Ordering::Relaxed);

    if !usb_enabled {
        out.fill(0);
        ptr::write(bytes_read, len);
        return ESP_OK;
    }

    match context.i2s.lock() {
        Ok(mut i2s) => match read_exact_i2s(&mut i2s, out) {
            Ok(()) => {
                context
                    .level
                    .store(pcm_peak_percent(out), Ordering::Relaxed);
                ptr::write(bytes_read, len);
            }
            Err(err) => {
                warn!("USB mic I2S read failed: {err}");
                out.fill(0);
                context.level.store(0, Ordering::Relaxed);
                ptr::write(bytes_read, len);
            }
        },
        Err(_) => {
            out.fill(0);
            context.level.store(0, Ordering::Relaxed);
            ptr::write(bytes_read, len);
        }
    }

    ESP_OK
}

fn read_exact_i2s(i2s: &mut I2sDriver<I2sRx>, mut out: &mut [u8]) -> Result<(), EspError> {
    while !out.is_empty() {
        let read = i2s.read(out, TickType::new_millis(100).ticks())?;
        if read == 0 {
            return Err(EspError::from(esp_idf_sys::ESP_ERR_TIMEOUT).unwrap());
        }
        let (_, rest) = out.split_at_mut(read);
        out = rest;
    }
    Ok(())
}

fn pcm_peak_percent(pcm: &[u8]) -> u8 {
    let mut peak = 0i32;
    for sample in pcm.chunks_exact(2) {
        let value = i16::from_le_bytes([sample[0], sample[1]]) as i32;
        let amplitude = if value < 0 { -value } else { value };
        if amplitude > peak {
            peak = amplitude;
        }
    }

    ((peak * 100) / 32_768).min(100) as u8
}
