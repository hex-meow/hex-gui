//! One build-time source of truth for firmware-update identity routing.
//!
//! A complete CANopen `0x1018` vendor/product identity selects exactly one
//! backend here. Remote catalogs, filenames, and WebView input may narrow this
//! table, but cannot add a target, enable a disabled target, or change its
//! native artifact policy.

use std::collections::{HashMap, HashSet};

use cobs_can_iap::{
    IapPolicy, PolicyError as MotorImgPolicyError, RegisteredTarget as MotorImgTarget,
    TargetRegistry as MotorImgTargetRegistry,
};
use hexmeow_stm32_can_dfu::{
    ArtifactPolicy, FirmwarePolicy, ProfileError as StandardPolicyError,
    RegisteredTarget as StandardTarget, TargetRegistry as StandardTargetRegistry, UpgradePolicy,
    MCU_STM32G0B1,
};
use thiserror::Error;

/// The only artifact/backend family that an exact current identity may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetBackend {
    StandardMeowpkg,
    MotorImg,
}

/// Read-only work that must succeed before the first update-protocol mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreflightPolicy {
    None,
    BackupMeowFactory4001,
}

/// GUI capability family selected by the same exact identity as DFU routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    Imu,
    Lift,
    Cia402Motor,
    MeowMotor,
}

/// Vendor/product portion of a complete CANopen `0x1018` identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdentityKey {
    pub vendor_id: u32,
    pub product_code: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardFirmwarePolicy {
    pub firmware_id: u32,
    pub application_names: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardArtifactPolicy {
    UnprotectedV1,
    EncryptedV2 {
        public_key: [u8; 64],
        signing_key_id: u32,
        encryption_key_id: u32,
        security_epoch: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardMeowpkgPolicy {
    pub mcu: &'static str,
    pub hardware_versions: &'static [u32],
    pub firmware: &'static [StandardFirmwarePolicy],
    pub artifact: StandardArtifactPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotorImgPolicy {
    pub device_id: u32,
    pub firmware_ids: &'static [u32],
    pub start_address: u32,
    pub require_encrypted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicy {
    StandardMeowpkg(StandardMeowpkgPolicy),
    MotorImg(MotorImgPolicy),
}

impl NativePolicy {
    pub const fn backend(self) -> TargetBackend {
        match self {
            Self::StandardMeowpkg(_) => TargetBackend::StandardMeowpkg,
            Self::MotorImg(_) => TargetBackend::MotorImg,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSupport {
    Enabled(NativePolicy),
    Disabled {
        backend: TargetBackend,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetDescriptor {
    /// Stable host-side update-contract identifier. A `-v1` suffix describes
    /// this profile contract, not a firmware or CANopen revision. An
    /// incompatible future trust/policy contract must migrate to a new
    /// profile generation instead of changing v1 in place.
    pub profile_id: &'static str,
    pub display_name: &'static str,
    /// Optional general device-browser name. This remains distinct from the
    /// concise firmware-profile `display_name` used by the updater.
    pub device_name: Option<&'static str>,
    pub device_class: DeviceClass,
    pub vendor_id: u32,
    pub product_code: u32,
    pub expected_postflash_identity: IdentityKey,
    pub preflight: PreflightPolicy,
    pub support: TargetSupport,
}

impl TargetDescriptor {
    pub const fn identity(self) -> IdentityKey {
        IdentityKey {
            vendor_id: self.vendor_id,
            product_code: self.product_code,
        }
    }

    pub const fn backend(self) -> TargetBackend {
        match self.support {
            TargetSupport::Enabled(policy) => policy.backend(),
            TargetSupport::Disabled { backend, .. } => backend,
        }
    }

    pub const fn is_enabled(self) -> bool {
        matches!(self.support, TargetSupport::Enabled(_))
    }

    pub const fn standard_meowpkg_policy(self) -> Option<StandardMeowpkgPolicy> {
        match self.support {
            TargetSupport::Enabled(NativePolicy::StandardMeowpkg(policy)) => Some(policy),
            _ => None,
        }
    }

    pub const fn motor_img_policy(self) -> Option<MotorImgPolicy> {
        match self.support {
            TargetSupport::Enabled(NativePolicy::MotorImg(policy)) => Some(policy),
            _ => None,
        }
    }
}

/// HexMeow vendor ID, ASCII `hexm`.
pub const VENDOR_HEXM: u32 = 0x6865_786D;
pub const PRODUCT_IMU_G4: u32 = 0x0069_6D75;
pub const PRODUCT_ARM_IMU: u32 = 0x6169_6D75;
pub const PRODUCT_LIFT: u32 = 0x006C_6674;

pub const CIA402_MOTOR_VENDOR_ID: u32 = 0x4859_444C;
pub const CIA402_MOTOR_4310_PRODUCT_CODE: u32 = 0xAAAA_0001;
pub const CIA402_MOTOR_4342_PRODUCT_CODE: u32 = 0xAAAA_0002;
pub const CIA402_MOTOR_4360_PRODUCT_CODE: u32 = 0xAAAA_0005;

pub const MEOW_MOTOR_VENDOR_ID: u32 = 0x0068_6578;
pub const MEOW_MOTOR_4310_PRODUCT_CODE: u32 = 0x6C64_BC78;
pub const MEOW_MOTOR_4342_PRODUCT_CODE: u32 = 0x6C64_BCAA;

const STANDARD_FIRMWARE_ID: u32 = 0;
const SIGNING_KEY_ID: u32 = 1;
const ENCRYPTION_KEY_ID: u32 = 1;
const SECURITY_EPOCH: u32 = 0;

const LIFT_PUBLIC_KEY: [u8; 64] = [
    0x52, 0x77, 0x77, 0xB3, 0x24, 0xC8, 0x34, 0x7D, 0xB0, 0xB1, 0x68, 0xAB, 0xB0, 0x41, 0x89, 0x90,
    0x52, 0x89, 0x76, 0x6D, 0xDE, 0x95, 0xAC, 0x4B, 0xE6, 0x8C, 0x60, 0xF3, 0xDC, 0x35, 0x0C, 0xC9,
    0x37, 0x09, 0x23, 0xF7, 0xE1, 0x4E, 0x95, 0x2A, 0x7D, 0xF1, 0x86, 0x33, 0x1F, 0x83, 0xF1, 0x2F,
    0x29, 0xA7, 0x01, 0x4F, 0xFA, 0x55, 0x89, 0xDE, 0xFF, 0x5F, 0xA5, 0xCA, 0x7E, 0xFA, 0x37, 0x9F,
];

const ARM_IMU_PUBLIC_KEY: [u8; 64] = [
    0xEE, 0xB5, 0x44, 0xEC, 0xB5, 0x62, 0x6A, 0x84, 0x56, 0xE3, 0x99, 0xBD, 0x28, 0x25, 0x8A, 0x66,
    0x4A, 0x17, 0xAF, 0x8B, 0x44, 0xFC, 0x8C, 0x7A, 0x8C, 0x7A, 0x5A, 0xEB, 0x2C, 0x32, 0x1B, 0x83,
    0x2E, 0xCF, 0x27, 0xC3, 0xE0, 0xA5, 0x2F, 0x9A, 0x6F, 0xA9, 0x53, 0x60, 0x98, 0x79, 0x59, 0xB0,
    0xD3, 0xA2, 0x69, 0x93, 0x55, 0x1A, 0x8B, 0xF6, 0xC1, 0x76, 0x3D, 0x48, 0x66, 0x0A, 0x95, 0x19,
];

const ARM_IMU_HARDWARE: &[u32] = &[0x0002_0000];
const LIFT_HARDWARE: &[u32] = &[0x0001_0001];
const ARM_IMU_FIRMWARE: &[StandardFirmwarePolicy] = &[StandardFirmwarePolicy {
    firmware_id: STANDARD_FIRMWARE_ID,
    application_names: &["hexmeow-arm-imu"],
}];
const LIFT_FIRMWARE: &[StandardFirmwarePolicy] = &[StandardFirmwarePolicy {
    firmware_id: STANDARD_FIRMWARE_ID,
    application_names: &["hexmeow-lift-driver"],
}];

const MOTOR_4310_FIRMWARE_IDS: &[u32] = &[0x2025_1025];
const MOTOR_4342_FIRMWARE_IDS: &[u32] = &[0x2025_1209];

const fn current_identity(vendor_id: u32, product_code: u32) -> IdentityKey {
    IdentityKey {
        vendor_id,
        product_code,
    }
}

const fn standard_policy(
    hardware_versions: &'static [u32],
    firmware: &'static [StandardFirmwarePolicy],
    public_key: [u8; 64],
) -> NativePolicy {
    NativePolicy::StandardMeowpkg(StandardMeowpkgPolicy {
        mcu: MCU_STM32G0B1,
        hardware_versions,
        firmware,
        artifact: StandardArtifactPolicy::EncryptedV2 {
            public_key,
            signing_key_id: SIGNING_KEY_ID,
            encryption_key_id: ENCRYPTION_KEY_ID,
            security_epoch: SECURITY_EPOCH,
        },
    })
}

const fn motor_img_policy(device_id: u32, firmware_ids: &'static [u32]) -> NativePolicy {
    NativePolicy::MotorImg(MotorImgPolicy {
        device_id,
        firmware_ids,
        start_address: 0x1000_C000,
        require_encrypted: true,
    })
}

/// Complete build-time firmware target catalog.
///
/// New identities and enablement changes require a source change and tests;
/// neither R2 nor the GUI can extend this list at runtime.
pub const TARGETS: &[TargetDescriptor] = &[
    TargetDescriptor {
        profile_id: "imu-g4-bench",
        display_name: "IMU bench / demo",
        device_name: Some("hex-meow IMU G4"),
        device_class: DeviceClass::Imu,
        vendor_id: VENDOR_HEXM,
        product_code: PRODUCT_IMU_G4,
        expected_postflash_identity: current_identity(VENDOR_HEXM, PRODUCT_IMU_G4),
        preflight: PreflightPolicy::None,
        support: TargetSupport::Disabled {
            backend: TargetBackend::StandardMeowpkg,
            reason: "Known bench product, but hardware_version -> G431/G474 and firmware-ID mappings are not frozen",
        },
    },
    TargetDescriptor {
        profile_id: "arm-imu-v1",
        display_name: "Arm IMU",
        device_name: Some("hex-meow arm IMU"),
        device_class: DeviceClass::Imu,
        vendor_id: VENDOR_HEXM,
        product_code: PRODUCT_ARM_IMU,
        expected_postflash_identity: current_identity(VENDOR_HEXM, PRODUCT_ARM_IMU),
        preflight: PreflightPolicy::None,
        support: TargetSupport::Enabled(standard_policy(
            ARM_IMU_HARDWARE,
            ARM_IMU_FIRMWARE,
            ARM_IMU_PUBLIC_KEY,
        )),
    },
    TargetDescriptor {
        profile_id: "lift-g0b1-v1",
        display_name: "Lift controller",
        device_name: Some("hex-meow lift controller"),
        device_class: DeviceClass::Lift,
        vendor_id: VENDOR_HEXM,
        product_code: PRODUCT_LIFT,
        expected_postflash_identity: current_identity(VENDOR_HEXM, PRODUCT_LIFT),
        preflight: PreflightPolicy::None,
        support: TargetSupport::Enabled(standard_policy(
            LIFT_HARDWARE,
            LIFT_FIRMWARE,
            LIFT_PUBLIC_KEY,
        )),
    },
    TargetDescriptor {
        profile_id: "compatible-motor-4310-v1",
        display_name: "CiA402 HEX-4310",
        device_name: None,
        device_class: DeviceClass::Cia402Motor,
        vendor_id: CIA402_MOTOR_VENDOR_ID,
        product_code: CIA402_MOTOR_4310_PRODUCT_CODE,
        expected_postflash_identity: current_identity(
            CIA402_MOTOR_VENDOR_ID,
            CIA402_MOTOR_4310_PRODUCT_CODE,
        ),
        preflight: PreflightPolicy::None,
        support: TargetSupport::Enabled(motor_img_policy(
            CIA402_MOTOR_4310_PRODUCT_CODE,
            MOTOR_4310_FIRMWARE_IDS,
        )),
    },
    TargetDescriptor {
        profile_id: "compatible-motor-4342-v1",
        display_name: "CiA402 HEX-4342P",
        device_name: None,
        device_class: DeviceClass::Cia402Motor,
        vendor_id: CIA402_MOTOR_VENDOR_ID,
        product_code: CIA402_MOTOR_4342_PRODUCT_CODE,
        expected_postflash_identity: current_identity(
            CIA402_MOTOR_VENDOR_ID,
            CIA402_MOTOR_4342_PRODUCT_CODE,
        ),
        preflight: PreflightPolicy::None,
        support: TargetSupport::Enabled(motor_img_policy(
            CIA402_MOTOR_4342_PRODUCT_CODE,
            MOTOR_4342_FIRMWARE_IDS,
        )),
    },
    TargetDescriptor {
        profile_id: "compatible-motor-4360-v1",
        display_name: "CiA402 HEX-4360P",
        device_name: None,
        device_class: DeviceClass::Cia402Motor,
        vendor_id: CIA402_MOTOR_VENDOR_ID,
        product_code: CIA402_MOTOR_4360_PRODUCT_CODE,
        expected_postflash_identity: current_identity(
            CIA402_MOTOR_VENDOR_ID,
            CIA402_MOTOR_4360_PRODUCT_CODE,
        ),
        preflight: PreflightPolicy::None,
        support: TargetSupport::Disabled {
            backend: TargetBackend::MotorImg,
            reason: "Known CANopen product, but its IAP identity is not qualified",
        },
    },
    TargetDescriptor {
        profile_id: "custom-motor-4310-v1",
        display_name: "hexmeow 4310",
        device_name: Some("hexmeow 4310"),
        device_class: DeviceClass::MeowMotor,
        vendor_id: MEOW_MOTOR_VENDOR_ID,
        product_code: MEOW_MOTOR_4310_PRODUCT_CODE,
        expected_postflash_identity: current_identity(
            MEOW_MOTOR_VENDOR_ID,
            MEOW_MOTOR_4310_PRODUCT_CODE,
        ),
        preflight: PreflightPolicy::BackupMeowFactory4001,
        support: TargetSupport::Enabled(motor_img_policy(
            CIA402_MOTOR_4310_PRODUCT_CODE,
            MOTOR_4310_FIRMWARE_IDS,
        )),
    },
    TargetDescriptor {
        profile_id: "custom-motor-4342-v1",
        display_name: "hexmeow 4342",
        device_name: Some("hexmeow 4342"),
        device_class: DeviceClass::MeowMotor,
        vendor_id: MEOW_MOTOR_VENDOR_ID,
        product_code: MEOW_MOTOR_4342_PRODUCT_CODE,
        expected_postflash_identity: current_identity(
            MEOW_MOTOR_VENDOR_ID,
            MEOW_MOTOR_4342_PRODUCT_CODE,
        ),
        preflight: PreflightPolicy::BackupMeowFactory4001,
        support: TargetSupport::Enabled(motor_img_policy(
            CIA402_MOTOR_4342_PRODUCT_CODE,
            MOTOR_4342_FIRMWARE_IDS,
        )),
    },
];

pub fn target_by_profile_id(profile_id: &str) -> Option<&'static TargetDescriptor> {
    TARGETS
        .iter()
        .find(|target| target.profile_id == profile_id)
}

pub fn target_by_identity(vendor_id: u32, product_code: u32) -> Option<&'static TargetDescriptor> {
    TARGETS
        .iter()
        .find(|target| target.vendor_id == vendor_id && target.product_code == product_code)
}

pub fn display_name_for_profile(profile_id: &str) -> Option<&'static str> {
    target_by_profile_id(profile_id).map(|target| target.display_name)
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CatalogError {
    #[error("target profile ID must not be empty")]
    EmptyProfileId,
    #[error("target display name must not be empty for profile {0}")]
    EmptyDisplayName(&'static str),
    #[error("target device name must not be empty for profile {0}")]
    EmptyDeviceName(&'static str),
    #[error("target {profile_id} has invalid {field} value {value:#010x}")]
    InvalidIdentity {
        profile_id: &'static str,
        field: &'static str,
        value: u32,
    },
    #[error("duplicate target profile ID {0}")]
    DuplicateProfileId(&'static str),
    #[error("profiles {first} and {second} share CANopen vendor/product identity")]
    DuplicateIdentity {
        first: &'static str,
        second: &'static str,
    },
    #[error("profile {0} requests a Meow 0x4001 backup without an enabled motor IMG backend")]
    InvalidPreflight(&'static str),
    #[error("profile {0} has a device class that is inconsistent with its firmware backend")]
    BackendDeviceClassMismatch(&'static str),
}

/// Validate global invariants, including uniqueness across both backends.
pub fn validate_catalog() -> Result<(), CatalogError> {
    let mut profile_ids = HashSet::new();
    let mut identities = HashMap::new();
    for target in TARGETS {
        if target.profile_id.trim().is_empty() {
            return Err(CatalogError::EmptyProfileId);
        }
        if target.display_name.trim().is_empty() {
            return Err(CatalogError::EmptyDisplayName(target.profile_id));
        }
        if target
            .device_name
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(CatalogError::EmptyDeviceName(target.profile_id));
        }
        for (field, value) in [
            ("vendor_id", target.vendor_id),
            ("product_code", target.product_code),
            (
                "expected_postflash_vendor_id",
                target.expected_postflash_identity.vendor_id,
            ),
            (
                "expected_postflash_product_code",
                target.expected_postflash_identity.product_code,
            ),
        ] {
            if value == 0 || value == u32::MAX {
                return Err(CatalogError::InvalidIdentity {
                    profile_id: target.profile_id,
                    field,
                    value,
                });
            }
        }
        if !profile_ids.insert(target.profile_id) {
            return Err(CatalogError::DuplicateProfileId(target.profile_id));
        }
        if let Some(first) = identities.insert(target.identity(), target.profile_id) {
            return Err(CatalogError::DuplicateIdentity {
                first,
                second: target.profile_id,
            });
        }
        if target.preflight == PreflightPolicy::BackupMeowFactory4001
            && (target.backend() != TargetBackend::MotorImg
                || target.device_class != DeviceClass::MeowMotor
                || !target.is_enabled())
        {
            return Err(CatalogError::InvalidPreflight(target.profile_id));
        }
        if !matches!(
            (target.backend(), target.device_class),
            (
                TargetBackend::StandardMeowpkg,
                DeviceClass::Imu | DeviceClass::Lift
            ) | (
                TargetBackend::MotorImg,
                DeviceClass::Cia402Motor | DeviceClass::MeowMotor
            )
        ) {
            return Err(CatalogError::BackendDeviceClassMismatch(target.profile_id));
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum CatalogBuildError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("invalid standard meowpkg target policy: {0}")]
    Standard(#[from] StandardPolicyError),
    #[error("invalid motor IMG target policy: {0}")]
    MotorImg(#[from] MotorImgPolicyError),
}

pub fn standard_target_registry() -> Result<StandardTargetRegistry, CatalogBuildError> {
    validate_catalog()?;
    let mut targets = Vec::new();
    for target in TARGETS
        .iter()
        .filter(|target| target.backend() == TargetBackend::StandardMeowpkg)
    {
        let registered = match target.support {
            TargetSupport::Enabled(NativePolicy::StandardMeowpkg(policy)) => {
                StandardTarget::enabled(
                    target.profile_id,
                    target.vendor_id,
                    target.product_code,
                    build_standard_policy(policy)?,
                )?
            }
            TargetSupport::Disabled { reason, .. } => StandardTarget::disabled(
                target.profile_id,
                target.vendor_id,
                target.product_code,
                reason,
            )?,
            TargetSupport::Enabled(NativePolicy::MotorImg(_)) => unreachable!("backend agrees"),
        };
        targets.push(registered);
    }
    Ok(StandardTargetRegistry::new(targets)?)
}

pub fn motor_img_target_registry() -> Result<MotorImgTargetRegistry, CatalogBuildError> {
    validate_catalog()?;
    let mut targets = Vec::new();
    for target in TARGETS
        .iter()
        .filter(|target| target.backend() == TargetBackend::MotorImg)
    {
        let registered = match target.support {
            TargetSupport::Enabled(NativePolicy::MotorImg(policy)) => MotorImgTarget::enabled(
                target.profile_id,
                target.vendor_id,
                target.product_code,
                IapPolicy::new(
                    policy.device_id,
                    policy.firmware_ids.to_vec(),
                    policy.start_address,
                    policy.require_encrypted,
                )?,
            )?,
            TargetSupport::Disabled { reason, .. } => MotorImgTarget::disabled(
                target.profile_id,
                target.vendor_id,
                target.product_code,
                reason,
            )?,
            TargetSupport::Enabled(NativePolicy::StandardMeowpkg(_)) => {
                unreachable!("backend agrees")
            }
        };
        targets.push(registered);
    }
    Ok(MotorImgTargetRegistry::new(targets)?)
}

fn build_standard_policy(
    policy: StandardMeowpkgPolicy,
) -> Result<UpgradePolicy, StandardPolicyError> {
    let firmware = policy
        .firmware
        .iter()
        .map(|policy| {
            FirmwarePolicy::new(
                policy.firmware_id,
                policy
                    .application_names
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let artifact = match policy.artifact {
        StandardArtifactPolicy::UnprotectedV1 => ArtifactPolicy::UnprotectedV1,
        StandardArtifactPolicy::EncryptedV2 {
            public_key,
            signing_key_id,
            encryption_key_id,
            security_epoch,
        } => ArtifactPolicy::encrypted_v2(
            public_key,
            signing_key_id,
            encryption_key_id,
            security_epoch,
        ),
    };
    UpgradePolicy::new(
        policy.mcu,
        policy.hardware_versions.to_vec(),
        firmware,
        artifact,
    )
}

#[cfg(test)]
mod tests {
    use cobs_can_iap::SupportPolicy as MotorSupport;
    use hexmeow_stm32_can_dfu::SupportPolicy as StandardSupport;
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn catalog_is_unique_and_freezes_backend_routing() {
        validate_catalog().unwrap();
        assert_eq!(TARGETS.len(), 8);
        assert_eq!(
            TARGETS.iter().filter(|target| target.is_enabled()).count(),
            6
        );
        assert_eq!(
            TARGETS
                .iter()
                .filter(|target| target.backend() == TargetBackend::StandardMeowpkg)
                .count(),
            3
        );
        assert_eq!(
            TARGETS
                .iter()
                .filter(|target| target.backend() == TargetBackend::MotorImg)
                .count(),
            5
        );
    }

    #[test]
    fn exact_profile_and_identity_queries_share_the_same_rows() {
        for target in TARGETS {
            assert_eq!(target_by_profile_id(target.profile_id), Some(target));
            assert_eq!(
                target_by_identity(target.vendor_id, target.product_code),
                Some(target)
            );
            assert_eq!(
                display_name_for_profile(target.profile_id),
                Some(target.display_name)
            );
        }
        assert!(target_by_profile_id("unknown").is_none());
        assert!(target_by_identity(1, 2).is_none());
    }

    #[test]
    fn motor_4310_and_4342_are_enabled_but_4360_stays_disabled() {
        for profile_id in [
            "compatible-motor-4310-v1",
            "compatible-motor-4342-v1",
            "custom-motor-4310-v1",
            "custom-motor-4342-v1",
        ] {
            let target = target_by_profile_id(profile_id).unwrap();
            assert!(target.is_enabled());
            assert_eq!(target.backend(), TargetBackend::MotorImg);
            let policy = target.motor_img_policy().unwrap();
            assert!(policy.require_encrypted);
            assert_eq!(policy.start_address, 0x1000_C000);
        }
        assert!(!target_by_profile_id("compatible-motor-4360-v1")
            .unwrap()
            .is_enabled());
    }

    #[test]
    fn only_meow_motor_rows_require_factory_backup() {
        let requiring_backup = TARGETS
            .iter()
            .filter(|target| target.preflight == PreflightPolicy::BackupMeowFactory4001)
            .map(|target| target.profile_id)
            .collect::<Vec<_>>();
        assert_eq!(
            requiring_backup,
            ["custom-motor-4310-v1", "custom-motor-4342-v1"]
        );
    }

    #[test]
    fn normal_updates_preserve_the_canopen_firmware_family() {
        for target in TARGETS {
            assert_eq!(target.expected_postflash_identity, target.identity());
        }
    }

    #[test]
    fn native_registries_are_built_from_the_catalog() {
        let standard = standard_target_registry().unwrap();
        let motor = motor_img_target_registry().unwrap();
        assert_eq!(standard.targets().len(), 3);
        assert_eq!(motor.targets().len(), 5);
        assert_eq!(
            standard
                .targets()
                .iter()
                .filter(|target| matches!(target.support(), StandardSupport::Enabled(_)))
                .count(),
            2
        );
        assert_eq!(
            motor
                .targets()
                .iter()
                .filter(|target| matches!(target.support(), MotorSupport::Enabled(_)))
                .count(),
            4
        );
    }

    #[test]
    fn embedded_standard_public_keys_keep_qualified_fingerprints() {
        assert_eq!(
            hex_sha256(&LIFT_PUBLIC_KEY),
            "1d1995248476c1a10befe31813f36a2b9109e1ba70b9f7194d376beda069d053"
        );
        assert_eq!(
            hex_sha256(&ARM_IMU_PUBLIC_KEY),
            "75b7a0dd228a131840f64347444c8406bc84de1f2fd71b3b8afec31903544a70"
        );
    }

    fn hex_sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}
