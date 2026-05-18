use std::{ptr, slice};

use m5mic_protocol::{
    discovery_response_url, ima_adpcm4_decode, AudioFrameHeader, BleAudioFragmentHeader, Codec,
    ImaAdpcmState, CONTROL_MODE_BLE, CONTROL_MODE_USB, CONTROL_MODE_WIFI, CONTROL_PORT,
    CONTROL_RECORD_START, CONTROL_RECORD_STOP, DISCOVERY_PORT, DISCOVERY_REQUEST, WS_PORT,
};

const OK: i32 = 0;
const INCOMPLETE: i32 = 1;
const STREAM_STARTED: i32 = 2;
const STREAM_AUDIO: i32 = 3;
const STREAM_ENDED: i32 = 4;
const ERROR_NULL: i32 = -1;
const ERROR_PROTOCOL: i32 = -2;
const ERROR_UNSUPPORTED_FORMAT: i32 = -3;
const ERROR_OUTPUT_TOO_SMALL: i32 = -4;
const ERROR_FRAGMENT_TOO_LARGE: i32 = -5;
const OUTPUT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_FRAME_CAPACITY: usize = 2_048;
const DEFAULT_OUTPUT_SAMPLE_CAPACITY: usize = 4_096;
const WS_PATH_C: &[u8] = b"/audio\0";
const BONJOUR_TYPE_C: &[u8] = b"_m5mic._tcp.local.\0";
const BONJOUR_SERVICE_TYPE_C: &[u8] = b"_m5mic._tcp\0";
const BLE_SERVICE_UUID_C: &[u8] = b"6d356d69-6321-4d35-8000-000000000001\0";
const BLE_AUDIO_UUID_C: &[u8] = b"6d356d69-6321-4d35-8000-000000000002\0";
const BLE_CONTROL_UUID_C: &[u8] = b"6d356d69-6321-4d35-8000-000000000003\0";
const BLE_STATUS_UUID_C: &[u8] = b"6d356d69-6321-4d35-8000-000000000004\0";
const DISCOVERY_RESPONSE_PREFIX_C: &[u8] = b"M5MIC_SERVER_V1 \0";

#[repr(C)]
pub struct M5MicDecoder {
    decoded_pcm: Vec<u8>,
    adpcm_state: ImaAdpcmState,
    active_stream_id: Option<u32>,
}

impl M5MicDecoder {
    fn new() -> Self {
        Self {
            decoded_pcm: Vec::with_capacity(1_280),
            adpcm_state: ImaAdpcmState::new(),
            active_stream_id: None,
        }
    }

    fn reset(&mut self) {
        self.decoded_pcm.clear();
        self.adpcm_state.reset();
        self.active_stream_id = None;
    }

    fn decode_frame(
        &mut self,
        frame: &[u8],
        out_samples: &mut [f32],
        metadata: &mut FrameMetadata,
    ) -> i32 {
        let header = match AudioFrameHeader::decode(frame) {
            Ok(header) => header,
            Err(_) => return ERROR_PROTOCOL,
        };
        metadata.stream_id = header.stream_id;
        metadata.sample_rate = OUTPUT_SAMPLE_RATE;
        metadata.channels = header.channels;
        metadata.flags = header.flags;
        metadata.out_sample_len = 0;
        metadata.level = 0;

        if header.channels != 1 || header.sample_rate == 0 {
            return ERROR_UNSUPPORTED_FORMAT;
        }

        let stream_started =
            header.is_stream_start() || self.active_stream_id != Some(header.stream_id);
        if stream_started {
            self.active_stream_id = Some(header.stream_id);
            self.adpcm_state.reset();
        }

        if header.is_stream_end() {
            self.reset();
            return STREAM_ENDED;
        }

        let payload = match header.payload(frame) {
            Ok(payload) => payload,
            Err(_) => return ERROR_PROTOCOL,
        };
        if payload.is_empty() {
            return if stream_started {
                STREAM_STARTED
            } else {
                STREAM_AUDIO
            };
        }

        let pcm = match decode_audio_payload(
            header.codec,
            payload,
            &mut self.decoded_pcm,
            &mut self.adpcm_state,
        ) {
            Ok(pcm) => pcm,
            Err(err) => return err,
        };

        metadata.level = pcm_peak_percent(pcm);
        let output_len = resampled_len(pcm.len() / 2, header.sample_rate);
        if out_samples.len() < output_len {
            return ERROR_OUTPUT_TOO_SMALL;
        }
        resample_mono_s16le_to_f32_48k(pcm, header.sample_rate, &mut out_samples[..output_len]);
        metadata.out_sample_len = output_len;

        if stream_started {
            STREAM_STARTED
        } else {
            STREAM_AUDIO
        }
    }
}

#[derive(Default)]
struct FrameMetadata {
    out_sample_len: usize,
    stream_id: u32,
    level: u8,
    sample_rate: u32,
    channels: u8,
    flags: u16,
}

#[repr(C)]
pub struct M5MicBleReassembler {
    frame_sequence: Option<u32>,
    fragment_count: u8,
    received_count: u8,
    fragments: Vec<Option<Vec<u8>>>,
}

impl M5MicBleReassembler {
    fn new() -> Self {
        Self {
            frame_sequence: None,
            fragment_count: 0,
            received_count: 0,
            fragments: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.frame_sequence = None;
        self.fragment_count = 0;
        self.received_count = 0;
        self.fragments.clear();
    }

    fn push(&mut self, fragment: &[u8], out_frame: &mut [u8]) -> Result<Option<usize>, i32> {
        let header = BleAudioFragmentHeader::decode(fragment).map_err(|_| ERROR_PROTOCOL)?;
        let payload = header.payload(fragment).map_err(|_| ERROR_PROTOCOL)?;

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
            .ok_or(ERROR_PROTOCOL)?;
        if slot.is_none() {
            self.received_count = self.received_count.saturating_add(1);
        }
        *slot = Some(payload.to_vec());

        if self.received_count != self.fragment_count {
            return Ok(None);
        }

        let total_len = self
            .fragments
            .iter()
            .map(|fragment| fragment.as_ref().map_or(0, Vec::len))
            .sum::<usize>();
        if out_frame.len() < total_len {
            self.reset();
            return Err(ERROR_FRAGMENT_TOO_LARGE);
        }

        let mut offset = 0;
        for fragment in &mut self.fragments {
            let fragment = fragment.take().ok_or(ERROR_PROTOCOL)?;
            out_frame[offset..offset + fragment.len()].copy_from_slice(&fragment);
            offset += fragment.len();
        }
        self.reset();
        Ok(Some(total_len))
    }
}

#[no_mangle]
pub extern "C" fn m5mic_discovery_port() -> u16 {
    DISCOVERY_PORT
}

#[no_mangle]
pub extern "C" fn m5mic_control_port() -> u16 {
    CONTROL_PORT
}

#[no_mangle]
pub extern "C" fn m5mic_ws_port() -> u16 {
    WS_PORT
}

#[no_mangle]
pub extern "C" fn m5mic_ws_path() -> *const std::ffi::c_char {
    c_string_ptr(WS_PATH_C)
}

#[no_mangle]
pub extern "C" fn m5mic_bonjour_type() -> *const std::ffi::c_char {
    c_string_ptr(BONJOUR_TYPE_C)
}

#[no_mangle]
pub extern "C" fn m5mic_bonjour_service_type() -> *const std::ffi::c_char {
    c_string_ptr(BONJOUR_SERVICE_TYPE_C)
}

#[no_mangle]
pub extern "C" fn m5mic_ble_service_uuid() -> *const std::ffi::c_char {
    c_string_ptr(BLE_SERVICE_UUID_C)
}

#[no_mangle]
pub extern "C" fn m5mic_ble_audio_characteristic_uuid() -> *const std::ffi::c_char {
    c_string_ptr(BLE_AUDIO_UUID_C)
}

#[no_mangle]
pub extern "C" fn m5mic_ble_control_characteristic_uuid() -> *const std::ffi::c_char {
    c_string_ptr(BLE_CONTROL_UUID_C)
}

#[no_mangle]
pub extern "C" fn m5mic_ble_status_characteristic_uuid() -> *const std::ffi::c_char {
    c_string_ptr(BLE_STATUS_UUID_C)
}

#[no_mangle]
pub extern "C" fn m5mic_discovery_request(len: *mut usize) -> *const u8 {
    byte_slice_ptr(DISCOVERY_REQUEST, len)
}

#[no_mangle]
pub extern "C" fn m5mic_discovery_response_prefix() -> *const std::ffi::c_char {
    c_string_ptr(DISCOVERY_RESPONSE_PREFIX_C)
}

#[no_mangle]
pub extern "C" fn m5mic_control_mode_wifi(len: *mut usize) -> *const u8 {
    byte_slice_ptr(CONTROL_MODE_WIFI, len)
}

#[no_mangle]
pub extern "C" fn m5mic_control_mode_ble(len: *mut usize) -> *const u8 {
    byte_slice_ptr(CONTROL_MODE_BLE, len)
}

#[no_mangle]
pub extern "C" fn m5mic_control_mode_usb(len: *mut usize) -> *const u8 {
    byte_slice_ptr(CONTROL_MODE_USB, len)
}

#[no_mangle]
pub extern "C" fn m5mic_control_record_start(len: *mut usize) -> *const u8 {
    byte_slice_ptr(CONTROL_RECORD_START, len)
}

#[no_mangle]
pub extern "C" fn m5mic_control_record_stop(len: *mut usize) -> *const u8 {
    byte_slice_ptr(CONTROL_RECORD_STOP, len)
}

#[no_mangle]
pub extern "C" fn m5mic_default_frame_capacity() -> usize {
    DEFAULT_FRAME_CAPACITY
}

#[no_mangle]
pub extern "C" fn m5mic_default_output_sample_capacity() -> usize {
    DEFAULT_OUTPUT_SAMPLE_CAPACITY
}

#[no_mangle]
pub extern "C" fn m5mic_decoder_new() -> *mut M5MicDecoder {
    Box::into_raw(Box::new(M5MicDecoder::new()))
}

#[no_mangle]
pub unsafe extern "C" fn m5mic_decoder_free(decoder: *mut M5MicDecoder) {
    if !decoder.is_null() {
        drop(Box::from_raw(decoder));
    }
}

#[no_mangle]
pub unsafe extern "C" fn m5mic_decoder_reset(decoder: *mut M5MicDecoder) {
    if let Some(decoder) = decoder.as_mut() {
        decoder.reset();
    }
}

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn m5mic_decode_frame(
    decoder: *mut M5MicDecoder,
    frame: *const u8,
    frame_len: usize,
    out_samples: *mut f32,
    out_sample_capacity: usize,
    out_sample_len: *mut usize,
    stream_id: *mut u32,
    level: *mut u8,
    sample_rate: *mut u32,
    channels: *mut u8,
    flags: *mut u16,
) -> i32 {
    let Some(decoder) = decoder.as_mut() else {
        return ERROR_NULL;
    };
    if frame.is_null() || out_samples.is_null() || out_sample_len.is_null() {
        return ERROR_NULL;
    }

    let frame = slice::from_raw_parts(frame, frame_len);
    let out_samples = slice::from_raw_parts_mut(out_samples, out_sample_capacity);
    let mut metadata = FrameMetadata::default();
    let status = decoder.decode_frame(frame, out_samples, &mut metadata);

    ptr::write(out_sample_len, metadata.out_sample_len);
    write_if_present(stream_id, metadata.stream_id);
    write_if_present(level, metadata.level);
    write_if_present(sample_rate, metadata.sample_rate);
    write_if_present(channels, metadata.channels);
    write_if_present(flags, metadata.flags);

    status
}

#[no_mangle]
pub extern "C" fn m5mic_ble_reassembler_new() -> *mut M5MicBleReassembler {
    Box::into_raw(Box::new(M5MicBleReassembler::new()))
}

#[no_mangle]
pub unsafe extern "C" fn m5mic_ble_reassembler_free(reassembler: *mut M5MicBleReassembler) {
    if !reassembler.is_null() {
        drop(Box::from_raw(reassembler));
    }
}

#[no_mangle]
pub unsafe extern "C" fn m5mic_ble_reassembler_reset(reassembler: *mut M5MicBleReassembler) {
    if let Some(reassembler) = reassembler.as_mut() {
        reassembler.reset();
    }
}

#[no_mangle]
pub unsafe extern "C" fn m5mic_ble_reassembler_push(
    reassembler: *mut M5MicBleReassembler,
    fragment: *const u8,
    fragment_len: usize,
    out_frame: *mut u8,
    out_frame_capacity: usize,
    out_frame_len: *mut usize,
) -> i32 {
    let Some(reassembler) = reassembler.as_mut() else {
        return ERROR_NULL;
    };
    if fragment.is_null() || out_frame.is_null() || out_frame_len.is_null() {
        return ERROR_NULL;
    }

    ptr::write(out_frame_len, 0);
    let fragment = slice::from_raw_parts(fragment, fragment_len);
    let out_frame = slice::from_raw_parts_mut(out_frame, out_frame_capacity);
    match reassembler.push(fragment, out_frame) {
        Ok(Some(len)) => {
            ptr::write(out_frame_len, len);
            OK
        }
        Ok(None) => INCOMPLETE,
        Err(err) => err,
    }
}

fn byte_slice_ptr(bytes: &'static [u8], len: *mut usize) -> *const u8 {
    if !len.is_null() {
        unsafe {
            ptr::write(len, bytes.len());
        }
    }
    bytes.as_ptr()
}

fn c_string_ptr(bytes: &'static [u8]) -> *const std::ffi::c_char {
    bytes.as_ptr().cast()
}

unsafe fn write_if_present<T>(out: *mut T, value: T) {
    if !out.is_null() {
        ptr::write(out, value);
    }
}

fn decode_audio_payload<'a>(
    codec: Codec,
    payload: &'a [u8],
    decoded_pcm: &'a mut Vec<u8>,
    adpcm_state: &mut ImaAdpcmState,
) -> Result<&'a [u8], i32> {
    match codec {
        Codec::PcmS16Le => Ok(payload),
        Codec::ImaAdpcm4 => {
            let sample_count = payload.len() * 2;
            decoded_pcm.resize(sample_count * 2, 0);
            let decoded_len = ima_adpcm4_decode(payload, sample_count, decoded_pcm, adpcm_state)
                .map_err(|_| ERROR_PROTOCOL)?;
            Ok(&decoded_pcm[..decoded_len])
        }
    }
}

fn resampled_len(input_samples: usize, input_sample_rate: u32) -> usize {
    if input_samples == 0 || input_sample_rate == 0 {
        return 0;
    }
    ((input_samples as u64 * OUTPUT_SAMPLE_RATE as u64 + input_sample_rate as u64 - 1)
        / input_sample_rate as u64) as usize
}

fn resample_mono_s16le_to_f32_48k(payload: &[u8], input_sample_rate: u32, out: &mut [f32]) {
    let input_samples = payload.len() / 2;
    if input_samples == 0 {
        return;
    }

    for (out_index, out_sample) in out.iter_mut().enumerate() {
        let source_position = out_index as u64 * input_sample_rate as u64;
        let base = (source_position / OUTPUT_SAMPLE_RATE as u64) as usize;
        let remainder = (source_position % OUTPUT_SAMPLE_RATE as u64) as f32;
        let fraction = remainder / OUTPUT_SAMPLE_RATE as f32;
        let current = pcm_sample_f32(payload, base.min(input_samples - 1));
        let next = pcm_sample_f32(payload, (base + 1).min(input_samples - 1));
        *out_sample = current + (next - current) * fraction;
    }
}

fn pcm_sample_f32(payload: &[u8], sample_index: usize) -> f32 {
    let byte_index = sample_index * 2;
    i16::from_le_bytes([payload[byte_index], payload[byte_index + 1]]) as f32 / 32768.0
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

#[allow(dead_code)]
fn parse_discovery_response(payload: &str) -> Option<&str> {
    discovery_response_url(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use m5mic_protocol::{
        ima_adpcm4_encode, BleAudioFragmentHeader, FLAG_STREAM_START, HEADER_LEN,
    };

    #[test]
    fn decode_pcm_frame_to_48k_float_samples() {
        let payload = [0x00, 0x00, 0x00, 0x40];
        let mut frame = vec![0u8; HEADER_LEN + payload.len()];
        let header = AudioFrameHeader::new(
            Codec::PcmS16Le,
            1,
            16_000,
            0,
            0,
            payload.len() as u16,
            0x1234,
            FLAG_STREAM_START,
        );
        header.encode_into(&mut frame).unwrap();
        frame[HEADER_LEN..].copy_from_slice(&payload);

        let mut decoder = M5MicDecoder::new();
        let mut samples = [0.0; 16];
        let mut metadata = FrameMetadata::default();
        let status = decoder.decode_frame(&frame, &mut samples, &mut metadata);

        assert_eq!(status, STREAM_STARTED);
        assert_eq!(metadata.out_sample_len, 6);
        assert_eq!(metadata.stream_id, 0x1234);
        assert_eq!(samples[0], 0.0);
        assert!(samples[1] > 0.16 && samples[1] < 0.17);
        assert!(samples[2] > 0.33 && samples[2] < 0.34);
        assert_eq!(samples[3], 0.5);
    }

    #[test]
    fn decode_adpcm_frame() {
        let mut pcm = [0u8; 1_280];
        for (index, sample) in pcm.chunks_exact_mut(2).enumerate() {
            let value = (((index as i32 % 96) - 48) * 300) as i16;
            sample.copy_from_slice(&value.to_le_bytes());
        }
        let mut adpcm = [0u8; 320];
        let mut encode_state = ImaAdpcmState::new();
        let adpcm_len = ima_adpcm4_encode(&pcm, &mut adpcm, &mut encode_state).unwrap();
        let mut frame = vec![0u8; HEADER_LEN + adpcm_len];
        let header = AudioFrameHeader::new(
            Codec::ImaAdpcm4,
            1,
            16_000,
            0,
            0,
            adpcm_len as u16,
            0x5678,
            FLAG_STREAM_START,
        );
        header.encode_into(&mut frame).unwrap();
        frame[HEADER_LEN..].copy_from_slice(&adpcm[..adpcm_len]);

        let mut decoder = M5MicDecoder::new();
        let mut samples = vec![0.0; DEFAULT_OUTPUT_SAMPLE_CAPACITY];
        let mut metadata = FrameMetadata::default();
        let status = decoder.decode_frame(&frame, &mut samples, &mut metadata);

        assert_eq!(status, STREAM_STARTED);
        assert_eq!(metadata.out_sample_len, 1_920);
        assert!(metadata.level > 0);
        assert!(samples.iter().any(|sample| *sample != 0.0));
    }

    #[test]
    fn ble_reassembler_waits_until_all_fragments_arrive() {
        let frame = b"hello bluetooth frame";
        let first_header = BleAudioFragmentHeader::new(7, 0, 2, 5).unwrap();
        let second_header = BleAudioFragmentHeader::new(7, 1, 2, (frame.len() - 5) as u16).unwrap();
        let mut first = [0u8; 13];
        let mut second = [0u8; 64];
        first_header.encode_into(&mut first).unwrap();
        first[8..].copy_from_slice(&frame[..5]);
        second_header.encode_into(&mut second).unwrap();
        second[8..8 + frame.len() - 5].copy_from_slice(&frame[5..]);

        let mut reassembler = M5MicBleReassembler::new();
        let mut out = [0u8; 64];
        assert_eq!(reassembler.push(&first, &mut out).unwrap(), None);
        let len = reassembler
            .push(&second[..8 + frame.len() - 5], &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(&out[..len], frame);
    }
}
