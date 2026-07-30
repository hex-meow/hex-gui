//! Exact, build-time STM32 CAN update authorization table.
//!
//! This is the single runtime location for product → hardware → MCU →
//! firmware-ID policy in the GUI. The reusable DFU crate deliberately contains
//! no product rows, and neither a package nor a future remote catalog may add
//! one.
//!
//! Evidence, package fingerprints, and the qualification boundary are recorded
//! in `hex-meow-fw/todo/dfu/stm32-target-registry.md`. Rows here are executable
//! authorization policy; that document is the review trail, not a second
//! runtime registry.

use hexmeow_stm32_can_dfu::{
    ArtifactPolicy, FirmwarePolicy, ProfileError, RegisteredTarget, TargetRegistry, UpgradePolicy,
    MCU_STM32G0B1,
};

pub(crate) const VENDOR_ID: u32 = 0x6865_786D;

const LIFT_PRODUCT_CODE: u32 = 0x006C_6674;
const LIFT_HARDWARE_1_1: u32 = 0x0001_0001;
const LIFT_APPLICATION_NAME: &str = "hexmeow-lift-driver";
const LIFT_PUBLIC_KEY: [u8; 64] = [
    0x52, 0x77, 0x77, 0xB3, 0x24, 0xC8, 0x34, 0x7D, 0xB0, 0xB1, 0x68, 0xAB, 0xB0, 0x41, 0x89, 0x90,
    0x52, 0x89, 0x76, 0x6D, 0xDE, 0x95, 0xAC, 0x4B, 0xE6, 0x8C, 0x60, 0xF3, 0xDC, 0x35, 0x0C, 0xC9,
    0x37, 0x09, 0x23, 0xF7, 0xE1, 0x4E, 0x95, 0x2A, 0x7D, 0xF1, 0x86, 0x33, 0x1F, 0x83, 0xF1, 0x2F,
    0x29, 0xA7, 0x01, 0x4F, 0xFA, 0x55, 0x89, 0xDE, 0xFF, 0x5F, 0xA5, 0xCA, 0x7E, 0xFA, 0x37, 0x9F,
];

const ARM_IMU_PRODUCT_CODE: u32 = 0x6169_6D75;
const ARM_IMU_HARDWARE_2_0: u32 = 0x0002_0000;
const ARM_IMU_APPLICATION_NAME: &str = "hexmeow-arm-imu";
const ARM_IMU_PUBLIC_KEY: [u8; 64] = [
    0xEE, 0xB5, 0x44, 0xEC, 0xB5, 0x62, 0x6A, 0x84, 0x56, 0xE3, 0x99, 0xBD, 0x28, 0x25, 0x8A, 0x66,
    0x4A, 0x17, 0xAF, 0x8B, 0x44, 0xFC, 0x8C, 0x7A, 0x8C, 0x7A, 0x5A, 0xEB, 0x2C, 0x32, 0x1B, 0x83,
    0x2E, 0xCF, 0x27, 0xC3, 0xE0, 0xA5, 0x2F, 0x9A, 0x6F, 0xA9, 0x53, 0x60, 0x98, 0x79, 0x59, 0xB0,
    0xD3, 0xA2, 0x69, 0x93, 0x55, 0x1A, 0x8B, 0xF6, 0xC1, 0x76, 0x3D, 0x48, 0x66, 0x0A, 0x95, 0x19,
];

const SIGNING_KEY_ID: u32 = 1;
const ENCRYPTION_KEY_ID: u32 = 1;
const SECURITY_EPOCH: u32 = 0;
const STANDARD_FIRMWARE_ID: u32 = 0;

pub(crate) fn target_registry() -> Result<TargetRegistry, ProfileError> {
    TargetRegistry::new(vec![
        RegisteredTarget::disabled(
            "imu-g4-bench",
            VENDOR_ID,
            0x0069_6D75,
            "Known bench product, but hardware_version → G431/G474 and firmware-ID mappings are not frozen",
        )?,
        secure_g0b1_target(
            "arm-imu-v1",
            ARM_IMU_PRODUCT_CODE,
            ARM_IMU_HARDWARE_2_0,
            ARM_IMU_APPLICATION_NAME,
            ARM_IMU_PUBLIC_KEY,
        )?,
        secure_g0b1_target(
            "lift-g0b1-v1",
            LIFT_PRODUCT_CODE,
            LIFT_HARDWARE_1_1,
            LIFT_APPLICATION_NAME,
            LIFT_PUBLIC_KEY,
        )?,
    ])
}

fn secure_g0b1_target(
    profile_id: &'static str,
    product_code: u32,
    hardware_version: u32,
    application_name: &'static str,
    public_key: [u8; 64],
) -> Result<RegisteredTarget, ProfileError> {
    let firmware = FirmwarePolicy::new(STANDARD_FIRMWARE_ID, vec![application_name.to_owned()])?;
    let policy = UpgradePolicy::new(
        MCU_STM32G0B1,
        vec![hardware_version],
        vec![firmware],
        ArtifactPolicy::encrypted_v2(
            public_key,
            SIGNING_KEY_ID,
            ENCRYPTION_KEY_ID,
            SECURITY_EPOCH,
        ),
    )?;
    RegisteredTarget::enabled(profile_id, VENDOR_ID, product_code, policy)
}

pub(crate) fn display_name_for_profile(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        "imu-g4-bench" => Some("IMU bench / demo"),
        "arm-imu-v1" => Some("Arm IMU"),
        "lift-g0b1-v1" => Some("Lift controller"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use hexmeow_stm32_can_dfu::SupportPolicy;
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn registry_enables_only_the_two_exact_test_profiles() {
        let registry = target_registry().unwrap();
        assert_eq!(registry.targets().len(), 3);

        let enabled = registry
            .targets()
            .iter()
            .filter_map(|target| match target.support() {
                SupportPolicy::Enabled(policy) => Some((target, policy)),
                SupportPolicy::Disabled { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(enabled.len(), 2);
        assert!(enabled.iter().all(|(target, policy)| {
            target.vendor_id() == VENDOR_ID
                && policy.mcu() == MCU_STM32G0B1
                && policy.hardware_versions().len() == 1
                && policy.firmware_ids() == [STANDARD_FIRMWARE_ID]
        }));

        let lift = enabled
            .iter()
            .find(|(target, _)| target.product_code() == LIFT_PRODUCT_CODE)
            .unwrap()
            .1;
        assert_eq!(lift.hardware_versions(), [LIFT_HARDWARE_1_1]);
        assert_eq!(
            lift.firmware_policy(STANDARD_FIRMWARE_ID)
                .unwrap()
                .application_names(),
            [LIFT_APPLICATION_NAME]
        );
        assert!(!lift.firmware_ids().contains(&1));

        let arm_imu = enabled
            .iter()
            .find(|(target, _)| target.product_code() == ARM_IMU_PRODUCT_CODE)
            .unwrap()
            .1;
        assert_eq!(arm_imu.hardware_versions(), [ARM_IMU_HARDWARE_2_0]);
        assert_eq!(
            arm_imu
                .firmware_policy(STANDARD_FIRMWARE_ID)
                .unwrap()
                .application_names(),
            [ARM_IMU_APPLICATION_NAME]
        );
    }

    #[test]
    fn embedded_public_keys_match_the_qualified_fingerprints() {
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
