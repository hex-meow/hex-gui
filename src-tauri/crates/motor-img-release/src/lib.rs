//! Signed, identity-bound catalog documents for supplier Motor IMG releases.
//!
//! Signatures cover a domain-separated binary transcript with fixed field
//! tags and lengths. JSON serialization and object-key order are deliberately
//! outside the signature contract.

use p256::ecdsa::{
    signature::hazmat::{PrehashSigner, PrehashVerifier},
    Signature, SigningKey, VerifyingKey,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const RELEASE_FORMAT: &str = "hexmeow-cobs-iap-release/1";
pub const LATEST_FORMAT: &str = "hexmeow-cobs-iap-latest/1";
pub const STABLE_CHANNEL: &str = "stable";
pub const SUPPLIER_IMG_SOURCE_KIND: &str = "supplier-img";

const RELEASE_DOMAIN: &[u8] = b"hexmeow-cobs-iap-release-signature/v1\0";
const LATEST_DOMAIN: &[u8] = b"hexmeow-cobs-iap-latest-signature/v1\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDocument {
    pub format: String,
    pub profile_id: String,
    #[serde(with = "hex_u32")]
    pub vendor_id: u32,
    #[serde(with = "hex_u32")]
    pub product_code: u32,
    pub channel: String,
    pub sequence: u64,
    pub release_id: String,
    pub artifact: ArtifactRef,
    pub native_img: NativeImg,
    pub expected_postflash: ExpectedPostflash,
    pub publication: Publication,
    pub catalog_key_id: u32,
    pub catalog_signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeImg {
    #[serde(with = "hex_u32")]
    pub device_id: u32,
    #[serde(with = "hex_u32")]
    pub firmware_id: u32,
    #[serde(with = "hex_u32")]
    pub firmware_version: u32,
    pub encrypted: bool,
    pub protected_sha256: String,
    /// Opaque supplier signature bytes from the IMG tag.
    pub vendor_signature: String,
    pub iv: String,
    #[serde(with = "hex_u32")]
    pub start_address: u32,
    #[serde(with = "hex_u32")]
    pub end_address: u32,
    pub bin_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedPostflash {
    #[serde(with = "hex_u32")]
    pub vendor_id: u32,
    #[serde(with = "hex_u32")]
    pub product_code: u32,
    #[serde(with = "hex_u32")]
    pub revision_number: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    /// Fixed provenance discriminator. This is not a hardware-test claim.
    pub source_kind: String,
    /// Exact whole-file SHA-256 selected by the publishing operator.
    pub artifact_sha256: String,
    pub published_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LatestDocument {
    pub format: String,
    pub profile_id: String,
    #[serde(with = "hex_u32")]
    pub vendor_id: u32,
    #[serde(with = "hex_u32")]
    pub product_code: u32,
    pub channel: String,
    pub sequence: u64,
    pub release_id: String,
    #[serde(with = "hex_u32")]
    pub native_firmware_version: u32,
    pub updated_at_utc: String,
    pub release: ArtifactRef,
    pub catalog_key_id: u32,
    pub catalog_signature: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReleaseError {
    #[error("invalid release JSON: {0}")]
    Json(String),
    #[error("{0}")]
    Invalid(String),
    #[error("catalog P-256 public key is invalid")]
    InvalidPublicKey,
    #[error("catalog P-256 signature is invalid")]
    InvalidSignature,
    #[error("catalog signature verification failed")]
    SignatureMismatch,
    #[error("catalog signing failed")]
    SigningFailed,
}

pub fn identity_root(vendor_id: u32, product_code: u32) -> String {
    format!("dfu/v1/releases/{vendor_id:08x}/{product_code:08x}")
}

pub fn release_id(sequence: u64, native_firmware_version: u32) -> String {
    format!("g{sequence:08}-fw{native_firmware_version:08x}")
}

pub fn release_path(sequence: u64, native_firmware_version: u32) -> String {
    format!(
        "{}/release.json",
        release_id(sequence, native_firmware_version)
    )
}

pub fn latest_json_path(vendor_id: u32, product_code: u32) -> String {
    format!("{}/latest.json", identity_root(vendor_id, product_code))
}

pub fn release_json_path(
    vendor_id: u32,
    product_code: u32,
    sequence: u64,
    native_firmware_version: u32,
) -> String {
    format!(
        "{}/{}",
        identity_root(vendor_id, product_code),
        release_path(sequence, native_firmware_version)
    )
}

pub fn artifact_object_path(
    vendor_id: u32,
    product_code: u32,
    sequence: u64,
    native_firmware_version: u32,
    sha256: &str,
) -> Result<String, ReleaseError> {
    Ok(format!(
        "{}/{}/{}",
        identity_root(vendor_id, product_code),
        release_id(sequence, native_firmware_version),
        artifact_name(sha256)?
    ))
}

pub fn artifact_name(sha256: &str) -> Result<String, ReleaseError> {
    validate_sha("artifact SHA-256", sha256)?;
    Ok(format!("{sha256}.img"))
}

pub fn parse_release_json(bytes: &[u8]) -> Result<ReleaseDocument, ReleaseError> {
    let document: ReleaseDocument =
        serde_json::from_slice(bytes).map_err(|error| ReleaseError::Json(error.to_string()))?;
    validate_release_shape(&document)?;
    Ok(document)
}

pub fn parse_latest_json(bytes: &[u8]) -> Result<LatestDocument, ReleaseError> {
    let document: LatestDocument =
        serde_json::from_slice(bytes).map_err(|error| ReleaseError::Json(error.to_string()))?;
    validate_latest_shape(&document)?;
    Ok(document)
}

/// Return the raw uncompressed SEC1 coordinates (`x || y`) pinned by the
/// build-time catalog.
pub fn raw_public_key(signing_key: &SigningKey) -> [u8; 64] {
    let encoded = VerifyingKey::from(signing_key).to_encoded_point(false);
    encoded.as_bytes()[1..]
        .try_into()
        .expect("an uncompressed P-256 key has 64 coordinate bytes")
}

pub fn sign_release(
    document: &mut ReleaseDocument,
    signing_key: &SigningKey,
) -> Result<(), ReleaseError> {
    let digest = release_digest(document)?;
    let signature: Signature = signing_key
        .sign_prehash(&digest)
        .map_err(|_| ReleaseError::SigningFailed)?;
    let signature = signature.normalize_s().unwrap_or(signature);
    document.catalog_signature = hex::encode(signature.to_bytes());
    Ok(())
}

pub fn sign_latest(
    document: &mut LatestDocument,
    signing_key: &SigningKey,
) -> Result<(), ReleaseError> {
    let digest = latest_digest(document)?;
    let signature: Signature = signing_key
        .sign_prehash(&digest)
        .map_err(|_| ReleaseError::SigningFailed)?;
    let signature = signature.normalize_s().unwrap_or(signature);
    document.catalog_signature = hex::encode(signature.to_bytes());
    Ok(())
}

pub fn verify_release(
    document: &ReleaseDocument,
    expected_profile_id: &str,
    expected_vendor_id: u32,
    expected_product_code: u32,
    expected_key_id: u32,
    raw_public_key: &[u8; 64],
) -> Result<(), ReleaseError> {
    validate_binding(
        "release",
        CatalogBinding {
            profile_id: &document.profile_id,
            vendor_id: document.vendor_id,
            product_code: document.product_code,
            key_id: document.catalog_key_id,
        },
        CatalogBinding {
            profile_id: expected_profile_id,
            vendor_id: expected_vendor_id,
            product_code: expected_product_code,
            key_id: expected_key_id,
        },
    )?;
    verify_digest(
        release_digest(document)?,
        &document.catalog_signature,
        raw_public_key,
    )
}

pub fn verify_latest(
    document: &LatestDocument,
    expected_profile_id: &str,
    expected_vendor_id: u32,
    expected_product_code: u32,
    expected_key_id: u32,
    raw_public_key: &[u8; 64],
) -> Result<(), ReleaseError> {
    validate_binding(
        "latest",
        CatalogBinding {
            profile_id: &document.profile_id,
            vendor_id: document.vendor_id,
            product_code: document.product_code,
            key_id: document.catalog_key_id,
        },
        CatalogBinding {
            profile_id: expected_profile_id,
            vendor_id: expected_vendor_id,
            product_code: expected_product_code,
            key_id: expected_key_id,
        },
    )?;
    verify_digest(
        latest_digest(document)?,
        &document.catalog_signature,
        raw_public_key,
    )
}

pub fn release_digest(document: &ReleaseDocument) -> Result<[u8; 32], ReleaseError> {
    validate_release_shape(document)?;
    let mut transcript = Transcript::new(RELEASE_DOMAIN);
    transcript.string(1, &document.format)?;
    transcript.string(2, &document.profile_id)?;
    transcript.u32(3, document.vendor_id);
    transcript.u32(4, document.product_code);
    transcript.string(5, &document.channel)?;
    transcript.u64(6, document.sequence);
    transcript.string(7, &document.release_id)?;
    transcript.string(9, &document.artifact.path)?;
    transcript.string(10, &document.artifact.sha256)?;
    transcript.u64(11, document.artifact.bytes);
    transcript.u32(12, document.native_img.device_id);
    transcript.u32(13, document.native_img.firmware_id);
    transcript.u32(14, document.native_img.firmware_version);
    transcript.boolean(15, document.native_img.encrypted);
    transcript.string(16, &document.native_img.protected_sha256)?;
    transcript.string(17, &document.native_img.vendor_signature)?;
    transcript.string(18, &document.native_img.iv)?;
    transcript.u32(19, document.native_img.start_address);
    transcript.u32(20, document.native_img.end_address);
    transcript.u64(21, document.native_img.bin_size);
    transcript.u32(22, document.expected_postflash.vendor_id);
    transcript.u32(23, document.expected_postflash.product_code);
    transcript.u32(24, document.expected_postflash.revision_number);
    transcript.string(25, &document.publication.source_kind)?;
    transcript.string(26, &document.publication.artifact_sha256)?;
    transcript.string(27, &document.publication.published_by)?;
    transcript.u32(29, document.catalog_key_id);
    Ok(transcript.finish())
}

pub fn latest_digest(document: &LatestDocument) -> Result<[u8; 32], ReleaseError> {
    validate_latest_shape(document)?;
    let mut transcript = Transcript::new(LATEST_DOMAIN);
    transcript.string(1, &document.format)?;
    transcript.string(2, &document.profile_id)?;
    transcript.u32(3, document.vendor_id);
    transcript.u32(4, document.product_code);
    transcript.string(5, &document.channel)?;
    transcript.u64(6, document.sequence);
    transcript.string(7, &document.release_id)?;
    transcript.u32(8, document.native_firmware_version);
    transcript.string(9, &document.updated_at_utc)?;
    transcript.string(10, &document.release.path)?;
    transcript.string(11, &document.release.sha256)?;
    transcript.u64(12, document.release.bytes);
    transcript.u32(13, document.catalog_key_id);
    Ok(transcript.finish())
}

pub fn validate_release_shape(document: &ReleaseDocument) -> Result<(), ReleaseError> {
    require_eq("release format", &document.format, RELEASE_FORMAT)?;
    validate_profile(&document.profile_id)?;
    validate_identity(document.vendor_id, document.product_code)?;
    require_eq("release channel", &document.channel, STABLE_CHANNEL)?;
    validate_sequence(document.sequence)?;
    let expected_id = release_id(document.sequence, document.native_img.firmware_version);
    require_eq("release ID", &document.release_id, &expected_id)?;
    validate_sha("artifact SHA-256", &document.artifact.sha256)?;
    let expected_artifact = artifact_name(&document.artifact.sha256)?;
    require_eq("artifact path", &document.artifact.path, &expected_artifact)?;
    if document.artifact.bytes == 0 || document.native_img.bin_size == 0 {
        return invalid("artifact and IMG BIN sizes must be nonzero");
    }
    if document.artifact.bytes != document.native_img.bin_size + 140 {
        return invalid("artifact bytes must equal the 140-byte IMG tag plus Bin_Size");
    }
    if document.native_img.end_address
        != document
            .native_img
            .start_address
            .checked_add(
                u32::try_from(document.native_img.bin_size)
                    .map_err(|_| ReleaseError::Invalid("IMG Bin_Size exceeds u32".into()))?
                    .checked_sub(1)
                    .ok_or_else(|| ReleaseError::Invalid("IMG Bin_Size is zero".into()))?,
            )
            .ok_or_else(|| ReleaseError::Invalid("IMG address range overflows u32".into()))?
    {
        return invalid("IMG end address is not the inclusive start + Bin_Size - 1");
    }
    validate_sha(
        "IMG protected SHA-256",
        &document.native_img.protected_sha256,
    )?;
    if document.native_img.vendor_signature.len() != 128
        || !is_lower_hex(&document.native_img.vendor_signature)
    {
        return invalid("IMG vendor signature must be 128 lowercase hex characters");
    }
    if document.native_img.iv.len() != 32 || !is_lower_hex(&document.native_img.iv) {
        return invalid("IMG IV must be 32 lowercase hex characters");
    }
    validate_identity(
        document.expected_postflash.vendor_id,
        document.expected_postflash.product_code,
    )?;
    if document.expected_postflash.revision_number == u32::MAX {
        return invalid("expected post-flash revision must not be the 0xFFFFFFFF sentinel");
    }
    require_eq(
        "publication source kind",
        &document.publication.source_kind,
        SUPPLIER_IMG_SOURCE_KIND,
    )?;
    validate_sha(
        "publication artifact SHA-256",
        &document.publication.artifact_sha256,
    )?;
    if document.publication.artifact_sha256 != document.artifact.sha256 {
        return invalid("publication SHA-256 must equal the bound artifact SHA-256");
    }
    validate_text(
        "publication operator",
        &document.publication.published_by,
        1,
        256,
    )?;
    if document.catalog_key_id == 0 {
        return invalid("catalog key ID must be nonzero");
    }
    validate_optional_signature(&document.catalog_signature)
}

pub fn validate_latest_shape(document: &LatestDocument) -> Result<(), ReleaseError> {
    require_eq("latest format", &document.format, LATEST_FORMAT)?;
    validate_profile(&document.profile_id)?;
    validate_identity(document.vendor_id, document.product_code)?;
    require_eq("latest channel", &document.channel, STABLE_CHANNEL)?;
    validate_sequence(document.sequence)?;
    let expected_id = release_id(document.sequence, document.native_firmware_version);
    require_eq("latest release ID", &document.release_id, &expected_id)?;
    let expected_path = release_path(document.sequence, document.native_firmware_version);
    require_eq(
        "latest release path",
        &document.release.path,
        &expected_path,
    )?;
    validate_sha("latest release SHA-256", &document.release.sha256)?;
    if document.release.bytes == 0 {
        return invalid("latest release bytes must be nonzero");
    }
    validate_text("latest update time", &document.updated_at_utc, 1, 64)?;
    if document.catalog_key_id == 0 {
        return invalid("catalog key ID must be nonzero");
    }
    validate_optional_signature(&document.catalog_signature)
}

#[derive(Clone, Copy)]
struct CatalogBinding<'a> {
    profile_id: &'a str,
    vendor_id: u32,
    product_code: u32,
    key_id: u32,
}

fn validate_binding(
    label: &str,
    actual: CatalogBinding<'_>,
    expected: CatalogBinding<'_>,
) -> Result<(), ReleaseError> {
    if actual.profile_id != expected.profile_id
        || actual.vendor_id != expected.vendor_id
        || actual.product_code != expected.product_code
        || actual.key_id != expected.key_id
    {
        return invalid(format!(
            "{label} is not bound to the selected local profile, identity and catalog key"
        ));
    }
    Ok(())
}

fn verify_digest(
    digest: [u8; 32],
    signature_hex: &str,
    raw_public_key: &[u8; 64],
) -> Result<(), ReleaseError> {
    if signature_hex.len() != 128 || !is_lower_hex(signature_hex) {
        return Err(ReleaseError::InvalidSignature);
    }
    let signature_bytes = hex::decode(signature_hex).map_err(|_| ReleaseError::InvalidSignature)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| ReleaseError::InvalidSignature)?;
    if signature.normalize_s().is_some() {
        return Err(ReleaseError::InvalidSignature);
    }
    let mut sec1 = [0u8; 65];
    sec1[0] = 0x04;
    sec1[1..].copy_from_slice(raw_public_key);
    let key = VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| ReleaseError::InvalidPublicKey)?;
    key.verify_prehash(&digest, &signature)
        .map_err(|_| ReleaseError::SignatureMismatch)
}

fn validate_profile(value: &str) -> Result<(), ReleaseError> {
    validate_text("profile ID", value, 1, 128)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return invalid("profile ID must use lowercase ASCII letters, digits and hyphens");
    }
    Ok(())
}

fn validate_identity(vendor_id: u32, product_code: u32) -> Result<(), ReleaseError> {
    if matches!(vendor_id, 0 | u32::MAX) || matches!(product_code, 0 | u32::MAX) {
        return invalid("catalog identity contains zero or sentinel values");
    }
    Ok(())
}

fn validate_sequence(sequence: u64) -> Result<(), ReleaseError> {
    if !(1..=99_999_999).contains(&sequence) {
        return invalid("catalog sequence must be within 1..=99999999");
    }
    Ok(())
}

fn validate_sha(label: &str, value: &str) -> Result<(), ReleaseError> {
    if value.len() != 64 || !is_lower_hex(value) {
        return invalid(format!(
            "{label} must be 64 lowercase hexadecimal characters"
        ));
    }
    Ok(())
}

fn validate_optional_signature(value: &str) -> Result<(), ReleaseError> {
    if !value.is_empty() && (value.len() != 128 || !is_lower_hex(value)) {
        return invalid(
            "catalog signature must be empty while signing or 128 lowercase hex characters",
        );
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, min: usize, max: usize) -> Result<(), ReleaseError> {
    if !(min..=max).contains(&value.len()) || value.chars().any(|character| character.is_control())
    {
        return invalid(format!(
            "{label} has an invalid length or control character"
        ));
    }
    Ok(())
}

fn require_eq(label: &str, actual: &str, expected: &str) -> Result<(), ReleaseError> {
    if actual != expected {
        return invalid(format!("{label} must be {expected:?}"));
    }
    Ok(())
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid<T>(message: impl Into<String>) -> Result<T, ReleaseError> {
    Err(ReleaseError::Invalid(message.into()))
}

struct Transcript {
    bytes: Vec<u8>,
}

impl Transcript {
    fn new(domain: &[u8]) -> Self {
        Self {
            bytes: domain.to_vec(),
        }
    }

    fn string(&mut self, tag: u16, value: &str) -> Result<(), ReleaseError> {
        let length = u32::try_from(value.len())
            .map_err(|_| ReleaseError::Invalid("transcript string is too large".into()))?;
        self.bytes.extend_from_slice(&tag.to_be_bytes());
        self.bytes.extend_from_slice(&length.to_be_bytes());
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn u32(&mut self, tag: u16, value: u32) {
        self.bytes.extend_from_slice(&tag.to_be_bytes());
        self.bytes.extend_from_slice(&4u32.to_be_bytes());
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, tag: u16, value: u64) {
        self.bytes.extend_from_slice(&tag.to_be_bytes());
        self.bytes.extend_from_slice(&8u32.to_be_bytes());
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn boolean(&mut self, tag: u16, value: bool) {
        self.bytes.extend_from_slice(&tag.to_be_bytes());
        self.bytes.extend_from_slice(&1u32.to_be_bytes());
        self.bytes.push(u8::from(value));
    }

    fn finish(self) -> [u8; 32] {
        Sha256::digest(self.bytes).into()
    }
}

mod hex_u32 {
    use serde::{de, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{value:08X}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() != 10
            || !value.starts_with("0x")
            || !value[2..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
        {
            return Err(de::Error::custom(
                "expected canonical 0xXXXXXXXX uppercase u32",
            ));
        }
        u32::from_str_radix(&value[2..], 16).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents() -> (ReleaseDocument, LatestDocument) {
        let sha = "51af1058197a0df08381a05e19fb8ed4ada8b6988492d280b5c9d650d8c7bf58";
        let release = ReleaseDocument {
            format: RELEASE_FORMAT.into(),
            profile_id: "custom-motor-4310-v1".into(),
            vendor_id: 0x0068_6578,
            product_code: 0x6C64_BC78,
            channel: STABLE_CHANNEL.into(),
            sequence: 1,
            release_id: release_id(1, 0x6578_0001),
            artifact: ArtifactRef {
                path: artifact_name(sha).unwrap(),
                sha256: sha.into(),
                bytes: 177_212,
            },
            native_img: NativeImg {
                device_id: 0xAAAA_0001,
                firmware_id: 0x2025_1025,
                firmware_version: 0x6578_0001,
                encrypted: true,
                protected_sha256:
                    "d8287c3d91f7e62e9892620ff907c4123e3984cc8c9b6f64115c93778cec4c9e"
                        .into(),
                vendor_signature: "48c63135e816af2ff5b68ba85f2486b8e4bc0b4daf5e1993a9eacbabe7d7a3ef6963e0409fc69eb8b7b10dfd33ff86315baa2573651678bfb2c8491ac471c762".into(),
                iv: "c50ea9257e125716be61255bbe9fc926".into(),
                start_address: 0x1000_C000,
                end_address: 0x1003_73AF,
                bin_size: 177_072,
            },
            expected_postflash: ExpectedPostflash {
                vendor_id: 0x0068_6578,
                product_code: 0x6C64_BC78,
                revision_number: 0x6578_0001,
            },
            publication: Publication {
                source_kind: SUPPLIER_IMG_SOURCE_KIND.into(),
                artifact_sha256: sha.into(),
                published_by: "HexMeow release operator".into(),
            },
            catalog_key_id: 7,
            catalog_signature: String::new(),
        };
        let latest = LatestDocument {
            format: LATEST_FORMAT.into(),
            profile_id: release.profile_id.clone(),
            vendor_id: release.vendor_id,
            product_code: release.product_code,
            channel: STABLE_CHANNEL.into(),
            sequence: release.sequence,
            release_id: release.release_id.clone(),
            native_firmware_version: release.native_img.firmware_version,
            updated_at_utc: "2026-08-11T12:00:00Z".into(),
            release: ArtifactRef {
                path: release_path(release.sequence, release.native_img.firmware_version),
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                bytes: 1_234,
            },
            catalog_key_id: release.catalog_key_id,
            catalog_signature: String::new(),
        };
        (release, latest)
    }

    #[test]
    fn canonical_paths_use_identity_sequence_raw_version_and_content_hash() {
        assert_eq!(
            identity_root(0x0068_6578, 0x6C64_BC78),
            "dfu/v1/releases/00686578/6c64bc78"
        );
        assert_eq!(release_id(12, 0x6578_0001), "g00000012-fw65780001");
        assert_eq!(
            release_path(12, 0x6578_0001),
            "g00000012-fw65780001/release.json"
        );
    }

    #[test]
    fn signatures_bind_every_identity_and_artifact_field_without_json_order() {
        let signing_key = SigningKey::from_slice(&[3u8; 32]).unwrap();
        let point = VerifyingKey::from(&signing_key).to_encoded_point(false);
        let raw_key: [u8; 64] = point.as_bytes()[1..].try_into().unwrap();
        let (mut release, mut latest) = documents();
        sign_release(&mut release, &signing_key).unwrap();
        sign_latest(&mut latest, &signing_key).unwrap();
        verify_release(
            &release,
            &release.profile_id,
            release.vendor_id,
            release.product_code,
            release.catalog_key_id,
            &raw_key,
        )
        .unwrap();
        verify_latest(
            &latest,
            &latest.profile_id,
            latest.vendor_id,
            latest.product_code,
            latest.catalog_key_id,
            &raw_key,
        )
        .unwrap();

        let bytes = serde_json::to_vec(&release).unwrap();
        let decoded: ReleaseDocument = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, release);
        verify_release(
            &decoded,
            &decoded.profile_id,
            decoded.vendor_id,
            decoded.product_code,
            decoded.catalog_key_id,
            &raw_key,
        )
        .unwrap();

        release.publication.published_by.push_str("-tampered");
        assert_eq!(
            verify_release(
                &release,
                &release.profile_id,
                release.vendor_id,
                release.product_code,
                release.catalog_key_id,
                &raw_key,
            ),
            Err(ReleaseError::SignatureMismatch)
        );
    }

    #[test]
    fn immutable_release_is_deterministic_across_a_partial_publish_retry() {
        let signing_key = SigningKey::from_slice(&[3u8; 32]).unwrap();
        let (mut first_release, mut first_latest) = documents();
        let mut retry_release = first_release.clone();
        let mut retry_latest = first_latest.clone();

        // A retry occurs later, so only the mutable pointer timestamp changes.
        retry_latest.updated_at_utc = "2026-08-11T12:05:00Z".into();
        sign_release(&mut first_release, &signing_key).unwrap();
        sign_release(&mut retry_release, &signing_key).unwrap();
        sign_latest(&mut first_latest, &signing_key).unwrap();
        sign_latest(&mut retry_latest, &signing_key).unwrap();

        assert_eq!(
            serde_json::to_vec(&first_release).unwrap(),
            serde_json::to_vec(&retry_release).unwrap()
        );
        assert_ne!(
            serde_json::to_vec(&first_latest).unwrap(),
            serde_json::to_vec(&retry_latest).unwrap()
        );
        let value = serde_json::to_value(first_release).unwrap();
        assert!(value.get("created_at_utc").is_none());
        assert!(value["publication"].get("published_at_utc").is_none());
    }

    #[test]
    fn strict_json_and_shape_checks_reject_aliases_and_traversal() {
        let (mut release, mut latest) = documents();
        release.artifact.path = format!("nested/{}", release.artifact.path);
        assert!(validate_release_shape(&release).is_err());
        latest.release.path = "../release.json".into();
        assert!(validate_latest_shape(&latest).is_err());

        let (release, _) = documents();
        let mut value = serde_json::to_value(release).unwrap();
        value["url"] = serde_json::Value::String("https://example.invalid/fw".into());
        assert!(serde_json::from_value::<ReleaseDocument>(value).is_err());

        let (mut release, _) = documents();
        release.publication.source_kind = "hardware-qualified".into();
        assert!(validate_release_shape(&release).is_err());
        let (mut release, _) = documents();
        release.publication.artifact_sha256 = "aa".repeat(32);
        assert!(validate_release_shape(&release).is_err());
    }

    #[test]
    fn canonical_hex_u32_rejects_lowercase_and_numeric_json() {
        let (release, _) = documents();
        let mut value = serde_json::to_value(release).unwrap();
        assert_eq!(value["vendor_id"], "0x00686578");
        value["vendor_id"] = serde_json::Value::String("0x0068657a".into());
        assert!(serde_json::from_value::<ReleaseDocument>(value.clone()).is_err());
        value["vendor_id"] = serde_json::json!(0x0068_6578u32);
        assert!(serde_json::from_value::<ReleaseDocument>(value).is_err());
    }
}
