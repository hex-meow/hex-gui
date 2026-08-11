use sha2::{Digest, Sha256};
use thiserror::Error;

pub const IMG_TAG_SIZE: usize = 140;
const MIN_BIN_SIZE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMode {
    Plaintext,
    Encrypted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImgLimits {
    pub max_file_bytes: usize,
    pub max_bin_bytes: usize,
}

impl Default for ImgLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 2 * 1024 * 1024,
            max_bin_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArtifactError {
    #[error("IMG file is {actual} bytes, below the {minimum}-byte minimum")]
    TooSmall { actual: usize, minimum: usize },
    #[error("IMG file is {actual} bytes, above the {maximum}-byte limit")]
    FileTooLarge { actual: usize, maximum: usize },
    #[error("IMG bin is {actual} bytes, above the {maximum}-byte limit")]
    BinTooLarge { actual: usize, maximum: usize },
    #[error("IMG size arithmetic overflow")]
    SizeOverflow,
    #[error("IMG declares {declared} bin bytes but the exact file size implies {actual}")]
    SizeMismatch { declared: usize, actual: usize },
    #[error("IMG encryption flag {0:#010x} is not 0 or 1")]
    InvalidEncryptionFlag(u32),
    #[error("IMG plaintext mode requires an all-zero signature and IV")]
    PlaintextMetadataMismatch,
    #[error("IMG encrypted mode requires non-zero signature and IV fields")]
    EncryptedMetadataMissing,
    #[error("IMG SHA-256 does not match its protected fields and bin data")]
    HashMismatch,
    #[error("IMG bin is {0} bytes; at least four bytes are required")]
    BinTooSmall(usize),
    #[error("IMG address arithmetic overflow")]
    AddressOverflow,
    #[error("IMG inclusive end address is {actual:#010x}, expected {expected:#010x} from start + size - 1")]
    AddressSizeMismatch { expected: u32, actual: u32 },
}

#[derive(Clone)]
pub struct ImgArtifact {
    raw: Vec<u8>,
    device_id: u32,
    firmware_id: u32,
    firmware_version: u32,
    encryption: EncryptionMode,
    hash: [u8; 32],
    signature: [u8; 64],
    iv: [u8; 16],
    start_address: u32,
    end_address: u32,
    bin_size: usize,
}

impl std::fmt::Debug for ImgArtifact {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImgArtifact")
            .field("device_id", &format_args!("{:#010x}", self.device_id))
            .field("firmware_id", &format_args!("{:#010x}", self.firmware_id))
            .field(
                "firmware_version",
                &format_args!("{:#010x}", self.firmware_version),
            )
            .field("encryption", &self.encryption)
            .field(
                "start_address",
                &format_args!("{:#010x}", self.start_address),
            )
            .field("end_address", &format_args!("{:#010x}", self.end_address))
            .field("bin_size", &self.bin_size)
            .finish_non_exhaustive()
    }
}

impl ImgArtifact {
    pub fn parse(bytes: &[u8], limits: ImgLimits) -> Result<Self, ArtifactError> {
        let minimum = IMG_TAG_SIZE + MIN_BIN_SIZE;
        if bytes.len() < minimum {
            return Err(ArtifactError::TooSmall {
                actual: bytes.len(),
                minimum,
            });
        }
        if bytes.len() > limits.max_file_bytes {
            return Err(ArtifactError::FileTooLarge {
                actual: bytes.len(),
                maximum: limits.max_file_bytes,
            });
        }

        let device_id = read_u32(bytes, 0);
        let firmware_id = read_u32(bytes, 4);
        let firmware_version = read_u32(bytes, 8);
        let encryption = match read_u32(bytes, 12) {
            0 => EncryptionMode::Plaintext,
            1 => EncryptionMode::Encrypted,
            other => return Err(ArtifactError::InvalidEncryptionFlag(other)),
        };
        let hash: [u8; 32] = bytes[16..48].try_into().expect("fixed slice");
        let signature: [u8; 64] = bytes[48..112].try_into().expect("fixed slice");
        let iv: [u8; 16] = bytes[112..128].try_into().expect("fixed slice");
        let start_address = read_u32(bytes, 128);
        let end_address = read_u32(bytes, 132);
        let bin_size = read_u32(bytes, 136) as usize;

        if bin_size < MIN_BIN_SIZE {
            return Err(ArtifactError::BinTooSmall(bin_size));
        }
        if bin_size > limits.max_bin_bytes {
            return Err(ArtifactError::BinTooLarge {
                actual: bin_size,
                maximum: limits.max_bin_bytes,
            });
        }
        let expected_size = IMG_TAG_SIZE
            .checked_add(bin_size)
            .ok_or(ArtifactError::SizeOverflow)?;
        if bytes.len() != expected_size {
            return Err(ArtifactError::SizeMismatch {
                declared: bin_size,
                actual: bytes.len() - IMG_TAG_SIZE,
            });
        }

        match encryption {
            EncryptionMode::Plaintext
                if signature.iter().any(|byte| *byte != 0) || iv.iter().any(|byte| *byte != 0) =>
            {
                return Err(ArtifactError::PlaintextMetadataMismatch)
            }
            EncryptionMode::Encrypted
                if signature.iter().all(|byte| *byte == 0) || iv.iter().all(|byte| *byte == 0) =>
            {
                return Err(ArtifactError::EncryptedMetadataMissing)
            }
            _ => {}
        }

        let expected_end = start_address
            .checked_add(u32::try_from(bin_size - 1).map_err(|_| ArtifactError::AddressOverflow)?)
            .ok_or(ArtifactError::AddressOverflow)?;
        if end_address != expected_end {
            return Err(ArtifactError::AddressSizeMismatch {
                expected: expected_end,
                actual: end_address,
            });
        }

        let mut hasher = Sha256::new();
        hasher.update(&bytes[0..16]);
        hasher.update(&bytes[112..128]);
        hasher.update(&bytes[128..IMG_TAG_SIZE]);
        hasher.update(&bytes[IMG_TAG_SIZE..]);
        let actual_hash: [u8; 32] = hasher.finalize().into();
        if actual_hash != hash {
            return Err(ArtifactError::HashMismatch);
        }

        Ok(Self {
            raw: bytes.to_vec(),
            device_id,
            firmware_id,
            firmware_version,
            encryption,
            hash,
            signature,
            iv,
            start_address,
            end_address,
            bin_size,
        })
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn tag(&self) -> &[u8] {
        &self.raw[..IMG_TAG_SIZE]
    }

    pub fn bin(&self) -> &[u8] {
        &self.raw[IMG_TAG_SIZE..]
    }

    pub fn device_id(&self) -> u32 {
        self.device_id
    }

    pub fn firmware_id(&self) -> u32 {
        self.firmware_id
    }

    pub fn firmware_version(&self) -> u32 {
        self.firmware_version
    }

    pub fn encryption(&self) -> EncryptionMode {
        self.encryption
    }

    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }

    pub fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    pub fn iv(&self) -> &[u8; 16] {
        &self.iv
    }

    pub fn start_address(&self) -> u32 {
        self.start_address
    }

    pub fn end_address(&self) -> u32 {
        self.end_address
    }

    pub fn bin_size(&self) -> usize {
        self.bin_size
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed slice"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn make_image(
        device_id: u32,
        firmware_id: u32,
        start: u32,
        bin_len: usize,
        encrypted: bool,
    ) -> Vec<u8> {
        let mut bytes = vec![0u8; IMG_TAG_SIZE + bin_len];
        bytes[0..4].copy_from_slice(&device_id.to_le_bytes());
        bytes[4..8].copy_from_slice(&firmware_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&8u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&(encrypted as u32).to_le_bytes());
        if encrypted {
            bytes[48..112].fill(0x5A);
            bytes[112..128].fill(0xA5);
        }
        bytes[128..132].copy_from_slice(&start.to_le_bytes());
        let end = start + bin_len as u32 - 1;
        bytes[132..136].copy_from_slice(&end.to_le_bytes());
        bytes[136..140].copy_from_slice(&(bin_len as u32).to_le_bytes());
        for (index, byte) in bytes[IMG_TAG_SIZE..].iter_mut().enumerate() {
            *byte = index.wrapping_mul(17) as u8;
        }
        let mut hasher = Sha256::new();
        hasher.update(&bytes[0..16]);
        hasher.update(&bytes[112..128]);
        hasher.update(&bytes[128..IMG_TAG_SIZE]);
        hasher.update(&bytes[IMG_TAG_SIZE..]);
        bytes[16..48].copy_from_slice(&hasher.finalize());
        bytes
    }

    #[test]
    fn parses_strict_encrypted_image() {
        let bytes = make_image(0xAAAA_0001, 0x2025_1025, 0x1000_C000, 260, true);
        let image = ImgArtifact::parse(&bytes, ImgLimits::default()).unwrap();
        assert_eq!(image.device_id(), 0xAAAA_0001);
        assert_eq!(image.firmware_id(), 0x2025_1025);
        assert_eq!(image.end_address(), 0x1000_C103);
        assert_eq!(image.encryption(), EncryptionMode::Encrypted);
    }

    #[test]
    fn rejects_hash_change() {
        let mut bytes = make_image(1, 2, 0x1000, 16, true);
        *bytes.last_mut().unwrap() ^= 1;
        assert_eq!(
            ImgArtifact::parse(&bytes, ImgLimits::default()).unwrap_err(),
            ArtifactError::HashMismatch
        );
    }

    #[test]
    fn rejects_inconsistent_inclusive_end() {
        let mut bytes = make_image(1, 2, 0x1000, 16, true);
        bytes[132..136].copy_from_slice(&0x1010u32.to_le_bytes());
        assert!(matches!(
            ImgArtifact::parse(&bytes, ImgLimits::default()),
            Err(ArtifactError::AddressSizeMismatch { .. })
        ));
    }

    #[test]
    fn rejects_unknown_encryption_flag() {
        let mut bytes = make_image(1, 2, 0x1000, 16, true);
        bytes[12..16].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            ImgArtifact::parse(&bytes, ImgLimits::default()).unwrap_err(),
            ArtifactError::InvalidEncryptionFlag(2)
        );
    }
}
