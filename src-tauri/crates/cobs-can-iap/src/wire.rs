use crc::{Crc, CRC_16_XMODEM};
use thiserror::Error;

const CRC16: Crc<u16> = Crc::<u16>::new(&CRC_16_XMODEM);
pub const MAX_DATA_LEN: usize = 256;
const FRAME_OVERHEAD: usize = 6;
const MAX_RAW_LEN: usize = FRAME_OVERHEAD + MAX_DATA_LEN;
const MAX_ENCODED_LEN: usize = MAX_RAW_LEN + (MAX_RAW_LEN / 254) + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FunctionCode {
    ResetRequest = 0x80,
    EnterIapRequest = 0x81,
    StartDownloadRequest = 0x82,
    SegmentARequest = 0x83,
    SegmentBRequest = 0x84,
    FinalDownloadRequest = 0x85,
    VerifyRequest = 0x86,
    ResetAck = 0x90,
    EnterIapAck = 0x91,
    StartDownloadAck = 0x92,
    SegmentAAck = 0x93,
    SegmentBAck = 0x94,
    FinalDownloadAck = 0x95,
    VerifyAck = 0x96,
}

impl FunctionCode {
    pub fn expected_ack(self) -> Option<Self> {
        match self {
            Self::ResetRequest => Some(Self::ResetAck),
            Self::EnterIapRequest => Some(Self::EnterIapAck),
            Self::StartDownloadRequest => Some(Self::StartDownloadAck),
            Self::SegmentARequest => Some(Self::SegmentAAck),
            Self::SegmentBRequest => Some(Self::SegmentBAck),
            Self::FinalDownloadRequest => Some(Self::FinalDownloadAck),
            Self::VerifyRequest => Some(Self::VerifyAck),
            _ => None,
        }
    }
}

impl TryFrom<u8> for FunctionCode {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x80 => Ok(Self::ResetRequest),
            0x81 => Ok(Self::EnterIapRequest),
            0x82 => Ok(Self::StartDownloadRequest),
            0x83 => Ok(Self::SegmentARequest),
            0x84 => Ok(Self::SegmentBRequest),
            0x85 => Ok(Self::FinalDownloadRequest),
            0x86 => Ok(Self::VerifyRequest),
            0x90 => Ok(Self::ResetAck),
            0x91 => Ok(Self::EnterIapAck),
            0x92 => Ok(Self::StartDownloadAck),
            0x93 => Ok(Self::SegmentAAck),
            0x94 => Ok(Self::SegmentBAck),
            0x95 => Ok(Self::FinalDownloadAck),
            0x96 => Ok(Self::VerifyAck),
            other => Err(other),
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FrameError {
    #[error("node ID {0} is outside 1..=127")]
    InvalidNode(u8),
    #[error("function {0:?} is not a request")]
    NotARequest(FunctionCode),
    #[error("frame data is {actual} bytes, above {maximum}")]
    DataTooLarge { actual: usize, maximum: usize },
    #[error("decoded frame is {0} bytes, below the minimum")]
    TooShort(usize),
    #[error("unknown function code {0:#04x}")]
    UnknownFunction(u8),
    #[error("frame length field is {declared}, but payload is {actual}")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("CRC16 mismatch: received {received:#06x}, computed {computed:#06x}")]
    CrcMismatch { received: u16, computed: u16 },
    #[error("invalid COBS encoding")]
    InvalidCobs,
    #[error("encoded frame exceeded the {maximum}-byte bound before its delimiter")]
    EncodedTooLong { maximum: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    node_id: u8,
    function: FunctionCode,
    data: Vec<u8>,
}

impl Frame {
    pub fn new(
        node_id: u8,
        function: FunctionCode,
        data: impl Into<Vec<u8>>,
    ) -> Result<Self, FrameError> {
        if !(1..=127).contains(&node_id) {
            return Err(FrameError::InvalidNode(node_id));
        }
        let data = data.into();
        if data.len() > MAX_DATA_LEN {
            return Err(FrameError::DataTooLarge {
                actual: data.len(),
                maximum: MAX_DATA_LEN,
            });
        }
        Ok(Self {
            node_id,
            function,
            data,
        })
    }

    pub fn request(
        node_id: u8,
        function: FunctionCode,
        data: impl Into<Vec<u8>>,
    ) -> Result<Self, FrameError> {
        if function.expected_ack().is_none() {
            return Err(FrameError::NotARequest(function));
        }
        Self::new(node_id, function, data)
    }

    pub fn node_id(&self) -> u8 {
        self.node_id
    }

    pub fn function(&self) -> FunctionCode {
        self.function
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut raw = Vec::with_capacity(FRAME_OVERHEAD + self.data.len());
        raw.push(self.node_id);
        raw.push(self.function as u8);
        raw.extend_from_slice(&(self.data.len() as u16).to_le_bytes());
        raw.extend_from_slice(&self.data);
        raw.extend_from_slice(&CRC16.checksum(&raw).to_le_bytes());
        let mut encoded = cobs_encode(&raw);
        encoded.push(0);
        encoded
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, FrameError> {
        let encoded = encoded.strip_suffix(&[0]).unwrap_or(encoded);
        let raw = cobs_decode(encoded)?;
        Self::decode_raw(&raw)
    }

    fn decode_raw(raw: &[u8]) -> Result<Self, FrameError> {
        if raw.len() < FRAME_OVERHEAD {
            return Err(FrameError::TooShort(raw.len()));
        }
        let node_id = raw[0];
        if !(1..=127).contains(&node_id) {
            return Err(FrameError::InvalidNode(node_id));
        }
        let function = FunctionCode::try_from(raw[1]).map_err(FrameError::UnknownFunction)?;
        let declared = u16::from_le_bytes([raw[2], raw[3]]) as usize;
        if declared > MAX_DATA_LEN {
            return Err(FrameError::DataTooLarge {
                actual: declared,
                maximum: MAX_DATA_LEN,
            });
        }
        let expected = FRAME_OVERHEAD + declared;
        if raw.len() != expected {
            return Err(FrameError::LengthMismatch {
                declared,
                actual: raw.len().saturating_sub(FRAME_OVERHEAD),
            });
        }
        let crc_offset = 4 + declared;
        let received = u16::from_le_bytes([raw[crc_offset], raw[crc_offset + 1]]);
        let computed = CRC16.checksum(&raw[..crc_offset]);
        if received != computed {
            return Err(FrameError::CrcMismatch { received, computed });
        }
        Ok(Self {
            node_id,
            function,
            data: raw[4..crc_offset].to_vec(),
        })
    }
}

fn cobs_encode(raw: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(raw.len() + (raw.len() / 254) + 1);
    let mut code_index = 0;
    let mut code = 1u8;
    encoded.push(0);
    for &byte in raw {
        if byte == 0 {
            encoded[code_index] = code;
            code_index = encoded.len();
            encoded.push(0);
            code = 1;
            continue;
        }
        encoded.push(byte);
        code += 1;
        if code == 0xFF {
            encoded[code_index] = code;
            code_index = encoded.len();
            encoded.push(0);
            code = 1;
        }
    }
    encoded[code_index] = code;
    encoded
}

fn cobs_decode(encoded: &[u8]) -> Result<Vec<u8>, FrameError> {
    if encoded.is_empty() {
        return Err(FrameError::InvalidCobs);
    }
    let mut raw = Vec::with_capacity(encoded.len());
    let mut offset = 0;
    while offset < encoded.len() {
        let code = encoded[offset];
        if code == 0 {
            return Err(FrameError::InvalidCobs);
        }
        offset += 1;
        let data_len = code as usize - 1;
        let end = offset
            .checked_add(data_len)
            .filter(|end| *end <= encoded.len())
            .ok_or(FrameError::InvalidCobs)?;
        raw.extend_from_slice(&encoded[offset..end]);
        offset = end;
        if code != 0xFF && offset < encoded.len() {
            raw.push(0);
        }
    }
    Ok(raw)
}

#[derive(Debug, Default)]
pub struct FrameAssembler {
    encoded: Vec<u8>,
    discarding: bool,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            encoded: Vec::with_capacity(MAX_ENCODED_LEN),
            discarding: false,
        }
    }

    /// Consume all bytes, preserving frames after a delimiter in the same CAN payload.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Result<Frame, FrameError>> {
        let mut results = Vec::new();
        for &byte in bytes {
            if byte == 0 {
                if self.discarding {
                    self.discarding = false;
                    self.encoded.clear();
                    continue;
                }
                if !self.encoded.is_empty() {
                    results.push(Frame::decode(&self.encoded));
                    self.encoded.clear();
                }
                continue;
            }
            if self.discarding {
                continue;
            }
            if self.encoded.len() == MAX_ENCODED_LEN {
                self.encoded.clear();
                self.discarding = true;
                results.push(Err(FrameError::EncodedTooLong {
                    maximum: MAX_ENCODED_LEN,
                }));
                continue;
            }
            self.encoded.push(byte);
        }
        results
    }

    pub fn reset(&mut self) {
        self.encoded.clear();
        self.discarding = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_captured_enter_ack() {
        let wire = hex::decode("0401910c020108aaaa2510252008010103064800").unwrap();
        let frame = Frame::decode(&wire).unwrap();
        assert_eq!(frame.node_id(), 1);
        assert_eq!(frame.function(), FunctionCode::EnterIapAck);
        assert_eq!(
            frame.data(),
            hex::decode("0100aaaa2510252008000000").unwrap()
        );
    }

    #[test]
    fn round_trips_maximum_frame() {
        let frame = Frame::request(
            127,
            FunctionCode::SegmentARequest,
            (0..=255).collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(Frame::decode(&frame.encode()).unwrap(), frame);
    }

    #[test]
    fn assembler_consumes_two_frames_in_one_payload() {
        let first = Frame::new(1, FunctionCode::ResetAck, [1]).unwrap();
        let second = Frame::new(1, FunctionCode::VerifyAck, [1]).unwrap();
        let bytes = [first.encode(), second.encode()].concat();
        let results = FrameAssembler::new().feed(&bytes);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap(), &first);
        assert_eq!(results[1].as_ref().unwrap(), &second);
    }

    #[test]
    fn assembler_bounds_missing_delimiter() {
        let mut assembler = FrameAssembler::new();
        let results = assembler.feed(&vec![1; MAX_ENCODED_LEN + 1]);
        assert_eq!(
            results,
            vec![Err(FrameError::EncodedTooLong {
                maximum: MAX_ENCODED_LEN
            })]
        );
        assert!(assembler.feed(&[2, 3, 0]).is_empty());
    }
}
