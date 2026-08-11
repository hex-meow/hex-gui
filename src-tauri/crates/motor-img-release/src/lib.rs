//! Strict identity-bound documents for supplier Motor IMG releases.
//!
//! The v1 trust boundary is the fixed HTTPS origin and its R2 writers. This
//! crate validates canonical paths, hashes, sizes, identities and local
//! profile binding; it deliberately has no second catalog-signature layer.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const RELEASE_FORMAT: &str = "hexmeow-cobs-iap-release/1";
pub const LATEST_FORMAT: &str = "hexmeow-cobs-iap-latest/1";
pub const STABLE_CHANNEL: &str = "stable";
pub const SUPPLIER_IMG_SOURCE_KIND: &str = "supplier-img";

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
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReleaseError {
    #[error("invalid release JSON: {0}")]
    Json(String),
    #[error("{0}")]
    Invalid(String),
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

/// Validate that an HTTPS-fetched release is bound to the exact local profile
/// selected from the build-time registry.
pub fn verify_release(
    document: &ReleaseDocument,
    expected_profile_id: &str,
    expected_vendor_id: u32,
    expected_product_code: u32,
) -> Result<(), ReleaseError> {
    validate_binding(
        "release",
        ProfileBinding {
            profile_id: &document.profile_id,
            vendor_id: document.vendor_id,
            product_code: document.product_code,
        },
        ProfileBinding {
            profile_id: expected_profile_id,
            vendor_id: expected_vendor_id,
            product_code: expected_product_code,
        },
    )
}

/// Validate that an HTTPS-fetched latest pointer is bound to the exact local
/// profile selected from the build-time registry.
pub fn verify_latest(
    document: &LatestDocument,
    expected_profile_id: &str,
    expected_vendor_id: u32,
    expected_product_code: u32,
) -> Result<(), ReleaseError> {
    validate_binding(
        "latest",
        ProfileBinding {
            profile_id: &document.profile_id,
            vendor_id: document.vendor_id,
            product_code: document.product_code,
        },
        ProfileBinding {
            profile_id: expected_profile_id,
            vendor_id: expected_vendor_id,
            product_code: expected_product_code,
        },
    )
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
    )
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
    validate_text("latest update time", &document.updated_at_utc, 1, 64)
}

#[derive(Clone, Copy)]
struct ProfileBinding<'a> {
    profile_id: &'a str,
    vendor_id: u32,
    product_code: u32,
}

fn validate_binding(
    label: &str,
    actual: ProfileBinding<'_>,
    expected: ProfileBinding<'_>,
) -> Result<(), ReleaseError> {
    if actual.profile_id != expected.profile_id
        || actual.vendor_id != expected.vendor_id
        || actual.product_code != expected.product_code
    {
        return invalid(format!(
            "{label} is not bound to the selected local profile and identity"
        ));
    }
    Ok(())
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
    fn https_documents_bind_to_the_exact_local_profile_and_identity() {
        let (release, latest) = documents();
        verify_release(
            &release,
            &release.profile_id,
            release.vendor_id,
            release.product_code,
        )
        .unwrap();
        verify_latest(
            &latest,
            &latest.profile_id,
            latest.vendor_id,
            latest.product_code,
        )
        .unwrap();

        let bytes = serde_json::to_vec(&release).unwrap();
        let decoded: ReleaseDocument = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, release);
        assert!(verify_release(
            &decoded,
            "custom-motor-4342-v1",
            decoded.vendor_id,
            decoded.product_code,
        )
        .is_err());
        assert!(
            verify_latest(&latest, &latest.profile_id, latest.vendor_id, 0x6C64_BCAA,).is_err()
        );
    }

    #[test]
    fn immutable_release_is_deterministic_across_a_partial_publish_retry() {
        let (first_release, first_latest) = documents();
        let retry_release = first_release.clone();
        let mut retry_latest = first_latest.clone();

        // A retry occurs later, so only the mutable pointer timestamp changes.
        retry_latest.updated_at_utc = "2026-08-11T12:05:00Z".into();

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

        let (release, _) = documents();
        let mut value = serde_json::to_value(release).unwrap();
        value["catalog_signature"] = serde_json::Value::String("00".repeat(64));
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
