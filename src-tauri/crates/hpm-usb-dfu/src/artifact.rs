use std::collections::HashMap;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::protocol::{validate_known_device, DeviceInfo, SecurityMode};
use crate::{DfuError, Result, APP0_MAX_SIZE, FLASH_SECTOR_SIZE};

const MAX_ARTIFACT_SIZE: usize = 2 * 1024 * 1024;
const MAX_META_SIZE: usize = 16 * 1024;
const KN_DATA_SIZE: usize = 128;
const TAR_BLOCK: usize = 512;
const PAD_PREFIX_SIZE: usize = 0x400;
const CODE_OFFSET: usize = 0x3000;
const NOR_CFG_HEADER_MASK: u32 = 0xFFFF_F000;
const NOR_CFG_HEADER_TAG: u32 = 0xFCF9_0000;

const MEMBER_META: &str = "meta.json";
const MEMBER_APP0: &str = "app0.bin";
const MEMBER_KN: &str = "kn_data.bin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    DevelopmentRaw,
    LegacyHpmOtaV2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSummary {
    pub kind: ArtifactKind,
    pub source_sha256_hex: String,
    pub wire_image_sha256_hex: String,
    pub source_size: usize,
    pub wire_image_size: usize,
    pub erase_size: usize,
    /// Informational only for v2: meta is public and KN_DATA is opaque.
    pub app_arv: Option<u32>,
    pub pack_tool_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PreparedArtifact {
    device: DeviceInfo,
    image: Vec<u8>,
    kn_data: Option<[u8; KN_DATA_SIZE]>,
    summary: ArtifactSummary,
}

impl PreparedArtifact {
    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    pub fn image(&self) -> &[u8] {
        &self.image
    }

    pub fn kn_data(&self) -> Option<&[u8; KN_DATA_SIZE]> {
        self.kn_data.as_ref()
    }

    pub fn summary(&self) -> &ArtifactSummary {
        &self.summary
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct V2Meta {
    format: String,
    key_fingerprint: String,
    pubkey_fingerprint: String,
    app0_size_bytes: usize,
    ciphertext_app0_sha256: String,
    kn_blob_size: usize,
    app_arv: u32,
    tool_version: String,
}

pub(crate) fn prepare_for_device(bytes: Vec<u8>, device: DeviceInfo) -> Result<PreparedArtifact> {
    validate_known_device(&device)?;
    if bytes.is_empty() {
        return Err(DfuError::InvalidArtifact("file is empty".into()));
    }
    if bytes.len() > MAX_ARTIFACT_SIZE {
        return Err(DfuError::InvalidArtifact(format!(
            "file is {} bytes; hard limit is {MAX_ARTIFACT_SIZE}",
            bytes.len()
        )));
    }

    match device.security {
        SecurityMode::ProductionConfidential => prepare_v2_pack(bytes, device),
        SecurityMode::Development => prepare_development_raw(bytes, device),
    }
}

fn prepare_v2_pack(bytes: Vec<u8>, device: DeviceInfo) -> Result<PreparedArtifact> {
    let source_sha256_hex = sha256_hex(&bytes);
    let source_size = bytes.len();
    let mut members = parse_strict_ustar(&bytes)?;
    let meta_bytes = members
        .remove(MEMBER_META)
        .ok_or_else(|| DfuError::InvalidArtifact(format!("missing {MEMBER_META}")))?;
    let image = members
        .remove(MEMBER_APP0)
        .ok_or_else(|| DfuError::InvalidArtifact(format!("missing {MEMBER_APP0}")))?;
    let kn_vec = members
        .remove(MEMBER_KN)
        .ok_or_else(|| DfuError::InvalidArtifact(format!("missing {MEMBER_KN}")))?;
    if !members.is_empty() {
        return Err(DfuError::InvalidArtifact(
            "archive contains unexpected members".into(),
        ));
    }

    let meta: V2Meta = serde_json::from_slice(&meta_bytes)
        .map_err(|error| DfuError::InvalidArtifact(format!("invalid meta.json: {error}")))?;
    if meta.format != "hpm-bl-ota/2" {
        return Err(DfuError::InvalidArtifact(format!(
            "unsupported meta format {:?}; expected \"hpm-bl-ota/2\"",
            meta.format
        )));
    }
    if meta.tool_version.is_empty() || meta.tool_version.len() > 64 {
        return Err(DfuError::InvalidArtifact(
            "tool_version must contain 1..64 bytes".into(),
        ));
    }

    let pack_key = parse_fingerprint(&meta.key_fingerprint, "key_fingerprint")?;
    let pack_pubkey = parse_fingerprint(&meta.pubkey_fingerprint, "pubkey_fingerprint")?;
    if pack_key != device.key_fingerprint {
        return Err(DfuError::InvalidArtifact(format!(
            "pack key fingerprint 0x{pack_key:08X} does not match device 0x{:08X}",
            device.key_fingerprint
        )));
    }
    if pack_pubkey != device.pubkey_fingerprint {
        return Err(DfuError::InvalidArtifact(format!(
            "pack public-key fingerprint 0x{pack_pubkey:08X} does not match device 0x{:08X}",
            device.pubkey_fingerprint
        )));
    }

    if image.is_empty() || image.len() > device.app0_max_size as usize {
        return Err(DfuError::InvalidArtifact(format!(
            "app0.bin size {} is outside 1..={}",
            image.len(),
            device.app0_max_size
        )));
    }
    if image.len() % FLASH_SECTOR_SIZE as usize != 0 {
        return Err(DfuError::InvalidArtifact(format!(
            "app0.bin wire size {} is not 4 KiB aligned",
            image.len()
        )));
    }
    validate_slot_aligned_image(&image)?;
    let expected_wire_size = meta
        .app0_size_bytes
        .checked_add(PAD_PREFIX_SIZE)
        .and_then(|size| size.checked_add(FLASH_SECTOR_SIZE as usize - 1))
        .map(|size| size / FLASH_SECTOR_SIZE as usize * FLASH_SECTOR_SIZE as usize)
        .ok_or_else(|| {
            DfuError::InvalidArtifact("meta app0_size_bytes overflows wire layout".into())
        })?;
    if meta.app0_size_bytes == 0 || expected_wire_size != image.len() {
        return Err(DfuError::InvalidArtifact(format!(
            "meta app0_size_bytes {} implies a {}-byte padded wire image, actual app0.bin is {} bytes",
            meta.app0_size_bytes, expected_wire_size, image.len()
        )));
    }

    let expected_digest = parse_sha256_hex(&meta.ciphertext_app0_sha256, "ciphertext_app0_sha256")?;
    let actual_digest: [u8; 32] = Sha256::digest(&image).into();
    if expected_digest != actual_digest {
        return Err(DfuError::InvalidArtifact(format!(
            "app0.bin SHA-256 mismatch: meta={}, actual={}",
            meta.ciphertext_app0_sha256,
            hex_lower(&actual_digest)
        )));
    }

    if meta.kn_blob_size != KN_DATA_SIZE || kn_vec.len() != KN_DATA_SIZE {
        return Err(DfuError::InvalidArtifact(format!(
            "KN_DATA must be exactly {KN_DATA_SIZE} bytes (meta={}, actual={})",
            meta.kn_blob_size,
            kn_vec.len()
        )));
    }
    let kn_data: [u8; KN_DATA_SIZE] = kn_vec.try_into().unwrap();
    let erase_size = image.len();
    let wire_image_sha256_hex = hex_lower(&actual_digest);

    Ok(PreparedArtifact {
        device,
        image,
        kn_data: Some(kn_data),
        summary: ArtifactSummary {
            kind: ArtifactKind::LegacyHpmOtaV2,
            source_sha256_hex,
            wire_image_sha256_hex,
            source_size,
            wire_image_size: erase_size,
            erase_size,
            app_arv: Some(meta.app_arv),
            pack_tool_version: Some(meta.tool_version),
        },
    })
}

fn prepare_development_raw(bytes: Vec<u8>, device: DeviceInfo) -> Result<PreparedArtifact> {
    if looks_like_ustar(&bytes) {
        return Err(DfuError::InvalidArtifact(
            "development devices accept plaintext raw .bin only; .hpmota is rejected because v2 does not encode payload mode"
                .into(),
        ));
    }

    let source_sha256_hex = sha256_hex(&bytes);
    let source_size = bytes.len();
    let image = if is_nor_cfg_header_at(&bytes, 0) {
        let mut padded = vec![0xFF; PAD_PREFIX_SIZE];
        padded.extend_from_slice(&bytes);
        padded
    } else if bytes.len() > PAD_PREFIX_SIZE
        && bytes[..PAD_PREFIX_SIZE].iter().all(|byte| *byte == 0xFF)
        && is_nor_cfg_header_at(&bytes, PAD_PREFIX_SIZE)
    {
        bytes
    } else {
        return Err(DfuError::InvalidArtifact(
            "raw APP0 must contain the HPM NOR config header at offset 0, or after an existing 0x400-byte 0xFF prefix"
                .into(),
        ));
    };

    if image.len() < CODE_OFFSET {
        return Err(DfuError::InvalidArtifact(format!(
            "slot-aligned APP0 is only {} bytes; it does not reach code offset 0x{CODE_OFFSET:X}",
            image.len()
        )));
    }
    if image.len() > device.app0_max_size as usize || image.len() > APP0_MAX_SIZE as usize {
        return Err(DfuError::InvalidArtifact(format!(
            "slot-aligned APP0 is {} bytes, capacity is {}",
            image.len(),
            device.app0_max_size
        )));
    }
    validate_slot_aligned_image(&image)?;

    let wire_image_size = image.len();
    let sector = device.sector_size as usize;
    let erase_size = image.len().div_ceil(sector) * sector;
    let wire_image_sha256_hex = sha256_hex(&image);
    Ok(PreparedArtifact {
        device,
        image,
        kn_data: None,
        summary: ArtifactSummary {
            kind: ArtifactKind::DevelopmentRaw,
            source_sha256_hex,
            wire_image_sha256_hex,
            source_size,
            wire_image_size,
            erase_size,
            app_arv: None,
            pack_tool_version: None,
        },
    })
}

fn validate_slot_aligned_image(image: &[u8]) -> Result<()> {
    if image.len() <= PAD_PREFIX_SIZE || image[..PAD_PREFIX_SIZE].iter().any(|byte| *byte != 0xFF) {
        return Err(DfuError::InvalidArtifact(format!(
            "wire APP0 must start with exactly 0x{PAD_PREFIX_SIZE:X} bytes of 0xFF"
        )));
    }
    if !is_nor_cfg_header_at(image, PAD_PREFIX_SIZE) {
        return Err(DfuError::InvalidArtifact(format!(
            "wire APP0 has no HPM NOR config header at slot offset 0x{PAD_PREFIX_SIZE:X}"
        )));
    }
    Ok(())
}

fn is_nor_cfg_header_at(bytes: &[u8], offset: usize) -> bool {
    bytes
        .get(offset..offset + 4)
        .and_then(|field| field.try_into().ok())
        .map(u32::from_le_bytes)
        .is_some_and(|word| word & NOR_CFG_HEADER_MASK == NOR_CFG_HEADER_TAG)
}

fn looks_like_ustar(bytes: &[u8]) -> bool {
    bytes.get(257..263) == Some(b"ustar\0".as_slice())
}

/// Parse only the frozen uncompressed USTAR subset. PAX/GNU extensions,
/// links, directories, unknown members, duplicate members, non-zero padding,
/// bad checksums and trailing non-zero data all fail closed.
fn parse_strict_ustar(bytes: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    if bytes.len() < TAR_BLOCK * 2 || bytes.len().checked_rem(TAR_BLOCK) != Some(0) {
        return Err(DfuError::InvalidArtifact(
            "hpmota must be an uncompressed 512-byte-aligned USTAR archive".into(),
        ));
    }

    let mut members = HashMap::new();
    let mut offset = 0usize;
    let mut saw_end = false;
    while offset + TAR_BLOCK <= bytes.len() {
        let header = &bytes[offset..offset + TAR_BLOCK];
        if header.iter().all(|byte| *byte == 0) {
            if offset + TAR_BLOCK * 2 > bytes.len()
                || bytes[offset + TAR_BLOCK..offset + TAR_BLOCK * 2]
                    .iter()
                    .any(|byte| *byte != 0)
            {
                return Err(DfuError::InvalidArtifact(
                    "USTAR archive is missing the second zero end block".into(),
                ));
            }
            if bytes[offset + TAR_BLOCK * 2..]
                .iter()
                .any(|byte| *byte != 0)
            {
                return Err(DfuError::InvalidArtifact(
                    "USTAR archive has non-zero trailing data".into(),
                ));
            }
            saw_end = true;
            break;
        }

        validate_ustar_header(header)?;
        let name = parse_ustar_name(&header[0..100])?;
        if !matches!(name.as_str(), MEMBER_META | MEMBER_APP0 | MEMBER_KN) {
            return Err(DfuError::InvalidArtifact(format!(
                "unexpected USTAR member {name:?}"
            )));
        }
        if members.contains_key(&name) {
            return Err(DfuError::InvalidArtifact(format!(
                "duplicate USTAR member {name:?}"
            )));
        }

        let size = parse_octal(&header[124..136], "member size")?;
        let hard_limit = match name.as_str() {
            MEMBER_META => MAX_META_SIZE,
            MEMBER_APP0 => APP0_MAX_SIZE as usize,
            MEMBER_KN => KN_DATA_SIZE,
            _ => unreachable!(),
        };
        if size > hard_limit {
            return Err(DfuError::InvalidArtifact(format!(
                "USTAR member {name:?} is {size} bytes, limit is {hard_limit}"
            )));
        }
        let data_start = offset + TAR_BLOCK;
        let data_end = data_start
            .checked_add(size)
            .ok_or_else(|| DfuError::InvalidArtifact("member size overflow".into()))?;
        let padded_end = data_start
            .checked_add(size.div_ceil(TAR_BLOCK) * TAR_BLOCK)
            .ok_or_else(|| DfuError::InvalidArtifact("member padding overflow".into()))?;
        if padded_end > bytes.len() {
            return Err(DfuError::InvalidArtifact(format!(
                "USTAR member {name:?} is truncated"
            )));
        }
        if bytes[data_end..padded_end].iter().any(|byte| *byte != 0) {
            return Err(DfuError::InvalidArtifact(format!(
                "USTAR member {name:?} has non-zero block padding"
            )));
        }
        members.insert(name, bytes[data_start..data_end].to_vec());
        offset = padded_end;
    }

    if !saw_end {
        return Err(DfuError::InvalidArtifact(
            "USTAR archive has no two-block end marker".into(),
        ));
    }
    if members.len() != 3
        || !members.contains_key(MEMBER_META)
        || !members.contains_key(MEMBER_APP0)
        || !members.contains_key(MEMBER_KN)
    {
        return Err(DfuError::InvalidArtifact(
            "hpmota must contain exactly meta.json, app0.bin and kn_data.bin".into(),
        ));
    }
    Ok(members)
}

fn validate_ustar_header(header: &[u8]) -> Result<()> {
    if &header[257..263] != b"ustar\0" || &header[263..265] != b"00" {
        return Err(DfuError::InvalidArtifact(
            "archive member is not POSIX USTAR".into(),
        ));
    }
    if !matches!(header[156], 0 | b'0') {
        return Err(DfuError::InvalidArtifact(format!(
            "USTAR member type 0x{:02X} is not a regular file",
            header[156]
        )));
    }
    if header[157..257].iter().any(|byte| *byte != 0)
        || header[345..500].iter().any(|byte| *byte != 0)
    {
        return Err(DfuError::InvalidArtifact(
            "USTAR links and path prefixes are not allowed".into(),
        ));
    }

    let stored_checksum = parse_octal(&header[148..156], "header checksum")? as u64;
    let actual_checksum: u64 = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                b' ' as u64
            } else {
                *byte as u64
            }
        })
        .sum();
    if stored_checksum != actual_checksum {
        return Err(DfuError::InvalidArtifact(format!(
            "USTAR header checksum mismatch: stored={stored_checksum}, actual={actual_checksum}"
        )));
    }
    Ok(())
}

fn parse_ustar_name(field: &[u8]) -> Result<String> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if field[end..].iter().any(|byte| *byte != 0) {
        return Err(DfuError::InvalidArtifact(
            "USTAR name has data after NUL terminator".into(),
        ));
    }
    let name = std::str::from_utf8(&field[..end])
        .map_err(|_| DfuError::InvalidArtifact("USTAR member name is not UTF-8".into()))?;
    if name.is_empty() {
        return Err(DfuError::InvalidArtifact(
            "USTAR member name is empty".into(),
        ));
    }
    Ok(name.to_owned())
}

fn parse_octal(field: &[u8], label: &str) -> Result<usize> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err(DfuError::InvalidArtifact(format!(
            "{label} uses unsupported base-256 encoding"
        )));
    }
    let trimmed = field
        .iter()
        .copied()
        .skip_while(|byte| matches!(byte, 0 | b' '))
        .take_while(|byte| !matches!(byte, 0 | b' '))
        .collect::<Vec<_>>();
    if trimmed.is_empty() || trimmed.iter().any(|byte| !(b'0'..=b'7').contains(byte)) {
        return Err(DfuError::InvalidArtifact(format!(
            "{label} is not a canonical octal field"
        )));
    }
    trimmed.iter().try_fold(0usize, |value, byte| {
        value
            .checked_mul(8)
            .and_then(|value| value.checked_add((byte - b'0') as usize))
            .ok_or_else(|| DfuError::InvalidArtifact(format!("{label} overflows usize")))
    })
}

fn parse_fingerprint(value: &str, label: &str) -> Result<u32> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .ok_or_else(|| {
            DfuError::InvalidArtifact(format!("{label} must use exact 0x12345678 syntax"))
        })?;
    if digits.len() != 8 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DfuError::InvalidArtifact(format!(
            "{label} must contain exactly eight hexadecimal digits"
        )));
    }
    u32::from_str_radix(digits, 16)
        .map_err(|error| DfuError::InvalidArtifact(format!("invalid {label}: {error}")))
}

fn parse_sha256_hex(value: &str, label: &str) -> Result<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DfuError::InvalidArtifact(format!(
            "{label} must contain exactly 64 hexadecimal digits"
        )));
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|error| DfuError::InvalidArtifact(format!("invalid {label}: {error}")))?;
    }
    Ok(out)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::{GCAN_PRODUCT_CODE, LEGACY_BL_VERSION},
        APP0_ADDRESS, FLASH_PAGE_SIZE, PLACEHOLDER_KEY_FINGERPRINT, PLACEHOLDER_PUBKEY_FINGERPRINT,
    };

    fn device(security: SecurityMode) -> DeviceInfo {
        DeviceInfo {
            uid: [0x33; 16],
            chip_family_id: 0x5300,
            product_code: GCAN_PRODUCT_CODE,
            hw_version: 0,
            hw_version_valid: false,
            bl_version: LEGACY_BL_VERSION,
            app0_addr: APP0_ADDRESS,
            app0_max_size: APP0_MAX_SIZE,
            sector_size: FLASH_SECTOR_SIZE,
            page_size: FLASH_PAGE_SIZE,
            key_fingerprint: if security == SecurityMode::Development {
                PLACEHOLDER_KEY_FINGERPRINT
            } else {
                0x1122_3344
            },
            pubkey_fingerprint: if security == SecurityMode::Development {
                PLACEHOLDER_PUBKEY_FINGERPRINT
            } else {
                0x5566_7788
            },
            security,
            otp_app_arv_floor: 0,
        }
    }

    fn slot_image() -> Vec<u8> {
        let mut image = vec![0xA5; 0x4000];
        image[..PAD_PREFIX_SIZE].fill(0xFF);
        image[PAD_PREFIX_SIZE..PAD_PREFIX_SIZE + 4].copy_from_slice(&0xFCF9_0002u32.to_le_bytes());
        image
    }

    fn header(name: &str, size: usize, typeflag: u8) -> [u8; TAR_BLOCK] {
        let mut header = [0u8; TAR_BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size_field = format!("{size:011o}\0");
        header[124..136].copy_from_slice(size_field.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].fill(b' ');
        header[156] = typeflag;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| *byte as u64).sum();
        let checksum_field = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum_field.as_bytes());
        header
    }

    fn push_member(archive: &mut Vec<u8>, name: &str, data: &[u8]) {
        archive.extend_from_slice(&header(name, data.len(), b'0'));
        archive.extend_from_slice(data);
        archive.resize(archive.len().div_ceil(TAR_BLOCK) * TAR_BLOCK, 0);
    }

    fn valid_pack() -> Vec<u8> {
        let image = slot_image();
        let meta = format!(
            concat!(
                "{{",
                "\"format\":\"hpm-bl-ota/2\",",
                "\"key_fingerprint\":\"0x11223344\",",
                "\"pubkey_fingerprint\":\"0x55667788\",",
                "\"app0_size_bytes\":12288,",
                "\"ciphertext_app0_sha256\":\"{}\",",
                "\"kn_blob_size\":128,",
                "\"app_arv\":7,",
                "\"tool_version\":\"0.2.0\"",
                "}}"
            ),
            sha256_hex(&image)
        );
        let mut archive = Vec::new();
        push_member(&mut archive, MEMBER_META, meta.as_bytes());
        push_member(&mut archive, MEMBER_APP0, &image);
        push_member(&mut archive, MEMBER_KN, &[0xC3; KN_DATA_SIZE]);
        archive.resize(archive.len() + TAR_BLOCK * 2, 0);
        archive
    }

    #[test]
    fn accepts_strict_current_v2_pack() {
        let prepared =
            prepare_for_device(valid_pack(), device(SecurityMode::ProductionConfidential)).unwrap();
        assert_eq!(prepared.summary.kind, ArtifactKind::LegacyHpmOtaV2);
        assert_eq!(prepared.summary.app_arv, Some(7));
        assert_eq!(prepared.image.len(), 0x4000);
        assert_eq!(prepared.kn_data.unwrap(), [0xC3; KN_DATA_SIZE]);
    }

    #[test]
    fn rejects_extra_duplicate_link_and_path_members() {
        let mut extra = valid_pack();
        extra.truncate(extra.len() - TAR_BLOCK * 2);
        push_member(&mut extra, "extra.bin", b"x");
        extra.resize(extra.len() + TAR_BLOCK * 2, 0);
        assert!(parse_strict_ustar(&extra).is_err());

        let mut duplicate = valid_pack();
        duplicate.truncate(duplicate.len() - TAR_BLOCK * 2);
        push_member(&mut duplicate, MEMBER_KN, &[0; KN_DATA_SIZE]);
        duplicate.resize(duplicate.len() + TAR_BLOCK * 2, 0);
        assert!(parse_strict_ustar(&duplicate).is_err());

        let mut link = Vec::new();
        link.extend_from_slice(&header(MEMBER_META, 0, b'2'));
        link.resize(link.len() + TAR_BLOCK * 2, 0);
        assert!(parse_strict_ustar(&link).is_err());

        let mut traversal = Vec::new();
        push_member(&mut traversal, "../meta.json", b"{}");
        traversal.resize(traversal.len() + TAR_BLOCK * 2, 0);
        assert!(parse_strict_ustar(&traversal).is_err());
    }

    #[test]
    fn rejects_sha_kn_length_and_fingerprint_mismatch() {
        let mut bad_sha = valid_pack();
        let needle = sha256_hex(&slot_image());
        let pos = bad_sha
            .windows(needle.len())
            .position(|window| window == needle.as_bytes())
            .unwrap();
        bad_sha[pos] = if bad_sha[pos] == b'a' { b'b' } else { b'a' };
        assert!(prepare_for_device(bad_sha, device(SecurityMode::ProductionConfidential)).is_err());

        let mut bad_kn = valid_pack();
        let parsed = parse_strict_ustar(&bad_kn).unwrap();
        assert_eq!(parsed[MEMBER_KN].len(), KN_DATA_SIZE);
        let kn_header_pos = bad_kn
            .windows(MEMBER_KN.len())
            .position(|window| window == MEMBER_KN.as_bytes())
            .unwrap();
        bad_kn[kn_header_pos + 124..kn_header_pos + 136]
            .copy_from_slice(format!("{:011o}\0", 127).as_bytes());
        assert!(parse_strict_ustar(&bad_kn).is_err());

        let mut wrong_device = device(SecurityMode::ProductionConfidential);
        wrong_device.key_fingerprint ^= 1;
        assert!(prepare_for_device(valid_pack(), wrong_device).is_err());
    }

    #[test]
    fn rejects_meta_plain_size_that_does_not_imply_wire_size() {
        let mut pack = valid_pack();
        let needle = b"\"app0_size_bytes\":12288";
        let pos = pack
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap();
        pack[pos..pos + needle.len()].copy_from_slice(b"\"app0_size_bytes\":16384");
        assert!(prepare_for_device(pack, device(SecurityMode::ProductionConfidential)).is_err());
    }

    #[test]
    fn development_mode_accepts_only_structural_raw_image() {
        let mut raw = vec![0xA5; 0x3000];
        raw[0..4].copy_from_slice(&0xFCF9_0002u32.to_le_bytes());
        let prepared = prepare_for_device(raw, device(SecurityMode::Development)).unwrap();
        assert_eq!(prepared.summary.kind, ArtifactKind::DevelopmentRaw);
        assert!(prepared.image[..PAD_PREFIX_SIZE]
            .iter()
            .all(|byte| *byte == 0xFF));

        assert!(prepare_for_device(valid_pack(), device(SecurityMode::Development)).is_err());
        assert!(prepare_for_device(vec![0xAA; 0x4000], device(SecurityMode::Development)).is_err());
    }
}
