use std::collections::HashSet;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::{CanopenIdentity, EncryptionMode, ImgArtifact};

const READY_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PolicyError {
    #[error("profile ID must not be empty")]
    EmptyProfileId,
    #[error("profile {profile_id} uses invalid {field} value {value:#010x}")]
    InvalidProfileIdentity {
        profile_id: String,
        field: &'static str,
        value: u32,
    },
    #[error("profile {0} must allow at least one firmware ID")]
    EmptyFirmwareIds(String),
    #[error("profile {0} repeats a firmware ID")]
    DuplicateFirmwareId(String),
    #[error("duplicate profile ID {0}")]
    DuplicateProfileId(String),
    #[error("profiles {first} and {second} repeat the same exact CANopen vendor/product identity")]
    DuplicateCanopenIdentity { first: String, second: String },
    #[error("CANopen identity is not in the local update registry")]
    UnknownTarget,
    #[error("profile {profile_id} is known but disabled: {reason}")]
    DisabledTarget { profile_id: String, reason: String },
    #[error("IMG device ID {actual:#010x} does not match profile value {expected:#010x}")]
    DeviceMismatch { expected: u32, actual: u32 },
    #[error("IMG firmware ID {0:#010x} is not allowed by this profile")]
    FirmwareMismatch(u32),
    #[error("IMG start address {actual:#010x} does not match profile value {expected:#010x}")]
    StartAddressMismatch { expected: u32, actual: u32 },
    #[error("this profile requires an encrypted IMG")]
    EncryptionRequired,
    #[error("the CANopen identity changed after artifact validation")]
    IdentityChanged,
    #[error("the local profile changed after artifact validation")]
    ProfileChanged,
    #[error("the same-bus revalidation capability expired before Reset")]
    ReadyExpired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IapPolicy {
    device_id: u32,
    firmware_ids: Vec<u32>,
    start_address: u32,
    require_encrypted: bool,
}

impl IapPolicy {
    pub fn new(
        device_id: u32,
        firmware_ids: Vec<u32>,
        start_address: u32,
        require_encrypted: bool,
    ) -> Result<Self, PolicyError> {
        if firmware_ids.is_empty() {
            return Err(PolicyError::EmptyFirmwareIds("<unbound>".into()));
        }
        let unique = firmware_ids.iter().copied().collect::<HashSet<_>>();
        if unique.len() != firmware_ids.len() {
            return Err(PolicyError::DuplicateFirmwareId("<unbound>".into()));
        }
        Ok(Self {
            device_id,
            firmware_ids,
            start_address,
            require_encrypted,
        })
    }

    pub fn device_id(&self) -> u32 {
        self.device_id
    }

    pub fn firmware_ids(&self) -> &[u32] {
        &self.firmware_ids
    }

    pub fn start_address(&self) -> u32 {
        self.start_address
    }

    pub fn require_encrypted(&self) -> bool {
        self.require_encrypted
    }

    fn validate_artifact(&self, artifact: &ImgArtifact) -> Result<(), PolicyError> {
        if artifact.device_id() != self.device_id {
            return Err(PolicyError::DeviceMismatch {
                expected: self.device_id,
                actual: artifact.device_id(),
            });
        }
        if !self.firmware_ids.contains(&artifact.firmware_id()) {
            return Err(PolicyError::FirmwareMismatch(artifact.firmware_id()));
        }
        if artifact.start_address() != self.start_address {
            return Err(PolicyError::StartAddressMismatch {
                expected: self.start_address,
                actual: artifact.start_address(),
            });
        }
        if self.require_encrypted && artifact.encryption() != EncryptionMode::Encrypted {
            return Err(PolicyError::EncryptionRequired);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupportPolicy {
    Enabled(IapPolicy),
    Disabled { reason: String },
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
        policy: IapPolicy,
    ) -> Result<Self, PolicyError> {
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
    ) -> Result<Self, PolicyError> {
        Self::new(
            profile_id.into(),
            vendor_id,
            product_code,
            SupportPolicy::Disabled {
                reason: reason.into(),
            },
        )
    }

    fn new(
        profile_id: String,
        vendor_id: u32,
        product_code: u32,
        support: SupportPolicy,
    ) -> Result<Self, PolicyError> {
        if profile_id.trim().is_empty() {
            return Err(PolicyError::EmptyProfileId);
        }
        for (field, value) in [("vendor_id", vendor_id), ("product_code", product_code)] {
            if value == 0 || value == u32::MAX {
                return Err(PolicyError::InvalidProfileIdentity {
                    profile_id,
                    field,
                    value,
                });
            }
        }
        if let SupportPolicy::Enabled(policy) = &support {
            if policy.device_id == 0 || policy.device_id == u32::MAX {
                return Err(PolicyError::InvalidProfileIdentity {
                    profile_id,
                    field: "device_id",
                    value: policy.device_id,
                });
            }
            if policy
                .firmware_ids
                .iter()
                .any(|value| *value == 0 || *value == u32::MAX)
            {
                let value = policy
                    .firmware_ids
                    .iter()
                    .copied()
                    .find(|value| *value == 0 || *value == u32::MAX)
                    .unwrap();
                return Err(PolicyError::InvalidProfileIdentity {
                    profile_id,
                    field: "firmware_id",
                    value,
                });
            }
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

    pub fn vendor_id(&self) -> u32 {
        self.vendor_id
    }

    pub fn product_code(&self) -> u32 {
        self.product_code
    }

    pub fn support(&self) -> &SupportPolicy {
        &self.support
    }
}

#[derive(Debug, Clone)]
pub struct TargetRegistry {
    targets: Vec<RegisteredTarget>,
}

impl TargetRegistry {
    pub fn new(targets: Vec<RegisteredTarget>) -> Result<Self, PolicyError> {
        let mut profile_ids = HashSet::new();
        let mut identities = std::collections::HashMap::new();
        for target in &targets {
            if !profile_ids.insert(target.profile_id.clone()) {
                return Err(PolicyError::DuplicateProfileId(target.profile_id.clone()));
            }
            if let Some(first) = identities.insert(
                (target.vendor_id, target.product_code),
                target.profile_id.clone(),
            ) {
                return Err(PolicyError::DuplicateCanopenIdentity {
                    first,
                    second: target.profile_id.clone(),
                });
            }
        }
        Ok(Self { targets })
    }

    pub fn targets(&self) -> &[RegisteredTarget] {
        &self.targets
    }

    pub fn classify(&self, identity: CanopenIdentity) -> TargetClassification {
        let target = self.targets.iter().find(|target| {
            target.vendor_id == identity.vendor_id()
                && target.product_code == identity.product_code()
        });
        match target {
            Some(target) if matches!(target.support, SupportPolicy::Enabled(_)) => {
                TargetClassification::Enabled(target.clone())
            }
            Some(target) => TargetClassification::Disabled(target.clone()),
            None => TargetClassification::Unknown,
        }
    }

    pub fn authorize(&self, identity: CanopenIdentity) -> Result<AuthorizedTarget, PolicyError> {
        match self.classify(identity) {
            TargetClassification::Enabled(target) => Ok(AuthorizedTarget { target, identity }),
            TargetClassification::Disabled(target) => {
                let SupportPolicy::Disabled { reason } = target.support else {
                    unreachable!("classification and policy agree")
                };
                Err(PolicyError::DisabledTarget {
                    profile_id: target.profile_id,
                    reason,
                })
            }
            TargetClassification::Unknown => Err(PolicyError::UnknownTarget),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TargetClassification {
    Enabled(RegisteredTarget),
    Disabled(RegisteredTarget),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct AuthorizedTarget {
    target: RegisteredTarget,
    identity: CanopenIdentity,
}

impl AuthorizedTarget {
    pub fn target(&self) -> &RegisteredTarget {
        &self.target
    }

    pub fn identity(&self) -> CanopenIdentity {
        self.identity
    }
}

#[derive(Debug, Clone)]
pub struct PreparedUpgrade {
    target: AuthorizedTarget,
    artifact: ImgArtifact,
}

impl PreparedUpgrade {
    pub fn bind(target: AuthorizedTarget, artifact: ImgArtifact) -> Result<Self, PolicyError> {
        let SupportPolicy::Enabled(policy) = target.target.support() else {
            return Err(PolicyError::ProfileChanged);
        };
        policy.validate_artifact(&artifact)?;
        Ok(Self { target, artifact })
    }

    pub fn target(&self) -> &AuthorizedTarget {
        &self.target
    }

    pub fn artifact(&self) -> &ImgArtifact {
        &self.artifact
    }

    /// Bind this prepared artifact to a fresh complete identity read from the
    /// exact bus that will immediately be passed to `flash`.
    pub fn revalidate(
        &self,
        actual: CanopenIdentity,
        registry: &TargetRegistry,
    ) -> Result<ReadyToFlash, PolicyError> {
        if actual != self.target.identity {
            return Err(PolicyError::IdentityChanged);
        }
        let authorized = registry.authorize(actual)?;
        if authorized.target != self.target.target {
            return Err(PolicyError::ProfileChanged);
        }
        Ok(ReadyToFlash {
            prepared: self.clone(),
            issued_at: Instant::now(),
        })
    }
}

/// Short-lived proof that the selected device and local policy were reread on
/// the actual update bus. Its fields and constructor are intentionally private.
#[derive(Debug)]
pub struct ReadyToFlash {
    prepared: PreparedUpgrade,
    issued_at: Instant,
}

impl ReadyToFlash {
    pub(crate) fn node_id(&self) -> u8 {
        self.prepared.target.identity.node_id()
    }

    pub(crate) fn into_prepared(self) -> Result<PreparedUpgrade, PolicyError> {
        if self.issued_at.elapsed() > READY_TTL {
            return Err(PolicyError::ReadyExpired);
        }
        Ok(self.prepared)
    }
}

#[cfg(test)]
mod tests {
    use crate::artifact::tests::make_image;
    use crate::{ImgArtifact, ImgLimits};

    use super::*;

    fn identity(product: u32) -> CanopenIdentity {
        CanopenIdentity::new(1, 0x4859_444C, product, 7, 123).unwrap()
    }

    fn enabled() -> RegisteredTarget {
        RegisteredTarget::enabled(
            "compatible-motor-4310-v1",
            0x4859_444C,
            0xAAAA_0001,
            IapPolicy::new(0xAAAA_0001, vec![0x2025_1025], 0x1000_C000, true).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn registry_is_exact_and_deny_by_default() {
        let registry = TargetRegistry::new(vec![enabled()]).unwrap();
        assert!(registry.authorize(identity(0xAAAA_0001)).is_ok());
        assert_eq!(
            registry.authorize(identity(0xAAAA_0002)).unwrap_err(),
            PolicyError::UnknownTarget
        );
    }

    #[test]
    fn disabled_target_cannot_authorize() {
        let disabled = RegisteredTarget::disabled(
            "compatible-motor-4342-v1",
            0x4859_444C,
            0xAAAA_0002,
            "IAP identity and memory policy are not qualified",
        )
        .unwrap();
        let registry = TargetRegistry::new(vec![disabled]).unwrap();
        assert!(matches!(
            registry.authorize(identity(0xAAAA_0002)),
            Err(PolicyError::DisabledTarget { .. })
        ));
    }

    #[test]
    fn binding_checks_device_firmware_address_and_encryption() {
        let registry = TargetRegistry::new(vec![enabled()]).unwrap();
        let target = registry.authorize(identity(0xAAAA_0001)).unwrap();
        let bytes = make_image(0xAAAA_0001, 0x2025_1025, 0x1000_C000, 1024, true);
        let artifact = ImgArtifact::parse(&bytes, ImgLimits::default()).unwrap();
        assert!(PreparedUpgrade::bind(target, artifact).is_ok());
    }

    #[test]
    fn binding_does_not_guess_a_device_specific_bin_capacity() {
        let registry = TargetRegistry::new(vec![enabled()]).unwrap();
        let target = registry.authorize(identity(0xAAAA_0001)).unwrap();
        let bytes = make_image(0xAAAA_0001, 0x2025_1025, 0x1000_C000, 176_441, true);
        let artifact = ImgArtifact::parse(&bytes, ImgLimits::default()).unwrap();
        assert!(PreparedUpgrade::bind(target, artifact).is_ok());
    }

    #[test]
    fn changed_serial_cannot_revalidate() {
        let registry = TargetRegistry::new(vec![enabled()]).unwrap();
        let target = registry.authorize(identity(0xAAAA_0001)).unwrap();
        let bytes = make_image(0xAAAA_0001, 0x2025_1025, 0x1000_C000, 1024, true);
        let artifact = ImgArtifact::parse(&bytes, ImgLimits::default()).unwrap();
        let prepared = PreparedUpgrade::bind(target, artifact).unwrap();
        let changed = CanopenIdentity::new(1, 0x4859_444C, 0xAAAA_0001, 7, 124).unwrap();
        assert_eq!(
            prepared.revalidate(changed, &registry).unwrap_err(),
            PolicyError::IdentityChanged
        );
    }
}
