use std::collections::HashMap;
use std::io::{Cursor, Read};

use image_container::{
    Header, FORMAT_VERSION_V1, FORMAT_VERSION_V2, TARGET_MCU_G0B1, V2_RECORD_PLAIN_SIZE,
    V2_RECORD_TAG_SIZE, VENDOR_ID,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const FORMAT: &str = "meow-fw/1";
pub const MCU_STM32G431: &str = "stm32g431";
pub const MCU_STM32G474: &str = "stm32g474";
pub const MCU_STM32G0B1: &str = "stm32g0b1";

const MEMBER_MANIFEST: &str = "manifest.json";
const MEMBER_IMAGE: &str = "image.bin";
const MEMBER_HEADER: &str = "header.bin";
const HEADER_LEN: usize = image_container::HEADER_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageLimits {
    pub max_archive_bytes: usize,
    pub max_manifest_bytes: usize,
    pub max_image_bytes: usize,
    pub max_entries: usize,
}

impl Default for PackageLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 4 * 1024 * 1024,
            max_manifest_bytes: 64 * 1024,
            max_image_bytes: 1024 * 1024,
            max_entries: 3,
        }
    }
}

mod hex_u32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{value:08X}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        let value = String::deserialize(deserializer)?;
        let trimmed = value.trim();
        let result = match trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
        {
            Some(hex) => u32::from_str_radix(hex, 16),
            None => trimmed.parse::<u32>(),
        };
        result.map_err(|_| serde::de::Error::custom(format!("invalid u32 {value:?}")))
    }
}

mod hex_u32_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<u32>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_str(&format!("0x{value:08X}")),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u32>, D::Error> {
        let value = Option::<String>::deserialize(deserializer)?;
        value
            .map(|value| {
                let trimmed = value.trim();
                match trimmed
                    .strip_prefix("0x")
                    .or_else(|| trimmed.strip_prefix("0X"))
                {
                    Some(hex) => u32::from_str_radix(hex, 16),
                    None => trimmed.parse::<u32>(),
                }
                .map_err(|_| serde::de::Error::custom(format!("invalid u32 {value:?}")))
            })
            .transpose()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub mcu: String,
    #[serde(with = "hex_u32")]
    pub vendor_id: u32,
    #[serde(with = "hex_u32")]
    pub product_code: u32,
    pub min_hardware_rev: u32,
    pub firmware_id: u32,
    pub firmware_version: u32,
    pub image: ImageMeta,
    pub header: Option<MemberRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<MemberRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pubkey_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_arv: Option<u32>,
    pub payload_format: Option<PayloadFormat>,
    pub tool_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ImageMeta {
    pub member: String,
    pub size: u64,
    pub sha256: String,
    #[serde(with = "hex_u32_opt", default, skip_serializing_if = "Option::is_none")]
    pub crc32: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct MemberRef {
    pub member: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct PayloadFormat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stm32_header_version: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hpm_kn_version: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stm32ImageMode {
    PlaintextV1,
    SignedV2,
    EncryptedV2,
}

/// A bounded package with transport integrity and header/manifest consistency
/// checked.
///
/// For encrypted v2, the package parser deliberately does not decrypt opaque
/// wire records. [`crate::PreparedUpgrade::bind`] authenticates the signed
/// header with the product profile's P-256 public key, and the Bootloader
/// authenticates each AES-GCM record before writing its plaintext.
#[derive(Debug, Clone)]
pub struct IntegrityCheckedPackage {
    manifest: Manifest,
    image: Vec<u8>,
    header: [u8; HEADER_LEN],
    image_mode: Stm32ImageMode,
    plaintext_size: usize,
    wire_size: usize,
}

impl IntegrityCheckedPackage {
    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn image(&self) -> &[u8] {
        &self.image
    }

    pub const fn header(&self) -> &[u8; HEADER_LEN] {
        &self.header
    }

    pub const fn image_mode(&self) -> Stm32ImageMode {
        self.image_mode
    }

    /// Unpadded application bytes described by the validated container.
    pub const fn plaintext_size(&self) -> usize {
        self.plaintext_size
    }

    /// Exact bytes streamed over CAN by this core.
    ///
    /// V1 adds canonical `0xFF` padding to the 8-byte Flash granularity.
    /// Secure v2's `image.bin` already is its canonical wire representation;
    /// encrypted mode includes one detached AES-GCM tag per record.
    pub const fn wire_size(&self) -> usize {
        self.wire_size
    }
}

pub fn read_package_bytes(
    bytes: &[u8],
    limits: PackageLimits,
) -> Result<IntegrityCheckedPackage, PackageError> {
    if limits.max_entries < 3 {
        return Err(PackageError::InvalidLimits(
            "max_entries must permit the three mandatory STM32 members",
        ));
    }
    if bytes.len() > limits.max_archive_bytes {
        return Err(PackageError::ArchiveTooLarge {
            actual: bytes.len(),
            limit: limits.max_archive_bytes,
        });
    }
    if bytes.len() % 512 != 0 {
        return Err(PackageError::InvalidArchive(
            "tar length is not a multiple of 512 bytes".to_owned(),
        ));
    }

    let cursor = Cursor::new(bytes);
    let mut archive = tar::Archive::new(cursor);
    let mut members: HashMap<String, Vec<u8>> = HashMap::new();
    let entries = archive
        .entries()
        .map_err(|error| PackageError::InvalidArchive(error.to_string()))?;

    for (entry_index, entry) in entries.enumerate() {
        if entry_index >= limits.max_entries {
            return Err(PackageError::TooManyEntries {
                limit: limits.max_entries,
            });
        }
        let mut entry = entry.map_err(|error| PackageError::InvalidArchive(error.to_string()))?;
        if !entry.header().entry_type().is_file() {
            return Err(PackageError::NonFileMember);
        }
        let path = entry
            .path()
            .map_err(|error| PackageError::InvalidArchive(error.to_string()))?;
        let name = path.to_str().ok_or(PackageError::NonUtf8Member)?.to_owned();
        let limit = match name.as_str() {
            MEMBER_MANIFEST => limits.max_manifest_bytes,
            MEMBER_IMAGE => limits.max_image_bytes,
            MEMBER_HEADER => HEADER_LEN,
            _ => return Err(PackageError::UnexpectedMember(name)),
        };
        let declared_size: usize =
            entry
                .size()
                .try_into()
                .map_err(|_| PackageError::MemberTooLarge {
                    member: name.clone(),
                    actual: usize::MAX,
                    limit,
                })?;
        if declared_size > limit {
            return Err(PackageError::MemberTooLarge {
                member: name,
                actual: declared_size,
                limit,
            });
        }
        if members.contains_key(&name) {
            return Err(PackageError::DuplicateMember(name));
        }
        let mut data = Vec::with_capacity(declared_size);
        entry
            .read_to_end(&mut data)
            .map_err(|error| PackageError::InvalidArchive(error.to_string()))?;
        if data.len() != declared_size {
            return Err(PackageError::MemberSizeMismatch {
                member: name,
                declared: declared_size,
                actual: data.len(),
            });
        }
        members.insert(name, data);
    }

    let manifest_bytes = members
        .remove(MEMBER_MANIFEST)
        .ok_or(PackageError::MissingMember(MEMBER_MANIFEST))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| PackageError::InvalidManifest(error.to_string()))?;
    validate_manifest_shape(&manifest)?;

    let image = members
        .remove(MEMBER_IMAGE)
        .ok_or(PackageError::MissingMember(MEMBER_IMAGE))?;
    let header_bytes = members
        .remove(MEMBER_HEADER)
        .ok_or(PackageError::MissingMember(MEMBER_HEADER))?;
    debug_assert!(members.is_empty());

    if manifest.image.size != image.len() as u64 {
        return Err(PackageError::ImageSizeMismatch {
            declared: manifest.image.size,
            actual: image.len(),
        });
    }
    let actual_sha256 = sha256_hex(&image);
    if !constant_shape_hex_eq(&manifest.image.sha256, &actual_sha256) {
        return Err(PackageError::ImageSha256Mismatch);
    }
    let declared_crc = manifest
        .image
        .crc32
        .ok_or(PackageError::MissingImageCrc32)?;
    let actual_crc = image_container::image_crc32_of(&image);
    if declared_crc != actual_crc {
        return Err(PackageError::ImageCrc32Mismatch {
            declared: declared_crc,
            actual: actual_crc,
        });
    }

    let header: [u8; HEADER_LEN] =
        header_bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| PackageError::HeaderSize {
                actual: bytes.len(),
            })?;
    let parsed = Header::parse(&header)
        .map_err(|error| PackageError::InvalidHeader(format!("{error:?}")))?;
    let image_mode = cross_check(&manifest, &parsed, &image)?;
    let plaintext_size = parsed.image_size() as usize;
    let wire_size = match image_mode {
        Stm32ImageMode::PlaintextV1 => image.len().div_ceil(8) * 8,
        Stm32ImageMode::SignedV2 | Stm32ImageMode::EncryptedV2 => image.len(),
    };

    Ok(IntegrityCheckedPackage {
        manifest,
        image,
        header,
        image_mode,
        plaintext_size,
        wire_size,
    })
}

fn validate_manifest_shape(manifest: &Manifest) -> Result<(), PackageError> {
    if manifest.format != FORMAT {
        return Err(PackageError::UnknownFormat(manifest.format.clone()));
    }
    if !matches!(
        manifest.mcu.as_str(),
        MCU_STM32G431 | MCU_STM32G474 | MCU_STM32G0B1
    ) {
        return Err(PackageError::UnknownMcu(manifest.mcu.clone()));
    }
    if manifest.vendor_id != VENDOR_ID {
        return Err(PackageError::ForeignVendor(manifest.vendor_id));
    }
    if manifest.product_code == 0xFFFF_FFFF {
        return Err(PackageError::SentinelProduct);
    }
    if manifest.image.member != MEMBER_IMAGE {
        return Err(PackageError::WrongMemberReference {
            field: "image.member",
            expected: MEMBER_IMAGE,
        });
    }
    if manifest
        .header
        .as_ref()
        .map(|member| member.member.as_str())
        != Some(MEMBER_HEADER)
    {
        return Err(PackageError::WrongMemberReference {
            field: "header.member",
            expected: MEMBER_HEADER,
        });
    }
    if manifest.envelope.is_some() {
        return Err(PackageError::UnexpectedEnvelope);
    }
    if manifest.tool_version.trim().is_empty() {
        return Err(PackageError::EmptyToolVersion);
    }
    Ok(())
}

fn cross_check(
    manifest: &Manifest,
    header: &Header,
    image: &[u8],
) -> Result<Stm32ImageMode, PackageError> {
    for (field, header_value, manifest_value) in [
        ("vendor_id", header.vendor_id(), manifest.vendor_id),
        ("product_code", header.product_code(), manifest.product_code),
        (
            "min_hardware_rev",
            header.min_hardware_rev(),
            manifest.min_hardware_rev,
        ),
        ("firmware_id", header.firmware_id(), manifest.firmware_id),
        (
            "firmware_version",
            header.firmware_version(),
            manifest.firmware_version,
        ),
    ] {
        if header_value != manifest_value {
            return Err(PackageError::HeaderManifestMismatch { field });
        }
    }

    let declared_header_version = manifest
        .payload_format
        .as_ref()
        .and_then(|format| format.stm32_header_version);
    if declared_header_version != Some(header.format_version()) {
        return Err(PackageError::HeaderManifestMismatch {
            field: "payload_format.stm32_header_version",
        });
    }
    if manifest
        .payload_format
        .as_ref()
        .and_then(|format| format.hpm_kn_version)
        .is_some()
    {
        return Err(PackageError::UnexpectedHpmPayloadFormat);
    }

    let series = mcu_series::series::by_name(&manifest.mcu)
        .ok_or_else(|| PackageError::UnknownMcu(manifest.mcu.clone()))?;
    let expected_load_address = series.flash_base + series.app_off;
    if header.load_address() != expected_load_address {
        return Err(PackageError::LoadAddressMismatch {
            expected: expected_load_address,
            actual: header.load_address(),
        });
    }
    if header.image_size() == 0 || header.image_size() > series.max_image() {
        return Err(PackageError::PlaintextSizeOutOfRange {
            actual: header.image_size(),
            limit: series.max_image(),
        });
    }

    match header.format_version() {
        FORMAT_VERSION_V1 => {
            if header.image_size() as usize != image.len() {
                return Err(PackageError::HeaderImageSizeMismatch);
            }
            let actual_crc = image_container::image_crc32_of(image);
            if header.image_crc32() != actual_crc {
                return Err(PackageError::HeaderImageCrc32Mismatch);
            }
            validate_v1_vector_table(series, image)?;
            Ok(Stm32ImageMode::PlaintextV1)
        }
        FORMAT_VERSION_V2 => {
            if manifest.mcu != MCU_STM32G0B1 {
                return Err(PackageError::V2WrongMcu);
            }
            if !header.flag_signed() || header.target_mcu() != TARGET_MCU_G0B1 {
                return Err(PackageError::InvalidV2Policy);
            }
            if header.wire_size() as usize != image.len() {
                return Err(PackageError::HeaderWireSizeMismatch);
            }
            let expected_wire = if header.flag_encrypted() {
                encrypted_wire_size(header.image_size())?
            } else {
                padded_plaintext_len(header.image_size())? as usize
            };
            if image.len() != expected_wire {
                return Err(PackageError::InvalidV2WireGeometry {
                    expected: expected_wire,
                    actual: image.len(),
                });
            }
            if header.flag_encrypted() {
                if header.record_plain_size() != V2_RECORD_PLAIN_SIZE
                    || header.record_tag_size() != V2_RECORD_TAG_SIZE
                {
                    return Err(PackageError::InvalidV2RecordGeometry);
                }
                Ok(Stm32ImageMode::EncryptedV2)
            } else {
                let plaintext_len = header.image_size() as usize;
                let plaintext = &image[..plaintext_len];
                if image[plaintext_len..].iter().any(|byte| *byte != 0xFF) {
                    return Err(PackageError::InvalidV2Padding);
                }
                if image_container::image_crc32_of(plaintext) != header.image_crc32() {
                    return Err(PackageError::HeaderImageCrc32Mismatch);
                }
                let digest: [u8; 32] = Sha256::digest(plaintext).into();
                if &digest != header.plaintext_sha256() {
                    return Err(PackageError::PlaintextSha256Mismatch);
                }
                Ok(Stm32ImageMode::SignedV2)
            }
        }
        version => Err(PackageError::UnsupportedHeaderVersion(version)),
    }
}

fn validate_v1_vector_table(
    series: &mcu_series::McuSeries,
    image: &[u8],
) -> Result<(), PackageError> {
    if image.len() < 8 {
        return Err(PackageError::ImageTooShortForVectorTable {
            actual: image.len(),
        });
    }
    let initial_msp = u32::from_le_bytes(image[0..4].try_into().expect("length checked"));
    let app_ram_top = series.ram_base + series.ram_len_app();
    if initial_msp <= series.ram_base || initial_msp > app_ram_top || initial_msp % 8 != 0 {
        return Err(PackageError::InvalidInitialMsp {
            actual: initial_msp,
            ram_base: series.ram_base,
            ram_top: app_ram_top,
        });
    }

    let reset_vector = u32::from_le_bytes(image[4..8].try_into().expect("length checked"));
    let image_start = series.flash_base + series.app_off;
    let image_end = image_start
        .checked_add(image.len() as u32)
        .ok_or(PackageError::VectorAddressOverflow)?;
    let reset_address = reset_vector & !1;
    if reset_vector & 1 == 0 || reset_address < image_start || reset_address >= image_end {
        return Err(PackageError::InvalidResetVector {
            actual: reset_vector,
            image_start,
            image_end,
        });
    }
    Ok(())
}

fn padded_plaintext_len(image_size: u32) -> Result<u32, PackageError> {
    image_size
        .checked_add(7)
        .map(|size| size / 8 * 8)
        .ok_or(PackageError::V2SizeOverflow)
}

fn encrypted_wire_size(image_size: u32) -> Result<usize, PackageError> {
    let padded = padded_plaintext_len(image_size)? as usize;
    let record_plain = usize::from(V2_RECORD_PLAIN_SIZE);
    let record_tag = usize::from(V2_RECORD_TAG_SIZE);
    let record_count = padded.div_ceil(record_plain);
    padded
        .checked_add(
            record_count
                .checked_mul(record_tag)
                .ok_or(PackageError::V2SizeOverflow)?,
        )
        .ok_or(PackageError::V2SizeOverflow)
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_shape_hex_eq(declared: &str, actual: &str) -> bool {
    declared.len() == 64
        && declared
            .bytes()
            .zip(actual.bytes())
            .all(|(left, right)| left.to_ascii_lowercase() == right)
}

#[derive(Debug, Error)]
pub enum PackageError {
    #[error("invalid package limits: {0}")]
    InvalidLimits(&'static str),
    #[error("archive is {actual} bytes, above the configured {limit}-byte limit")]
    ArchiveTooLarge { actual: usize, limit: usize },
    #[error("invalid tar archive: {0}")]
    InvalidArchive(String),
    #[error("archive contains more than {limit} entries")]
    TooManyEntries { limit: usize },
    #[error("archive members must be regular files")]
    NonFileMember,
    #[error("archive member name is not UTF-8")]
    NonUtf8Member,
    #[error("unexpected archive member {0:?}")]
    UnexpectedMember(String),
    #[error("duplicate archive member {0:?}")]
    DuplicateMember(String),
    #[error("archive member {member:?} is {actual} bytes, above its {limit}-byte limit")]
    MemberTooLarge {
        member: String,
        actual: usize,
        limit: usize,
    },
    #[error("archive member {member:?} declares {declared} bytes but yielded {actual}")]
    MemberSizeMismatch {
        member: String,
        declared: usize,
        actual: usize,
    },
    #[error("package is missing mandatory member {0:?}")]
    MissingMember(&'static str),
    #[error("manifest.json is invalid: {0}")]
    InvalidManifest(String),
    #[error("unsupported package format {0:?}")]
    UnknownFormat(String),
    #[error("unsupported MCU {0:?}")]
    UnknownMcu(String),
    #[error("STM32 backend requires vendor 0x{VENDOR_ID:08X}, got 0x{0:08X}")]
    ForeignVendor(u32),
    #[error("manifest product_code is the unprovisioned sentinel")]
    SentinelProduct,
    #[error("{field} must refer to {expected:?}")]
    WrongMemberReference {
        field: &'static str,
        expected: &'static str,
    },
    #[error("STM32 package must not declare an HPM envelope")]
    UnexpectedEnvelope,
    #[error("STM32 package must not declare hpm_kn_version")]
    UnexpectedHpmPayloadFormat,
    #[error("manifest tool_version must not be empty")]
    EmptyToolVersion,
    #[error("image.bin size mismatch: manifest {declared}, actual {actual}")]
    ImageSizeMismatch { declared: u64, actual: usize },
    #[error("image.bin SHA-256 mismatch or malformed digest")]
    ImageSha256Mismatch,
    #[error("STM32 manifest must include image.crc32")]
    MissingImageCrc32,
    #[error("image.bin CRC32 mismatch: manifest 0x{declared:08X}, actual 0x{actual:08X}")]
    ImageCrc32Mismatch { declared: u32, actual: u32 },
    #[error("header.bin must be exactly {HEADER_LEN} bytes, got {actual}")]
    HeaderSize { actual: usize },
    #[error("invalid container header: {0}")]
    InvalidHeader(String),
    #[error("header and manifest disagree at {field}")]
    HeaderManifestMismatch { field: &'static str },
    #[error("header load address 0x{actual:08X} does not match MCU address 0x{expected:08X}")]
    LoadAddressMismatch { expected: u32, actual: u32 },
    #[error("plaintext image size {actual} is outside 1..={limit}")]
    PlaintextSizeOutOfRange { actual: u32, limit: u32 },
    #[error("v1 header image_size does not match image.bin")]
    HeaderImageSizeMismatch,
    #[error("container image CRC32 does not match image.bin")]
    HeaderImageCrc32Mismatch,
    #[error("v1 image must contain an 8-byte Cortex-M vector table, got {actual} bytes")]
    ImageTooShortForVectorTable { actual: usize },
    #[error(
        "initial MSP 0x{actual:08X} must be 8-byte aligned and within (0x{ram_base:08X}, 0x{ram_top:08X}]"
    )]
    InvalidInitialMsp {
        actual: u32,
        ram_base: u32,
        ram_top: u32,
    },
    #[error(
        "reset vector 0x{actual:08X} must have the Thumb bit set and point within [0x{image_start:08X}, 0x{image_end:08X})"
    )]
    InvalidResetVector {
        actual: u32,
        image_start: u32,
        image_end: u32,
    },
    #[error("vector-table address calculation overflow")]
    VectorAddressOverflow,
    #[error("secure v2 is only valid for stm32g0b1")]
    V2WrongMcu,
    #[error("secure v2 header is not signed or has the wrong target MCU")]
    InvalidV2Policy,
    #[error("v2 header wire_size does not match image.bin")]
    HeaderWireSizeMismatch,
    #[error("v2 wire geometry mismatch: expected {expected}, got {actual}")]
    InvalidV2WireGeometry { expected: usize, actual: usize },
    #[error("v2 encrypted record geometry is unsupported")]
    InvalidV2RecordGeometry,
    #[error("v2 signed plaintext padding must contain only 0xFF")]
    InvalidV2Padding,
    #[error("v2 plaintext SHA-256 does not match the signed header")]
    PlaintextSha256Mismatch,
    #[error("unsupported STM32 header version {0}")]
    UnsupportedHeaderVersion(u16),
    #[error("v2 size calculation overflow")]
    V2SizeOverflow,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use image_container::HeaderBuilder;

    use super::*;

    const PRODUCT: u32 = 0x1234_5678;

    fn valid_image(len: usize) -> Vec<u8> {
        assert!(len >= 12);
        let mut image = vec![0xA5; len];
        image[0..4].copy_from_slice(&0x2000_7FD8u32.to_le_bytes());
        image[4..8].copy_from_slice(&0x0800_9209u32.to_le_bytes());
        image
    }

    fn manifest(image: &[u8]) -> Manifest {
        Manifest {
            format: FORMAT.to_owned(),
            mcu: MCU_STM32G431.to_owned(),
            vendor_id: VENDOR_ID,
            product_code: PRODUCT,
            min_hardware_rev: 0x0002_0000,
            firmware_id: 0x42,
            firmware_version: 0x0001_0001,
            image: ImageMeta {
                member: MEMBER_IMAGE.to_owned(),
                size: image.len() as u64,
                sha256: sha256_hex(image),
                crc32: Some(image_container::image_crc32_of(image)),
            },
            header: Some(MemberRef {
                member: MEMBER_HEADER.to_owned(),
            }),
            envelope: None,
            key_fingerprint: None,
            pubkey_fingerprint: None,
            app_arv: None,
            payload_format: Some(PayloadFormat {
                stm32_header_version: Some(FORMAT_VERSION_V1),
                hpm_kn_version: None,
            }),
            tool_version: "test".to_owned(),
            built_at: None,
        }
    }

    fn header(image: &[u8]) -> [u8; HEADER_LEN] {
        *HeaderBuilder::new()
            .product_code(PRODUCT)
            .min_hardware_rev(0x0002_0000)
            .firmware_id(0x42)
            .firmware_version(0x0001_0001)
            .load_address(0x0800_9200)
            .image(image)
            .finish()
            .as_bytes()
    }

    fn append<W: Write>(archive: &mut tar::Builder<W>, name: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_ustar();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, name, bytes).unwrap();
    }

    fn package_bytes(image: &[u8]) -> Vec<u8> {
        let manifest = serde_json::to_vec(&manifest(image)).unwrap();
        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append(&mut archive, MEMBER_MANIFEST, &manifest);
            append(&mut archive, MEMBER_IMAGE, image);
            append(&mut archive, MEMBER_HEADER, &header(image));
            archive.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn valid_package_round_trips_from_memory() {
        let image = valid_image(4099);
        let package = read_package_bytes(&package_bytes(&image), PackageLimits::default()).unwrap();
        assert_eq!(package.image(), image);
        assert_eq!(package.image_mode(), Stm32ImageMode::PlaintextV1);
        assert_eq!(package.plaintext_size(), 4099);
        assert_eq!(package.wire_size(), 4104);
        assert_eq!(package.manifest().product_code, PRODUCT);
    }

    #[test]
    fn archive_cap_is_checked_before_tar_parsing() {
        let bytes = package_bytes(&valid_image(64));
        let error = read_package_bytes(
            &bytes,
            PackageLimits {
                max_archive_bytes: bytes.len() - 1,
                ..PackageLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, PackageError::ArchiveTooLarge { .. }));
    }

    #[test]
    fn declared_member_cap_is_checked_before_allocation_or_read() {
        let image = valid_image(257);
        let bytes = package_bytes(&image);
        let error = read_package_bytes(
            &bytes,
            PackageLimits {
                max_image_bytes: 256,
                ..PackageLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PackageError::MemberTooLarge {
                ref member,
                actual: 257,
                limit: 256
            } if member == MEMBER_IMAGE
        ));
    }

    #[test]
    fn duplicate_member_is_rejected() {
        let image = valid_image(64);
        let manifest = serde_json::to_vec(&manifest(&image)).unwrap();
        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append(&mut archive, MEMBER_MANIFEST, &manifest);
            append(&mut archive, MEMBER_MANIFEST, &manifest);
            append(&mut archive, MEMBER_IMAGE, &image);
            append(&mut archive, MEMBER_HEADER, &header(&image));
            archive.finish().unwrap();
        }
        let error = read_package_bytes(
            &bytes,
            PackageLimits {
                max_entries: 4,
                ..PackageLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, PackageError::DuplicateMember(_)));
    }

    #[test]
    fn manifest_header_disagreement_is_rejected() {
        let image = valid_image(64);
        let mut manifest = manifest(&image);
        manifest.product_code ^= 1;
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append(&mut archive, MEMBER_MANIFEST, &manifest);
            append(&mut archive, MEMBER_IMAGE, &image);
            append(&mut archive, MEMBER_HEADER, &header(&image));
            archive.finish().unwrap();
        }
        let error = read_package_bytes(&bytes, PackageLimits::default()).unwrap_err();
        assert!(matches!(
            error,
            PackageError::HeaderManifestMismatch {
                field: "product_code"
            }
        ));
    }

    #[test]
    fn v1_vector_table_requires_aligned_ram_msp_and_thumb_reset_inside_image() {
        let mut bad_msp = valid_image(64);
        bad_msp[0..4].copy_from_slice(&0x2000_0004u32.to_le_bytes());
        let error =
            read_package_bytes(&package_bytes(&bad_msp), PackageLimits::default()).unwrap_err();
        assert!(matches!(error, PackageError::InvalidInitialMsp { .. }));

        let mut no_thumb = valid_image(64);
        no_thumb[4..8].copy_from_slice(&0x0800_9208u32.to_le_bytes());
        let error =
            read_package_bytes(&package_bytes(&no_thumb), PackageLimits::default()).unwrap_err();
        assert!(matches!(error, PackageError::InvalidResetVector { .. }));

        let mut past_image = valid_image(64);
        past_image[4..8].copy_from_slice(&0x0800_9301u32.to_le_bytes());
        let error =
            read_package_bytes(&package_bytes(&past_image), PackageLimits::default()).unwrap_err();
        assert!(matches!(error, PackageError::InvalidResetVector { .. }));
    }
}
