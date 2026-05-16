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

pub const MAGIC: [u8; 4] = *b"M5AU";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 32;
pub const FLAG_STREAM_START: u16 = 0x0001;
pub const FLAG_STREAM_END: u16 = 0x0002;
pub const FLAG_PUSH_TO_TALK: u16 = 0x0004;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub enum Codec {
    PcmS16Le = 1,
}

impl Codec {
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::PcmS16Le),
            _ => None,
        }
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
        .map(str::trim)
}

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
}
