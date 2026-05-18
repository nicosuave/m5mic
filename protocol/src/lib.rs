#![cfg_attr(not(feature = "std"), no_std)]

pub const DISCOVERY_PORT: u16 = 47_777;
pub const CONTROL_PORT: u16 = 47_779;
pub const WS_PORT: u16 = 47_776;
pub const WS_PATH: &str = "/audio";
pub const MDNS_SERVICE: &str = "_m5mic";
pub const MDNS_PROTO: &str = "_tcp";
pub const MDNS_TYPE_DOMAIN: &str = "_m5mic._tcp.local.";
pub const DISCOVERY_REQUEST: &[u8] = b"M5MIC_DISCOVER_V1";
pub const DISCOVERY_RESPONSE_PREFIX: &str = "M5MIC_SERVER_V1 ";
pub const CONTROL_MODE_USB: &[u8] = b"M5MIC_MODE_USB_V1";
pub const CONTROL_MODE_WIRELESS: &[u8] = b"M5MIC_MODE_WIRELESS_V1";
pub const CONTROL_MODE_WIFI: &[u8] = b"M5MIC_MODE_WIFI_V1";
pub const CONTROL_MODE_BLE: &[u8] = b"M5MIC_MODE_BLE_V1";
pub const CONTROL_RECORD_START: &[u8] = b"M5MIC_RECORD_START_V1";
pub const CONTROL_RECORD_STOP: &[u8] = b"M5MIC_RECORD_STOP_V1";
pub const CONTROL_PRIORITY_LEGACY: u8 = 10;
pub const CONTROL_PRIORITY_PHONE: u8 = 100;
pub const RECEIVER_PRIORITY_LEGACY: u8 = 0;
pub const RECEIVER_PRIORITY_DESKTOP: u8 = 10;
pub const RECEIVER_PRIORITY_PHONE: u8 = 100;
pub const BLE_SERVICE_UUID: &str = "6d356d69-6321-4d35-8000-000000000001";
pub const BLE_AUDIO_CHARACTERISTIC_UUID: &str = "6d356d69-6321-4d35-8000-000000000002";
pub const BLE_CONTROL_CHARACTERISTIC_UUID: &str = "6d356d69-6321-4d35-8000-000000000003";
pub const BLE_STATUS_CHARACTERISTIC_UUID: &str = "6d356d69-6321-4d35-8000-000000000004";
pub const BLE_PROVISION_CHARACTERISTIC_UUID: &str = "6d356d69-6321-4d35-8000-000000000005";
pub const BLE_PROVISION_INFO_MAGIC: &[u8; 5] = b"M5PI1";
pub const BLE_PROVISION_WIFI_MAGIC: &[u8; 5] = b"M5PW1";
pub const BLE_PROVISION_SALT_LEN: usize = 16;
pub const BLE_PROVISION_NONCE_LEN: usize = 12;
pub const BLE_PROVISION_CODE_DIGITS: usize = 8;

pub const MAGIC: [u8; 4] = *b"M5AU";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 32;
pub const BLE_AUDIO_FRAGMENT_HEADER_LEN: usize = 8;
pub const FLAG_STREAM_START: u16 = 0x0001;
pub const FLAG_STREAM_END: u16 = 0x0002;
pub const FLAG_PUSH_TO_TALK: u16 = 0x0004;
pub const IMA_ADPCM4_SAMPLES_PER_BYTE: usize = 2;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub enum Codec {
    PcmS16Le = 1,
    ImaAdpcm4 = 2,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ControlMode {
    Usb,
    Wifi,
    Bluetooth,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ControlAction {
    SetMode(ControlMode),
    RecordStart,
    RecordStop,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ControlCommand {
    pub action: ControlAction,
    pub priority: u8,
}

impl Codec {
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::PcmS16Le),
            2 => Some(Self::ImaAdpcm4),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PcmS16Le => "pcm_s16le",
            Self::ImaAdpcm4 => "ima_adpcm4",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ImaAdpcmState {
    predictor: i16,
    step_index: u8,
}

impl ImaAdpcmState {
    pub const fn new() -> Self {
        Self {
            predictor: 0,
            step_index: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for ImaAdpcmState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct AudioFrameHeader {
    pub codec: Codec,
    pub channels: u8,
    pub sample_rate: u32,
    pub sequence: u32,
    pub timestamp_us: u64,
    pub payload_len: u16,
    pub flags: u16,
    pub stream_id: u32,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ProtocolError {
    BufferTooSmall,
    BadMagic,
    BadVersion,
    BadHeaderLength,
    BadCodec,
    PayloadTooLarge,
    FrameTooShort,
    InvalidPcmLength,
    OutputTooSmall,
    BadAdpcmSampleCount,
    BadFragmentIndex,
}

impl AudioFrameHeader {
    pub const fn new(
        codec: Codec,
        channels: u8,
        sample_rate: u32,
        sequence: u32,
        timestamp_us: u64,
        payload_len: u16,
        stream_id: u32,
        flags: u16,
    ) -> Self {
        Self {
            codec,
            channels,
            sample_rate,
            sequence,
            timestamp_us,
            payload_len,
            flags,
            stream_id,
        }
    }

    pub const fn is_stream_start(&self) -> bool {
        self.flags & FLAG_STREAM_START != 0
    }

    pub const fn is_stream_end(&self) -> bool {
        self.flags & FLAG_STREAM_END != 0
    }

    pub const fn is_push_to_talk(&self) -> bool {
        self.flags & FLAG_PUSH_TO_TALK != 0
    }

    pub fn encode_into(&self, out: &mut [u8]) -> Result<(), ProtocolError> {
        if out.len() < HEADER_LEN {
            return Err(ProtocolError::BufferTooSmall);
        }

        out[..HEADER_LEN].fill(0);
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5] = HEADER_LEN as u8;
        out[6] = self.codec as u8;
        out[7] = self.channels;
        out[8..12].copy_from_slice(&self.sample_rate.to_le_bytes());
        out[12..16].copy_from_slice(&self.sequence.to_le_bytes());
        out[16..24].copy_from_slice(&self.timestamp_us.to_le_bytes());
        out[24..26].copy_from_slice(&self.payload_len.to_le_bytes());
        out[26..28].copy_from_slice(&self.flags.to_le_bytes());
        out[28..32].copy_from_slice(&self.stream_id.to_le_bytes());
        Ok(())
    }

    pub fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        if frame.len() < HEADER_LEN {
            return Err(ProtocolError::FrameTooShort);
        }
        if frame[0..4] != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        if frame[4] != VERSION {
            return Err(ProtocolError::BadVersion);
        }
        if frame[5] as usize != HEADER_LEN {
            return Err(ProtocolError::BadHeaderLength);
        }

        let codec = Codec::from_u8(frame[6]).ok_or(ProtocolError::BadCodec)?;
        let payload_len = u16::from_le_bytes([frame[24], frame[25]]);
        if frame.len() < HEADER_LEN + payload_len as usize {
            return Err(ProtocolError::FrameTooShort);
        }

        Ok(Self {
            codec,
            channels: frame[7],
            sample_rate: u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]),
            sequence: u32::from_le_bytes([frame[12], frame[13], frame[14], frame[15]]),
            timestamp_us: u64::from_le_bytes([
                frame[16], frame[17], frame[18], frame[19], frame[20], frame[21], frame[22],
                frame[23],
            ]),
            payload_len,
            flags: u16::from_le_bytes([frame[26], frame[27]]),
            stream_id: u32::from_le_bytes([frame[28], frame[29], frame[30], frame[31]]),
        })
    }

    pub fn payload<'a>(&self, frame: &'a [u8]) -> Result<&'a [u8], ProtocolError> {
        if frame.len() < HEADER_LEN + self.payload_len as usize {
            return Err(ProtocolError::FrameTooShort);
        }
        Ok(&frame[HEADER_LEN..HEADER_LEN + self.payload_len as usize])
    }
}

pub fn discovery_response_url(payload: &str) -> Option<&str> {
    payload
        .strip_prefix(DISCOVERY_RESPONSE_PREFIX)
        .and_then(|response| response.split_ascii_whitespace().next())
}

pub fn discovery_response_priority(payload: &str) -> u8 {
    payload
        .strip_prefix(DISCOVERY_RESPONSE_PREFIX)
        .map(metadata_priority)
        .unwrap_or(RECEIVER_PRIORITY_LEGACY)
}

pub fn parse_control_command(payload: &[u8]) -> Option<ControlCommand> {
    let payload = core::str::from_utf8(payload).ok()?;
    let mut parts = payload.split_ascii_whitespace();
    let action = match parts.next()? {
        value if value.as_bytes() == CONTROL_MODE_USB => ControlAction::SetMode(ControlMode::Usb),
        value
            if value.as_bytes() == CONTROL_MODE_WIFI
                || value.as_bytes() == CONTROL_MODE_WIRELESS =>
        {
            ControlAction::SetMode(ControlMode::Wifi)
        }
        value if value.as_bytes() == CONTROL_MODE_BLE => {
            ControlAction::SetMode(ControlMode::Bluetooth)
        }
        value if value.as_bytes() == CONTROL_RECORD_START => ControlAction::RecordStart,
        value if value.as_bytes() == CONTROL_RECORD_STOP => ControlAction::RecordStop,
        _ => return None,
    };

    Some(ControlCommand {
        action,
        priority: metadata_priority(payload).max(CONTROL_PRIORITY_LEGACY),
    })
}

fn metadata_priority(payload: &str) -> u8 {
    payload
        .split_ascii_whitespace()
        .find_map(|part| part.strip_prefix("priority=").and_then(parse_u8))
        .unwrap_or(0)
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BleAudioFragmentHeader {
    pub frame_sequence: u32,
    pub fragment_index: u8,
    pub fragment_count: u8,
    pub payload_len: u16,
}

impl BleAudioFragmentHeader {
    pub fn new(
        frame_sequence: u32,
        fragment_index: u8,
        fragment_count: u8,
        payload_len: u16,
    ) -> Result<Self, ProtocolError> {
        if fragment_count == 0 || fragment_index >= fragment_count {
            return Err(ProtocolError::BadFragmentIndex);
        }
        Ok(Self {
            frame_sequence,
            fragment_index,
            fragment_count,
            payload_len,
        })
    }

    pub fn encode_into(&self, out: &mut [u8]) -> Result<(), ProtocolError> {
        if out.len() < BLE_AUDIO_FRAGMENT_HEADER_LEN {
            return Err(ProtocolError::BufferTooSmall);
        }

        out[0..4].copy_from_slice(&self.frame_sequence.to_le_bytes());
        out[4] = self.fragment_index;
        out[5] = self.fragment_count;
        out[6..8].copy_from_slice(&self.payload_len.to_le_bytes());
        Ok(())
    }

    pub fn decode(fragment: &[u8]) -> Result<Self, ProtocolError> {
        if fragment.len() < BLE_AUDIO_FRAGMENT_HEADER_LEN {
            return Err(ProtocolError::FrameTooShort);
        }

        let header = Self {
            frame_sequence: u32::from_le_bytes([
                fragment[0],
                fragment[1],
                fragment[2],
                fragment[3],
            ]),
            fragment_index: fragment[4],
            fragment_count: fragment[5],
            payload_len: u16::from_le_bytes([fragment[6], fragment[7]]),
        };
        if header.fragment_count == 0 || header.fragment_index >= header.fragment_count {
            return Err(ProtocolError::BadFragmentIndex);
        }
        if fragment.len() < BLE_AUDIO_FRAGMENT_HEADER_LEN + header.payload_len as usize {
            return Err(ProtocolError::FrameTooShort);
        }
        Ok(header)
    }

    pub fn payload<'a>(&self, fragment: &'a [u8]) -> Result<&'a [u8], ProtocolError> {
        if fragment.len() < BLE_AUDIO_FRAGMENT_HEADER_LEN + self.payload_len as usize {
            return Err(ProtocolError::FrameTooShort);
        }
        Ok(&fragment[BLE_AUDIO_FRAGMENT_HEADER_LEN
            ..BLE_AUDIO_FRAGMENT_HEADER_LEN + self.payload_len as usize])
    }
}

pub const fn ble_audio_fragment_payload_capacity(notification_payload_len: usize) -> usize {
    notification_payload_len.saturating_sub(BLE_AUDIO_FRAGMENT_HEADER_LEN)
}

pub const fn ima_adpcm4_encoded_len(pcm_s16le_len: usize) -> usize {
    let samples = pcm_s16le_len / 2;
    samples.div_ceil(IMA_ADPCM4_SAMPLES_PER_BYTE)
}

pub fn ima_adpcm4_encode(
    pcm_s16le: &[u8],
    out: &mut [u8],
    state: &mut ImaAdpcmState,
) -> Result<usize, ProtocolError> {
    if pcm_s16le.len() % 2 != 0 {
        return Err(ProtocolError::InvalidPcmLength);
    }

    let encoded_len = ima_adpcm4_encoded_len(pcm_s16le.len());
    if out.len() < encoded_len {
        return Err(ProtocolError::OutputTooSmall);
    }

    for byte in out.iter_mut().take(encoded_len) {
        *byte = 0;
    }

    for (sample_index, sample) in pcm_s16le.chunks_exact(2).enumerate() {
        let sample = i16::from_le_bytes([sample[0], sample[1]]);
        let nibble = ima_adpcm4_encode_sample(sample, state);
        let out_index = sample_index / IMA_ADPCM4_SAMPLES_PER_BYTE;
        if sample_index % IMA_ADPCM4_SAMPLES_PER_BYTE == 0 {
            out[out_index] = nibble;
        } else {
            out[out_index] |= nibble << 4;
        }
    }

    Ok(encoded_len)
}

pub fn ima_adpcm4_decode(
    adpcm: &[u8],
    sample_count: usize,
    out_pcm_s16le: &mut [u8],
    state: &mut ImaAdpcmState,
) -> Result<usize, ProtocolError> {
    if sample_count > adpcm.len() * IMA_ADPCM4_SAMPLES_PER_BYTE {
        return Err(ProtocolError::BadAdpcmSampleCount);
    }

    let decoded_len = sample_count * 2;
    if out_pcm_s16le.len() < decoded_len {
        return Err(ProtocolError::OutputTooSmall);
    }

    for sample_index in 0..sample_count {
        let encoded = adpcm[sample_index / IMA_ADPCM4_SAMPLES_PER_BYTE];
        let nibble = if sample_index % IMA_ADPCM4_SAMPLES_PER_BYTE == 0 {
            encoded & 0x0f
        } else {
            encoded >> 4
        };
        let sample = ima_adpcm4_decode_sample(nibble, state);
        let out_index = sample_index * 2;
        out_pcm_s16le[out_index..out_index + 2].copy_from_slice(&sample.to_le_bytes());
    }

    Ok(decoded_len)
}

fn ima_adpcm4_encode_sample(sample: i16, state: &mut ImaAdpcmState) -> u8 {
    let step = IMA_ADPCM_STEP_TABLE[state.step_index as usize] as i32;
    let mut diff = sample as i32 - state.predictor as i32;
    let mut nibble = 0u8;

    if diff < 0 {
        nibble |= 0x08;
        diff = -diff;
    }

    let mut temp_step = step;
    if diff >= temp_step {
        nibble |= 0x04;
        diff -= temp_step;
    }
    temp_step >>= 1;
    if diff >= temp_step {
        nibble |= 0x02;
        diff -= temp_step;
    }
    temp_step >>= 1;
    if diff >= temp_step {
        nibble |= 0x01;
    }

    let _ = ima_adpcm4_decode_sample(nibble, state);
    nibble
}

fn ima_adpcm4_decode_sample(nibble: u8, state: &mut ImaAdpcmState) -> i16 {
    let nibble = nibble & 0x0f;
    let step = IMA_ADPCM_STEP_TABLE[state.step_index as usize] as i32;
    let mut diff = step >> 3;

    if nibble & 0x04 != 0 {
        diff += step;
    }
    if nibble & 0x02 != 0 {
        diff += step >> 1;
    }
    if nibble & 0x01 != 0 {
        diff += step >> 2;
    }

    let predictor = if nibble & 0x08 != 0 {
        state.predictor as i32 - diff
    } else {
        state.predictor as i32 + diff
    };
    state.predictor = predictor.clamp(i16::MIN as i32, i16::MAX as i32) as i16;

    let next_index = state.step_index as i16 + IMA_ADPCM_INDEX_TABLE[nibble as usize] as i16;
    state.step_index = next_index.clamp(0, (IMA_ADPCM_STEP_TABLE.len() - 1) as i16) as u8;

    state.predictor
}

const IMA_ADPCM_INDEX_TABLE: [i8; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

const IMA_ADPCM_STEP_TABLE: [i16; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_round_trips_stream_metadata() {
        let header = AudioFrameHeader::new(
            Codec::PcmS16Le,
            1,
            16_000,
            42,
            123_456,
            640,
            0xaabb_ccdd,
            FLAG_STREAM_START | FLAG_PUSH_TO_TALK,
        );
        let mut encoded = [0u8; HEADER_LEN + 640];

        header.encode_into(&mut encoded).unwrap();
        let decoded = AudioFrameHeader::decode(&encoded).unwrap();

        assert_eq!(decoded, header);
        assert!(decoded.is_stream_start());
        assert!(!decoded.is_stream_end());
        assert!(decoded.is_push_to_talk());
    }

    #[test]
    fn frame_header_round_trips_adpcm_codec() {
        let header = AudioFrameHeader::new(
            Codec::ImaAdpcm4,
            1,
            16_000,
            7,
            88_000,
            320,
            0x1234_5678,
            FLAG_STREAM_START,
        );
        let mut encoded = [0u8; HEADER_LEN + 320];

        header.encode_into(&mut encoded).unwrap();
        let decoded = AudioFrameHeader::decode(&encoded).unwrap();

        assert_eq!(decoded, header);
        assert_eq!(decoded.codec.as_str(), "ima_adpcm4");
    }

    #[test]
    fn ima_adpcm4_encodes_40ms_frame_to_quarter_size() {
        let mut pcm = [0u8; 1_280];
        for (index, sample) in pcm.chunks_exact_mut(2).enumerate() {
            let value = (((index as i32 % 128) - 64) * 256) as i16;
            sample.copy_from_slice(&value.to_le_bytes());
        }
        let mut encoded = [0u8; 320];
        let mut state = ImaAdpcmState::new();

        let written = ima_adpcm4_encode(&pcm, &mut encoded, &mut state).unwrap();

        assert_eq!(written, 320);
    }

    #[test]
    fn ima_adpcm4_round_trip_tracks_signal_shape() {
        let mut pcm = [0u8; 1_280];
        for (index, sample) in pcm.chunks_exact_mut(2).enumerate() {
            let phase = index as i32 % 160;
            let value = if phase < 80 {
                -16_000 + phase * 400
            } else {
                16_000 - (phase - 80) * 400
            } as i16;
            sample.copy_from_slice(&value.to_le_bytes());
        }

        let mut encoded = [0u8; 320];
        let mut encode_state = ImaAdpcmState::new();
        let written = ima_adpcm4_encode(&pcm, &mut encoded, &mut encode_state).unwrap();

        let mut decoded = [0u8; 1_280];
        let mut decode_state = ImaAdpcmState::new();
        let decoded_len =
            ima_adpcm4_decode(&encoded[..written], 640, &mut decoded, &mut decode_state).unwrap();

        assert_eq!(decoded_len, pcm.len());
        let mut absolute_error_sum = 0u64;
        for (expected, actual) in pcm.chunks_exact(2).zip(decoded.chunks_exact(2)) {
            let expected = i16::from_le_bytes([expected[0], expected[1]]) as i32;
            let actual = i16::from_le_bytes([actual[0], actual[1]]) as i32;
            absolute_error_sum += expected.abs_diff(actual) as u64;
        }
        let mean_absolute_error = absolute_error_sum / 640;
        assert!(mean_absolute_error < 2_500);
    }

    #[test]
    fn ima_adpcm4_streaming_state_matches_one_pass() {
        let mut pcm = [0u8; 1_280];
        for (index, sample) in pcm.chunks_exact_mut(2).enumerate() {
            let value = ((index as i32 * 97 % 40_000) - 20_000) as i16;
            sample.copy_from_slice(&value.to_le_bytes());
        }

        let mut one_pass = [0u8; 320];
        let mut one_pass_state = ImaAdpcmState::new();
        ima_adpcm4_encode(&pcm, &mut one_pass, &mut one_pass_state).unwrap();

        let mut split = [0u8; 320];
        let mut split_state = ImaAdpcmState::new();
        let first = ima_adpcm4_encode(&pcm[..640], &mut split[..160], &mut split_state).unwrap();
        let second = ima_adpcm4_encode(&pcm[640..], &mut split[160..], &mut split_state).unwrap();

        assert_eq!(first, 160);
        assert_eq!(second, 160);
        assert_eq!(split, one_pass);
        assert_eq!(split_state, one_pass_state);
    }

    #[test]
    fn ble_audio_fragment_header_round_trips() {
        let header = BleAudioFragmentHeader::new(99, 1, 3, 12).unwrap();
        let mut encoded = [0u8; BLE_AUDIO_FRAGMENT_HEADER_LEN + 12];
        encoded[BLE_AUDIO_FRAGMENT_HEADER_LEN..].copy_from_slice(b"hello world!");

        header.encode_into(&mut encoded).unwrap();
        let decoded = BleAudioFragmentHeader::decode(&encoded).unwrap();

        assert_eq!(decoded, header);
        assert_eq!(decoded.payload(&encoded).unwrap(), b"hello world!");
    }

    #[test]
    fn ble_audio_fragment_capacity_accounts_for_header() {
        assert_eq!(
            ble_audio_fragment_payload_capacity(185),
            185 - BLE_AUDIO_FRAGMENT_HEADER_LEN
        );
        assert_eq!(ble_audio_fragment_payload_capacity(4), 0);
    }

    #[test]
    fn discovery_response_uses_first_token_as_url() {
        let payload = "M5MIC_SERVER_V1 ws://10.0.0.5:47776/audio source=ios priority=100\n";

        assert_eq!(
            discovery_response_url(payload),
            Some("ws://10.0.0.5:47776/audio")
        );
        assert_eq!(
            discovery_response_priority(payload),
            RECEIVER_PRIORITY_PHONE
        );
    }

    #[test]
    fn control_command_accepts_priority_metadata() {
        let command = parse_control_command(b"M5MIC_MODE_WIFI_V1 source=ios priority=100").unwrap();

        assert_eq!(
            command,
            ControlCommand {
                action: ControlAction::SetMode(ControlMode::Wifi),
                priority: CONTROL_PRIORITY_PHONE
            }
        );
    }

    #[test]
    fn legacy_control_command_gets_low_priority() {
        let command = parse_control_command(CONTROL_MODE_USB).unwrap();

        assert_eq!(
            command,
            ControlCommand {
                action: ControlAction::SetMode(ControlMode::Usb),
                priority: CONTROL_PRIORITY_LEGACY
            }
        );
    }

    #[test]
    fn record_control_command_accepts_priority_metadata() {
        let command =
            parse_control_command(b"M5MIC_RECORD_START_V1 source=ios priority=100").unwrap();

        assert_eq!(
            command,
            ControlCommand {
                action: ControlAction::RecordStart,
                priority: CONTROL_PRIORITY_PHONE
            }
        );
        assert_eq!(
            parse_control_command(CONTROL_RECORD_STOP).unwrap(),
            ControlCommand {
                action: ControlAction::RecordStop,
                priority: CONTROL_PRIORITY_LEGACY
            }
        );
    }
}
