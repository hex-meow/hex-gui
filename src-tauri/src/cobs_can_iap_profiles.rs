//! Exact local authorization policy for the compatible COBS-over-CAN updater.
//!
//! Package metadata and a future remote catalog cannot add or widen rows here.
//! The original and custom 4310 identities have matching CANopen, Enter-IAP,
//! and encrypted IMG evidence. Every 4342/4360 identity remains visible but
//! locked until it has independent IAP and memory-policy evidence.

use cobs_can_iap::{IapPolicy, PolicyError, RegisteredTarget, TargetRegistry};

const COMPATIBLE_VENDOR_ID: u32 = 0x4859_444C;
const MOTOR_4310_PRODUCT: u32 = 0xAAAA_0001;
const MOTOR_4342_PRODUCT: u32 = 0xAAAA_0002;
const MOTOR_4360_PRODUCT: u32 = 0xAAAA_0005;

const CUSTOM_VENDOR_ID: u32 = 0x0068_6578;
const CUSTOM_MOTOR_4310_PRODUCT: u32 = 0x6C64_BC78;
const CUSTOM_MOTOR_4342_PRODUCT: u32 = 0x6C64_BCAA;

const MOTOR_4310_FIRMWARE_ID: u32 = 0x2025_1025;
const MOTOR_4310_APP_START: u32 = 0x1000_C000;
// This is the largest independently verified encrypted IMG body. Do not infer
// a wider flash region until the product memory map is authoritative.
const MOTOR_4310_MAX_BIN_SIZE: usize = 176_440;

pub(crate) fn target_registry() -> Result<TargetRegistry, PolicyError> {
    TargetRegistry::new(vec![
        RegisteredTarget::enabled(
            "compatible-motor-4310-v1",
            COMPATIBLE_VENDOR_ID,
            MOTOR_4310_PRODUCT,
            IapPolicy::new(
                MOTOR_4310_PRODUCT,
                vec![MOTOR_4310_FIRMWARE_ID],
                MOTOR_4310_APP_START,
                MOTOR_4310_MAX_BIN_SIZE,
                true,
            )?,
        )?,
        RegisteredTarget::disabled(
            "compatible-motor-4342-v1",
            COMPATIBLE_VENDOR_ID,
            MOTOR_4342_PRODUCT,
            "Known CANopen product, but its IAP identity and memory policy are not qualified",
        )?,
        RegisteredTarget::disabled(
            "compatible-motor-4360-v1",
            COMPATIBLE_VENDOR_ID,
            MOTOR_4360_PRODUCT,
            "Known CANopen product, but its IAP identity and memory policy are not qualified",
        )?,
        RegisteredTarget::enabled(
            "custom-motor-4310-v1",
            CUSTOM_VENDOR_ID,
            CUSTOM_MOTOR_4310_PRODUCT,
            IapPolicy::new(
                MOTOR_4310_PRODUCT,
                vec![MOTOR_4310_FIRMWARE_ID],
                MOTOR_4310_APP_START,
                MOTOR_4310_MAX_BIN_SIZE,
                true,
            )?,
        )?,
        RegisteredTarget::disabled(
            "custom-motor-4342-v1",
            CUSTOM_VENDOR_ID,
            CUSTOM_MOTOR_4342_PRODUCT,
            "Known custom motor, but its IAP identity and memory policy are not qualified",
        )?,
    ])
}

pub(crate) fn display_name_for_profile(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        "compatible-motor-4310-v1" => Some("CiA402 HEX-4310"),
        "compatible-motor-4342-v1" => Some("CiA402 HEX-4342P"),
        "compatible-motor-4360-v1" => Some("CiA402 HEX-4360P"),
        "custom-motor-4310-v1" => Some("hexmeow 4310"),
        "custom-motor-4342-v1" => Some("hexmeow 4342"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use cobs_can_iap::{CanopenIdentity, PolicyError, SupportPolicy};

    use super::*;

    fn identity(vendor_id: u32, product_code: u32) -> CanopenIdentity {
        CanopenIdentity::new(1, vendor_id, product_code, 1, 42).unwrap()
    }

    #[test]
    fn only_the_independently_evidenced_4310_routes_are_enabled() {
        let registry = target_registry().unwrap();
        assert_eq!(registry.targets().len(), 5);
        let enabled = registry
            .targets()
            .iter()
            .filter(|target| matches!(target.support(), SupportPolicy::Enabled(_)))
            .collect::<Vec<_>>();
        assert_eq!(enabled.len(), 2);
        assert!(enabled.iter().any(|target| {
            target.vendor_id() == COMPATIBLE_VENDOR_ID
                && target.product_code() == MOTOR_4310_PRODUCT
        }));
        assert!(enabled.iter().any(|target| {
            target.vendor_id() == CUSTOM_VENDOR_ID
                && target.product_code() == CUSTOM_MOTOR_4310_PRODUCT
        }));
        assert!(registry
            .authorize(identity(COMPATIBLE_VENDOR_ID, MOTOR_4310_PRODUCT))
            .is_ok());
        assert!(registry
            .authorize(identity(CUSTOM_VENDOR_ID, CUSTOM_MOTOR_4310_PRODUCT))
            .is_ok());
        assert!(matches!(
            registry.authorize(identity(COMPATIBLE_VENDOR_ID, MOTOR_4342_PRODUCT)),
            Err(PolicyError::DisabledTarget { .. })
        ));
        assert!(matches!(
            registry.authorize(identity(CUSTOM_VENDOR_ID, CUSTOM_MOTOR_4342_PRODUCT)),
            Err(PolicyError::DisabledTarget { .. })
        ));
        assert!(matches!(
            registry.authorize(identity(COMPATIBLE_VENDOR_ID, CUSTOM_MOTOR_4310_PRODUCT)),
            Err(PolicyError::UnknownTarget)
        ));
        for target in enabled {
            let SupportPolicy::Enabled(policy) = target.support() else {
                unreachable!("the enabled rows were filtered above");
            };
            assert_eq!(policy.max_bin_size(), 176_440);
        }
    }
}
