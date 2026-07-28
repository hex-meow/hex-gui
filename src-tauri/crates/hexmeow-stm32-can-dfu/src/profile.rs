use std::collections::HashSet;
use std::time::{Duration, Instant};

use image_container::{hardware_revision_compatible, Header, FORMAT_VERSION_V1, VENDOR_ID};
use thiserror::Error;

use crate::identity::{revalidate, AuthorizationError, AuthorizedTarget};
use crate::package::{IntegrityCheckedPackage, Stm32ImageMode};
use crate::transport::SdoTransport;

const UNPROVISIONED: u32 = 0xFFFF_FFFF;

/// Artifact policy supported by the safe mutation gate.
///
/// Secure v2 files are structurally parsed, but are not admitted here until a
/// signed-catalog proof binds the opaque wire digest to a trusted release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactPolicy {
    UnprotectedV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePolicy {
    mcu: String,
    hardware_versions: Vec<u32>,
    firmware_ids: Vec<u32>,
    artifact_policy: ArtifactPolicy,
}

impl UpgradePolicy {
    pub fn new(
        mcu: impl Into<String>,
        hardware_versions: Vec<u32>,
        firmware_ids: Vec<u32>,
        artifact_policy: ArtifactPolicy,
    ) -> Result<Self, ProfileError> {
        let mcu = mcu.into();
        let series = mcu_series::series::by_name(&mcu)
            .ok_or_else(|| ProfileError::UnknownMcu(mcu.clone()))?;
        if !matches!(series.name, "stm32g431" | "stm32g474" | "stm32g0b1") {
            return Err(ProfileError::UnknownMcu(mcu));
        }
        validate_exact_values("hardware_versions", &hardware_versions)?;
        validate_exact_values("firmware_ids", &firmware_ids)?;
        Ok(Self {
            mcu,
            hardware_versions,
            firmware_ids,
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
    match (policy.artifact_policy, package.image_mode()) {
        (ArtifactPolicy::UnprotectedV1, Stm32ImageMode::PlaintextV1) => {}
        (ArtifactPolicy::UnprotectedV1, mode) => {
            return Err(ProfileError::SecureArtifactNeedsCatalog(mode))
        }
    }

    let header = Header::parse(package.header())
        .map_err(|error| ProfileError::Header(format!("{error:?}")))?;
    debug_assert_eq!(header.format_version(), FORMAT_VERSION_V1);

    Ok(())
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
    #[error("unknown or non-STM32 MCU {0:?}")]
    UnknownMcu(String),
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
    #[error(
        "{0:?} is only structurally checked; an authenticated catalog descriptor is required before secure v2 streaming"
    )]
    SecureArtifactNeedsCatalog(Stm32ImageMode),
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
