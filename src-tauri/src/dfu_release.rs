//! Fixed-origin HTTPS discovery for protocol-native DFU artifacts.
//!
//! Device backends supply only a compile-time release identity and expected
//! file extension. Remote documents may select immutable bytes, but they
//! cannot provide an origin, redirect, device identity, backend, or key.

use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub(crate) const MAX_LATEST_BYTES: usize = 16 * 1024;
pub(crate) const MAX_RELEASE_BYTES: usize = 64 * 1024;
pub(crate) const R2_ORIGIN: &str = "https://downloads.hexmeow.com";

const LATEST_FORMAT: &str = "hexmeow-dfu-latest/1";
const RELEASE_FORMAT: &str = "hexmeow-dfu-release/1";
const R2_CHANNEL: &str = "stable";
const R2_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const R2_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy)]
pub(crate) struct ReleaseSpec<'a> {
    pub profile_id: &'a str,
    pub vendor_id: u32,
    pub product_code: u32,
    pub artifact_extension: &'a str,
    pub max_artifact_bytes: usize,
}

pub(crate) struct DownloadedArtifact {
    pub version: String,
    /// Canonical `M.m.0` encoded as `major << 16 | minor` for backends whose
    /// native format defines that mapping. Other backends treat the version
    /// string as release metadata only.
    pub release_version_code: u32,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub size: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LatestDocument {
    format: String,
    profile_id: String,
    vendor_id: String,
    product_code: String,
    channel: String,
    version: String,
    release: LatestReleaseRef,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LatestReleaseRef {
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseDocument {
    format: String,
    profile_id: String,
    vendor_id: String,
    product_code: String,
    version: String,
    tag: String,
    candidate_source_commit: String,
    artifact: ReleaseArtifactRef,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseArtifactRef {
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug)]
struct ValidatedLatest {
    version: String,
    release_version_code: u32,
    release_path: String,
    release_sha256: String,
    release_bytes: usize,
}

#[derive(Debug)]
struct ValidatedRelease {
    artifact_path: String,
    artifact_sha256: String,
    artifact_bytes: usize,
}

pub(crate) async fn download_latest(spec: ReleaseSpec<'_>) -> Result<DownloadedArtifact> {
    validate_spec(spec)?;
    let identity_url = identity_url(spec.vendor_id, spec.product_code);
    let client = r2_client()?;

    let latest_url = format!("{identity_url}/latest.json");
    let latest_bytes =
        fetch_bounded(&client, &latest_url, MAX_LATEST_BYTES, "latest pointer").await?;
    let latest = parse_latest_document(&latest_bytes, spec)?;

    let release_url = format!("{identity_url}/{}", latest.release_path);
    let release_bytes =
        fetch_bounded(&client, &release_url, MAX_RELEASE_BYTES, "release binding").await?;
    verify_bound_bytes(
        "release binding",
        &release_bytes,
        latest.release_bytes,
        &latest.release_sha256,
    )?;
    let release = parse_release_document(&release_bytes, &latest, spec)?;

    let artifact_url = format!(
        "{identity_url}/{}/{}",
        latest.version, release.artifact_path
    );
    let bytes = fetch_bounded(
        &client,
        &artifact_url,
        spec.max_artifact_bytes,
        "DFU artifact",
    )
    .await?;
    verify_bound_bytes(
        "DFU artifact",
        &bytes,
        release.artifact_bytes,
        &release.artifact_sha256,
    )?;

    Ok(DownloadedArtifact {
        version: latest.version,
        release_version_code: latest.release_version_code,
        bytes,
        sha256: release.artifact_sha256,
        size: release.artifact_bytes,
    })
}

pub(crate) fn identity_url(vendor_id: u32, product_code: u32) -> String {
    format!("{R2_ORIGIN}/dfu/v1/releases/{vendor_id:08x}/{product_code:08x}")
}

pub(crate) fn r2_client() -> Result<reqwest::Client> {
    // `rustls-no-provider` avoids AWS-LC/CMake. This application pins ring and
    // installs it once before constructing any HTTPS client.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        return Err("cannot install the rustls crypto provider".into());
    }

    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(R2_CONNECT_TIMEOUT)
        .timeout(R2_REQUEST_TIMEOUT)
        .user_agent(concat!("hexmeow-gui/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("building the R2 HTTPS client: {error}"))
}

pub(crate) async fn fetch_bounded(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("downloading {label} from R2: {error}"))?;
    if response.url().as_str() != url {
        return Err(format!("{label} response changed the fixed R2 URL"));
    }
    if !response.status().is_success() {
        return Err(format!(
            "{label} is unavailable at the fixed R2 path (HTTP {})",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!(
            "{label} Content-Length exceeds the {max_bytes}-byte limit"
        ));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("reading {label} from R2: {error}"))?
    {
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| format!("{label} length overflow"))?;
        if next_len > max_bytes {
            return Err(format!("{label} exceeds the {max_bytes}-byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err(format!("{label} is empty"));
    }
    Ok(bytes)
}

pub(crate) fn bounded_declared_size(label: &str, value: u64, limit: usize) -> Result<usize> {
    let value = usize::try_from(value).map_err(|_| format!("{label} size exceeds usize"))?;
    if value == 0 || value > limit {
        return Err(format!("{label} size must be within 1..={limit} bytes"));
    }
    Ok(value)
}

pub(crate) fn verify_bound_bytes(
    label: &str,
    bytes: &[u8],
    expected_size: usize,
    expected_sha256: &str,
) -> Result<()> {
    if bytes.len() != expected_size {
        return Err(format!(
            "{label} size mismatch: expected {expected_size}, got {}",
            bytes.len()
        ));
    }
    if sha256_hex(bytes) != expected_sha256 {
        return Err(format!("{label} SHA-256 does not match its binding"));
    }
    Ok(())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_spec(spec: ReleaseSpec<'_>) -> Result<()> {
    if spec.profile_id.is_empty() {
        return Err("release profile must not be empty".into());
    }
    if spec.artifact_extension.is_empty()
        || !spec
            .artifact_extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err("artifact extension must be lowercase ASCII alphanumeric".into());
    }
    if spec.max_artifact_bytes == 0 {
        return Err("artifact byte limit must be nonzero".into());
    }
    Ok(())
}

fn parse_latest_document(bytes: &[u8], spec: ReleaseSpec<'_>) -> Result<ValidatedLatest> {
    let document: LatestDocument = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid latest pointer JSON: {error}"))?;
    if document.format != LATEST_FORMAT {
        return Err(format!(
            "unsupported latest pointer format {:?}",
            document.format
        ));
    }
    validate_release_identity(
        "latest pointer",
        &document.profile_id,
        &document.vendor_id,
        &document.product_code,
        spec,
    )?;
    if document.channel != R2_CHANNEL {
        return Err(format!(
            "unsupported release channel {:?}",
            document.channel
        ));
    }
    let release_version_code = parse_release_version(&document.version)?;
    let expected_release_path = format!("{}/release.json", document.version);
    if document.release.path != expected_release_path {
        return Err(format!(
            "latest release path must be {expected_release_path:?}"
        ));
    }
    validate_lower_sha256("latest release SHA-256", &document.release.sha256)?;
    let release_bytes = bounded_declared_size(
        "latest release binding",
        document.release.bytes,
        MAX_RELEASE_BYTES,
    )?;
    Ok(ValidatedLatest {
        version: document.version,
        release_version_code,
        release_path: document.release.path,
        release_sha256: document.release.sha256,
        release_bytes,
    })
}

fn parse_release_document(
    bytes: &[u8],
    latest: &ValidatedLatest,
    spec: ReleaseSpec<'_>,
) -> Result<ValidatedRelease> {
    let document: ReleaseDocument = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid release binding JSON: {error}"))?;
    if document.format != RELEASE_FORMAT {
        return Err(format!(
            "unsupported release binding format {:?}",
            document.format
        ));
    }
    validate_release_identity(
        "release binding",
        &document.profile_id,
        &document.vendor_id,
        &document.product_code,
        spec,
    )?;
    if document.version != latest.version {
        return Err("release binding version does not match latest pointer".into());
    }
    if parse_release_version(&document.version)? != latest.release_version_code {
        return Err("release binding version encoding changed after pointer validation".into());
    }
    if document.tag != format!("v{}", latest.version) {
        return Err("release binding tag does not match its canonical version".into());
    }
    if !matches!(document.candidate_source_commit.len(), 40 | 64)
        || !document
            .candidate_source_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("release binding contains an invalid source commit".into());
    }
    validate_lower_sha256("release artifact SHA-256", &document.artifact.sha256)?;
    let expected_artifact_path =
        format!("{}.{}", document.artifact.sha256, spec.artifact_extension);
    if document.artifact.path != expected_artifact_path {
        return Err(format!(
            "release artifact path must be the single filename {expected_artifact_path:?}"
        ));
    }
    let artifact_bytes = bounded_declared_size(
        "release artifact",
        document.artifact.bytes,
        spec.max_artifact_bytes,
    )?;
    Ok(ValidatedRelease {
        artifact_path: document.artifact.path,
        artifact_sha256: document.artifact.sha256,
        artifact_bytes,
    })
}

fn validate_release_identity(
    label: &str,
    profile_id: &str,
    vendor_id: &str,
    product_code: &str,
    spec: ReleaseSpec<'_>,
) -> Result<()> {
    if profile_id != spec.profile_id {
        return Err(format!(
            "{label} profile does not match the selected local profile"
        ));
    }
    if vendor_id != format!("0x{:08X}", spec.vendor_id)
        || product_code != format!("0x{:08X}", spec.product_code)
    {
        return Err(format!(
            "{label} identity does not match the selected device"
        ));
    }
    Ok(())
}

fn parse_release_version(version: &str) -> Result<u32> {
    if version.contains('-') || version.contains('+') {
        return Err("release version must not contain pre-release or build metadata".into());
    }
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
        || parts[2] != "0"
    {
        return Err("release version must be canonical M.m.0 SemVer".into());
    }
    let major = parts[0]
        .parse::<u16>()
        .map_err(|_| "release major version exceeds u16".to_owned())?;
    let minor = parts[1]
        .parse::<u16>()
        .map_err(|_| "release minor version exceeds u16".to_owned())?;
    Ok((u32::from(major) << 16) | u32::from(minor))
}

fn validate_lower_sha256(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "{label} must be 64 lowercase hexadecimal characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PROFILE: &str = "gs-can-hpm-usb-v1";
    const TEST_VENDOR: u32 = 0x6865_786D;
    const TEST_PRODUCT: u32 = 0x6763_616E;
    const RELEASE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ARTIFACT_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn spec() -> ReleaseSpec<'static> {
        ReleaseSpec {
            profile_id: TEST_PROFILE,
            vendor_id: TEST_VENDOR,
            product_code: TEST_PRODUCT,
            artifact_extension: "hpmota",
            max_artifact_bytes: 2 * 1024 * 1024,
        }
    }

    fn latest_bytes(path: &str, version: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "format": LATEST_FORMAT,
            "profile_id": TEST_PROFILE,
            "vendor_id": "0x6865786D",
            "product_code": "0x6763616E",
            "channel": R2_CHANNEL,
            "version": version,
            "release": {
                "path": path,
                "sha256": RELEASE_SHA,
                "bytes": 321
            }
        }))
        .unwrap()
    }

    fn release_bytes(path: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "format": RELEASE_FORMAT,
            "profile_id": TEST_PROFILE,
            "vendor_id": "0x6865786D",
            "product_code": "0x6763616E",
            "version": "1.2.0",
            "tag": "v1.2.0",
            "candidate_source_commit":
                "cccccccccccccccccccccccccccccccccccccccc",
            "artifact": {
                "path": path,
                "sha256": ARTIFACT_SHA,
                "bytes": 1024
            }
        }))
        .unwrap()
    }

    #[test]
    fn literal_identity_url_is_lowercase_and_fixed_origin() {
        assert_eq!(
            identity_url(TEST_VENDOR, TEST_PRODUCT),
            "https://downloads.hexmeow.com/dfu/v1/releases/6865786d/6763616e"
        );
    }

    #[test]
    fn https_client_uses_the_process_crypto_provider() {
        r2_client().expect("fixed HTTPS client");
    }

    #[test]
    fn documents_accept_only_literal_identity_and_content_addressed_path() {
        let latest =
            parse_latest_document(&latest_bytes("1.2.0/release.json", "1.2.0"), spec()).unwrap();
        assert_eq!(latest.version, "1.2.0");
        assert_eq!(latest.release_version_code, 0x0001_0002);

        let release = parse_release_document(
            &release_bytes(&format!("{ARTIFACT_SHA}.hpmota")),
            &latest,
            spec(),
        )
        .unwrap();
        assert_eq!(release.artifact_path, format!("{ARTIFACT_SHA}.hpmota"));
        assert_eq!(release.artifact_bytes, 1024);
    }

    #[test]
    fn latest_rejects_noncanonical_version_traversal_and_unknown_fields() {
        assert!(
            parse_latest_document(&latest_bytes("01.2.0/release.json", "01.2.0"), spec(),).is_err()
        );
        assert!(
            parse_latest_document(&latest_bytes("../1.2.0/release.json", "1.2.0"), spec(),)
                .is_err()
        );

        let mut document: serde_json::Value =
            serde_json::from_slice(&latest_bytes("1.2.0/release.json", "1.2.0")).unwrap();
        document["url"] = serde_json::Value::String("https://example.invalid/fw".into());
        assert!(parse_latest_document(&serde_json::to_vec(&document).unwrap(), spec()).is_err());
    }

    #[test]
    fn release_rejects_nested_wrong_extension_or_sha_unbound_names() {
        let latest =
            parse_latest_document(&latest_bytes("1.2.0/release.json", "1.2.0"), spec()).unwrap();
        for path in [
            format!("nested/{ARTIFACT_SHA}.hpmota"),
            format!("{RELEASE_SHA}.hpmota"),
            format!("{ARTIFACT_SHA}.bin"),
        ] {
            assert!(parse_release_document(&release_bytes(&path), &latest, spec()).is_err());
        }
    }

    #[test]
    fn the_same_parser_preserves_the_stm32_meowpkg_contract() {
        let stm32_spec = ReleaseSpec {
            profile_id: "lift-g0b1-v1",
            vendor_id: 0x6865_786D,
            product_code: 0x006C_6674,
            artifact_extension: "meowpkg",
            max_artifact_bytes: 2 * 1024 * 1024,
        };
        let latest_bytes = serde_json::to_vec(&serde_json::json!({
            "format": LATEST_FORMAT,
            "profile_id": "lift-g0b1-v1",
            "vendor_id": "0x6865786D",
            "product_code": "0x006C6674",
            "channel": R2_CHANNEL,
            "version": "1.2.0",
            "release": {
                "path": "1.2.0/release.json",
                "sha256": RELEASE_SHA,
                "bytes": 321
            }
        }))
        .unwrap();
        let latest = parse_latest_document(&latest_bytes, stm32_spec).unwrap();
        let release_bytes = serde_json::to_vec(&serde_json::json!({
            "format": RELEASE_FORMAT,
            "profile_id": "lift-g0b1-v1",
            "vendor_id": "0x6865786D",
            "product_code": "0x006C6674",
            "version": "1.2.0",
            "tag": "v1.2.0",
            "candidate_source_commit":
                "cccccccccccccccccccccccccccccccccccccccc",
            "artifact": {
                "path": format!("{ARTIFACT_SHA}.meowpkg"),
                "sha256": ARTIFACT_SHA,
                "bytes": 1024
            }
        }))
        .unwrap();
        let release = parse_release_document(&release_bytes, &latest, stm32_spec).unwrap();
        assert_eq!(release.artifact_path, format!("{ARTIFACT_SHA}.meowpkg"));
    }

    #[test]
    fn bound_download_requires_exact_size_and_sha256() {
        let bytes = b"exact release bytes";
        let sha = sha256_hex(bytes);
        verify_bound_bytes("test", bytes, bytes.len(), &sha).unwrap();
        assert!(verify_bound_bytes("test", bytes, bytes.len() + 1, &sha).is_err());
        assert!(verify_bound_bytes("test", bytes, bytes.len(), RELEASE_SHA).is_err());
    }
}
