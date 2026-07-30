use std::collections::HashSet;
use std::time::{Duration, Instant};

use image_container::{
    hardware_revision_compatible, Header, FORMAT_VERSION_V1, FORMAT_VERSION_V2, VENDOR_ID,
};
use p256::ecdsa::{signature::hazmat::PrehashVerifier, Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::identity::{revalidate, AuthorizationError, AuthorizedTarget};
use crate::package::{IntegrityCheckedPackage, Stm32ImageMode};
use crate::transport::SdoTransport;

const UNPROVISIONED: u32 = 0xFFFF_FFFF;
const MAX_DEVICE_NAME_BYTES: usize = 64;
const BOOTLOADER_NAME_PREFIX: &str = "hexmeow-bl-";

/// Artifact policy supported by the safe mutation gate.
///
/// Encrypted v2 binds the product profile to the same raw P-256 public key
/// (`x || y`, without SEC1's `0x04` prefix), key IDs, and security epoch
/// provisioned into its Bootloader. The host authenticates the signed header
/// metadata without possessing the family AES key or decrypting `image.bin`;
/// the Bootloader authenticates every opaque AES-GCM record before writing
/// plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactPolicy {
    UnprotectedV1,
    EncryptedV2 {
        verifying_key: [u8; 64],
        signing_key_id: u32,
        encryption_key_id: u32,
        security_epoch: u32,
    },
}

impl ArtifactPolicy {
    pub const fn encrypted_v2(
        verifying_key: [u8; 64],
        signing_key_id: u32,
        encryption_key_id: u32,
        security_epoch: u32,
    ) -> Self {
        Self::EncryptedV2 {
            verifying_key,
            signing_key_id,
            encryption_key_id,
            security_epoch,
        }
    }
}

/// One logical firmware identity and the exact application names it may boot.
///
/// The name binding closes the ambiguity between multiple firmware variants
/// that share product code, hardware revision, and software revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwarePolicy {
    firmware_id: u32,
    application_names: Vec<String>,
}

impl FirmwarePolicy {
    pub fn new(firmware_id: u32, application_names: Vec<String>) -> Result<Self, ProfileError> {
        if firmware_id == UNPROVISIONED {
            return Err(ProfileError::SentinelValue("firmware_id"));
        }
        if application_names.is_empty() {
            return Err(ProfileError::EmptyApplicationNames { firmware_id });
        }
        let mut seen = HashSet::new();
        for name in &application_names {
            if name.is_empty()
                || name.len() > MAX_DEVICE_NAME_BYTES
                || name.contains('\0')
                || name.starts_with(BOOTLOADER_NAME_PREFIX)
            {
                return Err(ProfileError::InvalidApplicationName {
                    firmware_id,
                    name: name.clone(),
                });
            }
            if !seen.insert(name) {
                return Err(ProfileError::DuplicateApplicationName {
                    firmware_id,
                    name: name.clone(),
                });
            }
        }
        Ok(Self {
            firmware_id,
            application_names,
        })
    }

    pub const fn firmware_id(&self) -> u32 {
        self.firmware_id
    }

    pub fn application_names(&self) -> &[String] {
        &self.application_names
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePolicy {
    mcu: String,
    hardware_versions: Vec<u32>,
    firmware_ids: Vec<u32>,
    firmware_policies: Vec<FirmwarePolicy>,
    artifact_policy: ArtifactPolicy,
}

impl UpgradePolicy {
    pub fn new(
        mcu: impl Into<String>,
        hardware_versions: Vec<u32>,
        firmware_policies: Vec<FirmwarePolicy>,
        artifact_policy: ArtifactPolicy,
    ) -> Result<Self, ProfileError> {
        let mcu = mcu.into();
        let series = mcu_series::series::by_name(&mcu)
            .ok_or_else(|| ProfileError::UnknownMcu(mcu.clone()))?;
        if !matches!(series.name, "stm32g431" | "stm32g474" | "stm32g0b1") {
            return Err(ProfileError::UnknownMcu(mcu));
        }
        validate_exact_values("hardware_versions", &hardware_versions)?;
        let firmware_ids = firmware_policies
            .iter()
            .map(FirmwarePolicy::firmware_id)
            .collect::<Vec<_>>();
        validate_exact_values("firmware_ids", &firmware_ids)?;
        validate_artifact_policy(artifact_policy)?;
        Ok(Self {
            mcu,
            hardware_versions,
            firmware_ids,
            firmware_policies,
            artifact_policy,
        })
    }

    pub fn mcu(&self) -> &str {
        &self.mcu
    }

    pub fn hardware_versions(&self) -> &[u32] {
        &self.hardware_versions
    }

    pub fn firmware_ids(&self) -> &[u32] {
        &self.firmware_ids
    }

    pub fn firmware_policy(&self, firmware_id: u32) -> Option<&FirmwarePolicy> {
        self.firmware_policies
            .iter()
            .find(|policy| policy.firmware_id == firmware_id)
    }

    pub const fn artifact_policy(&self) -> ArtifactPolicy {
        self.artifact_policy
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupportPolicy {
    Disabled { reason: String },
    Enabled(UpgradePolicy),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredTarget {
    profile_id: String,
    vendor_id: u32,
    product_code: u32,
    support: SupportPolicy,
}

impl RegisteredTarget {
    pub fn enabled(
        profile_id: impl Into<String>,
        vendor_id: u32,
        product_code: u32,
        policy: UpgradePolicy,
    ) -> Result<Self, ProfileError> {
        Self::new(
            profile_id.into(),
            vendor_id,
            product_code,
            SupportPolicy::Enabled(policy),
        )
    }

    pub fn disabled(
        profile_id: impl Into<String>,
        vendor_id: u32,
        product_code: u32,
        reason: impl Into<String>,
    ) -> Result<Self, ProfileError> {
        let reason = reason.into();
        if reason.trim().is_empty() {
            return Err(ProfileError::EmptyDisabledReason);
        }
        Self::new(
            profile_id.into(),
            vendor_id,
            product_code,
            SupportPolicy::Disabled { reason },
        )
    }

    fn new(
        profile_id: String,
        vendor_id: u32,
        product_code: u32,
        support: SupportPolicy,
    ) -> Result<Self, ProfileError> {
        if profile_id.trim().is_empty() {
            return Err(ProfileError::EmptyProfileId);
        }
        if vendor_id != VENDOR_ID {
            return Err(ProfileError::ForeignVendor(vendor_id));
        }
        if product_code == UNPROVISIONED {
            return Err(ProfileError::SentinelValue("product_code"));
        }
        Ok(Self {
            profile_id,
            vendor_id,
            product_code,
            support,
        })
    }

    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub const fn vendor_id(&self) -> u32 {
        self.vendor_id
    }

    pub const fn product_code(&self) -> u32 {
        self.product_code
    }

    pub const fn support(&self) -> &SupportPolicy {
        &self.support
    }
}

#[derive(Debug, Clone)]
pub struct TargetRegistry {
    targets: Vec<RegisteredTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetClassification<'a> {
    Enabled(&'a RegisteredTarget),
    Disabled(&'a RegisteredTarget),
    Unknown,
    Sentinel { field: &'static str },
}

impl TargetRegistry {
    pub fn new(targets: Vec<RegisteredTarget>) -> Result<Self, ProfileError> {
        let mut identities = HashSet::new();
        let mut ids = HashSet::new();
        for target in &targets {
            if !identities.insert((target.vendor_id, target.product_code)) {
                return Err(ProfileError::DuplicateIdentity {
                    vendor_id: target.vendor_id,
                    product_code: target.product_code,
                });
            }
            if !ids.insert(target.profile_id.clone()) {
                return Err(ProfileError::DuplicateProfileId(target.profile_id.clone()));
            }
        }
        Ok(Self { targets })
    }

    pub fn targets(&self) -> &[RegisteredTarget] {
        &self.targets
    }

    /// Classify a complete standard identity without touching the bus.
    ///
    /// Software revision and serial are retained for session fixation but do
    /// not choose a product profile. Any sentinel in the full record wins over
    /// a nominal vendor/product match.
    pub fn classify(&self, identity: &crate::IdentitySnapshot) -> TargetClassification<'_> {
        if let Some(field) = identity.first_sentinel_field() {
            return TargetClassification::Sentinel { field };
        }
        let Some(target) = self.targets.iter().find(|target| {
            target.vendor_id == identity.vendor_id()
                && target.product_code == identity.product_code()
        }) else {
            return TargetClassification::Unknown;
        };
        match target.support {
            SupportPolicy::Enabled(_) => TargetClassification::Enabled(target),
            SupportPolicy::Disabled { .. } => TargetClassification::Disabled(target),
        }
    }
}

/// Cloneable state produced while the CAN adapter may be closed.
///
/// It binds a bounded package to the identity found during discovery.  It is
/// deliberately not sufficient to mutate the device: callers must pass it to
/// [`revalidate_prepared`] on a freshly opened bus immediately before the
/// first write.
#[derive(Debug, Clone)]
pub struct PreparedUpgrade {
    target: AuthorizedTarget,
    package: IntegrityCheckedPackage,
}

impl PreparedUpgrade {
    pub fn bind(
        target: AuthorizedTarget,
        package: IntegrityCheckedPackage,
    ) -> Result<Self, ProfileError> {
        validate_binding(&target, &package)?;
        Ok(Self { target, package })
    }

    pub const fn discovered_target(&self) -> &AuthorizedTarget {
        &self.target
    }

    pub const fn package(&self) -> &IntegrityCheckedPackage {
        &self.package
    }
}

/// Final mutation capability produced only by a fresh full identity re-read.
#[derive(Debug)]
pub struct ReadyToFlash {
    target: AuthorizedTarget,
    package: IntegrityCheckedPackage,
    validated_at: Instant,
}

impl ReadyToFlash {
    pub const fn target(&self) -> &AuthorizedTarget {
        &self.target
    }

    pub const fn package(&self) -> &IntegrityCheckedPackage {
        &self.package
    }

    pub fn into_parts(self) -> (AuthorizedTarget, IntegrityCheckedPackage) {
        (self.target, self.package)
    }

    pub fn authorization_age(&self) -> Duration {
        self.validated_at.elapsed()
    }
}

/// Re-open/start gate for the GUI lifecycle.
///
/// The complete `0x1018` snapshot (including serial and software revision) and
/// exact `0x2102` value must still equal discovery.  The package is rebound to
/// the refreshed token before `ReadyToFlash` is returned.
pub async fn revalidate_prepared(
    sdo: &(impl SdoTransport + ?Sized),
    prepared: &PreparedUpgrade,
    registry: &TargetRegistry,
    timeout: Duration,
) -> Result<ReadyToFlash, ReadyError> {
    let target = revalidate(sdo, &prepared.target, registry, timeout).await?;
    validate_binding(&target, &prepared.package)?;
    Ok(ReadyToFlash {
        target,
        package: prepared.package.clone(),
        validated_at: Instant::now(),
    })
}

fn validate_binding(
    target: &AuthorizedTarget,
    package: &IntegrityCheckedPackage,
) -> Result<(), ProfileError> {
    let policy = match target.target().support() {
        SupportPolicy::Enabled(policy) => policy,
        SupportPolicy::Disabled { .. } => return Err(ProfileError::InternalDisabledAuthorization),
    };

    if package.manifest().vendor_id != target.identity().vendor_id() {
        return Err(ProfileError::ArtifactVendorMismatch);
    }
    if package.manifest().product_code != target.identity().product_code() {
        return Err(ProfileError::ArtifactProductMismatch {
            device: target.identity().product_code(),
            artifact: package.manifest().product_code,
        });
    }
    if package.manifest().mcu != policy.mcu {
        return Err(ProfileError::ArtifactMcuMismatch {
            expected: policy.mcu.clone(),
            actual: package.manifest().mcu.clone(),
        });
    }
    if !policy
        .firmware_ids
        .contains(&package.manifest().firmware_id)
    {
        return Err(ProfileError::UnknownFirmwareId(
            package.manifest().firmware_id,
        ));
    }
    if !hardware_revision_compatible(
        package.manifest().min_hardware_rev,
        target.hardware_version(),
    ) {
        return Err(ProfileError::HardwareRequirementMismatch {
            required: package.manifest().min_hardware_rev,
            actual: target.hardware_version(),
        });
    }
    let header = Header::parse(package.header())
        .map_err(|error| ProfileError::Header(format!("{error:?}")))?;
    match (policy.artifact_policy, package.image_mode()) {
        (ArtifactPolicy::UnprotectedV1, Stm32ImageMode::PlaintextV1) => {}
        (ArtifactPolicy::UnprotectedV1, mode) => {
            return Err(ProfileError::ArtifactModeMismatch {
                required: "unprotected v1",
                actual: mode,
            })
        }
        (
            ArtifactPolicy::EncryptedV2 {
                verifying_key,
                signing_key_id,
                encryption_key_id,
                security_epoch,
            },
            Stm32ImageMode::EncryptedV2,
        ) => {
            validate_encrypted_v2(
                &header,
                package,
                &verifying_key,
                signing_key_id,
                encryption_key_id,
                security_epoch,
            )?;
        }
        (ArtifactPolicy::EncryptedV2 { .. }, mode) => {
            return Err(ProfileError::ArtifactModeMismatch {
                required: "signed and AES-GCM-encrypted v2",
                actual: mode,
            })
        }
    }

    match policy.artifact_policy {
        ArtifactPolicy::UnprotectedV1 => {
            debug_assert_eq!(header.format_version(), FORMAT_VERSION_V1);
        }
        ArtifactPolicy::EncryptedV2 { .. } => {
            debug_assert_eq!(header.format_version(), FORMAT_VERSION_V2);
        }
    }

    Ok(())
}

fn validate_artifact_policy(policy: ArtifactPolicy) -> Result<(), ProfileError> {
    if let ArtifactPolicy::EncryptedV2 {
        verifying_key,
        signing_key_id,
        encryption_key_id,
        ..
    } = policy
    {
        parse_verifying_key(&verifying_key)?;
        if signing_key_id == 0 {
            return Err(ProfileError::InvalidSigningKeyIdPolicy);
        }
        if encryption_key_id == 0 {
            return Err(ProfileError::InvalidEncryptionKeyIdPolicy);
        }
    }
    Ok(())
}

fn validate_encrypted_v2(
    header: &Header,
    package: &IntegrityCheckedPackage,
    raw_verifying_key: &[u8; 64],
    expected_signing_key_id: u32,
    expected_encryption_key_id: u32,
    expected_security_epoch: u32,
) -> Result<(), ProfileError> {
    if header.format_version() != FORMAT_VERSION_V2 || !header.flag_encrypted() {
        return Err(ProfileError::ArtifactModeMismatch {
            required: "signed and AES-GCM-encrypted v2",
            actual: package.image_mode(),
        });
    }
    if header.signing_key_id() != expected_signing_key_id {
        return Err(ProfileError::SigningKeyIdMismatch {
            expected: expected_signing_key_id,
            actual: header.signing_key_id(),
        });
    }
    if header.encryption_key_id() != expected_encryption_key_id {
        return Err(ProfileError::EncryptionKeyIdMismatch {
            expected: expected_encryption_key_id,
            actual: header.encryption_key_id(),
        });
    }
    if header.security_epoch() != expected_security_epoch {
        return Err(ProfileError::SecurityEpochMismatch {
            expected: expected_security_epoch,
            actual: header.security_epoch(),
        });
    }
    if package.manifest().key_fingerprint.is_some() {
        return Err(ProfileError::UnexpectedEncryptionKeyFingerprint);
    }
    let declared_fingerprint = package
        .manifest()
        .pubkey_fingerprint
        .as_deref()
        .ok_or(ProfileError::MissingPublicKeyFingerprint)?;
    let expected_fingerprint = sha256_hex(raw_verifying_key);
    if !constant_shape_hex_eq(declared_fingerprint, &expected_fingerprint) {
        return Err(ProfileError::PublicKeyFingerprintMismatch);
    }

    let verifying_key = parse_verifying_key(raw_verifying_key)?;
    let signature = Signature::from_slice(header.signature())
        .map_err(|_| ProfileError::MalformedHeaderSignature)?;
    // Match the Bootloader's canonical low-S acceptance policy.
    if signature.normalize_s().is_some() {
        return Err(ProfileError::NonCanonicalHeaderSignature);
    }
    let digest = header
        .signature_digest()
        .ok_or(ProfileError::MissingHeaderSignatureDigest)?;
    verifying_key
        .verify_prehash(&digest, &signature)
        .map_err(|_| ProfileError::HeaderSignatureVerificationFailed)
}

fn parse_verifying_key(raw: &[u8; 64]) -> Result<VerifyingKey, ProfileError> {
    let mut sec1 = [0u8; 65];
    sec1[0] = 0x04;
    sec1[1..].copy_from_slice(raw);
    VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| ProfileError::InvalidVerifyingKey)
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

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProfileError {
    #[error("profile_id must not be empty")]
    EmptyProfileId,
    #[error("a disabled profile must include a reason")]
    EmptyDisabledReason,
    #[error("this backend only accepts hexmeow vendor 0x{VENDOR_ID:08X}, got 0x{0:08X}")]
    ForeignVendor(u32),
    #[error("{0} must not contain the unprovisioned sentinel")]
    SentinelValue(&'static str),
    #[error("{field} must contain at least one exact value")]
    EmptyExactSet { field: &'static str },
    #[error("{field} contains duplicate exact value 0x{value:08X}")]
    DuplicateExactValue { field: &'static str, value: u32 },
    #[error("firmware id 0x{firmware_id:08X} must list at least one exact application name")]
    EmptyApplicationNames { firmware_id: u32 },
    #[error("invalid application name {name:?} for firmware id 0x{firmware_id:08X}")]
    InvalidApplicationName { firmware_id: u32, name: String },
    #[error("duplicate application name {name:?} for firmware id 0x{firmware_id:08X}")]
    DuplicateApplicationName { firmware_id: u32, name: String },
    #[error("unknown or non-STM32 MCU {0:?}")]
    UnknownMcu(String),
    #[error("encrypted-v2 profile contains an invalid raw P-256 public key")]
    InvalidVerifyingKey,
    #[error("encrypted-v2 profile signing_key_id must be nonzero")]
    InvalidSigningKeyIdPolicy,
    #[error("encrypted-v2 profile encryption_key_id must be nonzero")]
    InvalidEncryptionKeyIdPolicy,
    #[error("duplicate target identity vendor=0x{vendor_id:08X}, product=0x{product_code:08X}")]
    DuplicateIdentity { vendor_id: u32, product_code: u32 },
    #[error("duplicate profile id {0:?}")]
    DuplicateProfileId(String),
    #[error("disabled profile unexpectedly produced an authorization token")]
    InternalDisabledAuthorization,
    #[error("artifact vendor does not match the authorized device")]
    ArtifactVendorMismatch,
    #[error("artifact product 0x{artifact:08X} does not match device product 0x{device:08X}")]
    ArtifactProductMismatch { device: u32, artifact: u32 },
    #[error("artifact MCU {actual:?} does not match profile MCU {expected:?}")]
    ArtifactMcuMismatch { expected: String, actual: String },
    #[error("firmware id 0x{0:08X} is not listed by the local target profile")]
    UnknownFirmwareId(u32),
    #[error("artifact requires hardware 0x{required:08X}, device reports 0x{actual:08X}")]
    HardwareRequirementMismatch { required: u32, actual: u32 },
    #[error("artifact mode {actual:?} does not satisfy profile requirement {required}")]
    ArtifactModeMismatch {
        required: &'static str,
        actual: Stm32ImageMode,
    },
    #[error("secure-v2 signing key id mismatch: profile 0x{expected:08X}, header 0x{actual:08X}")]
    SigningKeyIdMismatch { expected: u32, actual: u32 },
    #[error(
        "secure-v2 encryption key id mismatch: profile 0x{expected:08X}, header 0x{actual:08X}"
    )]
    EncryptionKeyIdMismatch { expected: u32, actual: u32 },
    #[error("secure-v2 security epoch mismatch: profile 0x{expected:08X}, header 0x{actual:08X}")]
    SecurityEpochMismatch { expected: u32, actual: u32 },
    #[error("secure-v2 manifests must not disclose an encryption-key fingerprint")]
    UnexpectedEncryptionKeyFingerprint,
    #[error("encrypted-v2 manifest is missing pubkey_fingerprint")]
    MissingPublicKeyFingerprint,
    #[error("manifest pubkey_fingerprint does not match the product profile")]
    PublicKeyFingerprintMismatch,
    #[error("secure-v2 header contains a malformed raw P-256 signature")]
    MalformedHeaderSignature,
    #[error("secure-v2 header signature is not canonical low-S")]
    NonCanonicalHeaderSignature,
    #[error("secure-v2 header did not produce its canonical signature digest")]
    MissingHeaderSignatureDigest,
    #[error("secure-v2 header P-256 signature verification failed")]
    HeaderSignatureVerificationFailed,
    #[error("container header became invalid after package validation: {0}")]
    Header(String),
}

#[derive(Debug, Error)]
pub enum ReadyError {
    #[error(transparent)]
    Authorization(#[from] AuthorizationError),
    #[error(transparent)]
    Profile(#[from] ProfileError),
}

fn validate_exact_values(field: &'static str, values: &[u32]) -> Result<(), ProfileError> {
    if values.is_empty() {
        return Err(ProfileError::EmptyExactSet { field });
    }
    let mut seen = HashSet::new();
    for &value in values {
        if value == UNPROVISIONED {
            return Err(ProfileError::SentinelValue(field));
        }
        if !seen.insert(value) {
            return Err(ProfileError::DuplicateExactValue { field, value });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_policy_requires_nonempty_unique_application_names() {
        assert!(matches!(
            FirmwarePolicy::new(1, Vec::new()),
            Err(ProfileError::EmptyApplicationNames { firmware_id: 1 })
        ));
        assert!(matches!(
            FirmwarePolicy::new(1, vec![String::new()]),
            Err(ProfileError::InvalidApplicationName { firmware_id: 1, .. })
        ));
        assert!(matches!(
            FirmwarePolicy::new(1, vec!["app".to_owned(), "app".to_owned()]),
            Err(ProfileError::DuplicateApplicationName { firmware_id: 1, .. })
        ));
        assert!(matches!(
            FirmwarePolicy::new(1, vec!["hexmeow-bl-stm32g0b1".to_owned()]),
            Err(ProfileError::InvalidApplicationName { firmware_id: 1, .. })
        ));
    }

    #[test]
    fn upgrade_policy_rejects_duplicate_firmware_ids() {
        let firmware = || FirmwarePolicy::new(7, vec!["app".to_owned()]).unwrap();
        let error = UpgradePolicy::new(
            crate::MCU_STM32G0B1,
            vec![0x0001_0000],
            vec![firmware(), firmware()],
            ArtifactPolicy::UnprotectedV1,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ProfileError::DuplicateExactValue {
                field: "firmware_ids",
                value: 7
            }
        ));
    }

    #[test]
    fn encrypted_policy_rejects_invalid_curve_point_at_configuration_time() {
        let error = UpgradePolicy::new(
            crate::MCU_STM32G0B1,
            vec![0x0001_0000],
            vec![FirmwarePolicy::new(0, vec!["app".to_owned()]).unwrap()],
            ArtifactPolicy::encrypted_v2([0u8; 64], 1, 1, 0),
        )
        .unwrap_err();
        assert!(matches!(error, ProfileError::InvalidVerifyingKey));
    }
}
