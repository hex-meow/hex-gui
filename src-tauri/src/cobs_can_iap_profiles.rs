//! Exact local authorization policy for the compatible COBS-over-CAN updater.
//!
//! Package metadata and a future remote catalog cannot add or widen rows here.
//! Only the first product has matching evidence from a real CANopen identity,
//! Enter-IAP response, and encrypted IMG. The other known products remain
//! visible but locked until each has independent protocol and memory evidence.

use cobs_can_iap::{IapPolicy, PolicyError, RegisteredTarget, TargetRegistry};

const COMPATIBLE_VENDOR_ID: u32 = 0x4859_444C;
const MOTOR_4310_PRODUCT: u32 = 0xAAAA_0001;
const MOTOR_4342_PRODUCT: u32 = 0xAAAA_0002;
const MOTOR_4360_PRODUCT: u32 = 0xAAAA_0005;

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
    ])
}

pub(crate) fn display_name_for_profile(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        "compatible-motor-4310-v1" => Some("CiA402 HEX-4310"),
        "compatible-motor-4342-v1" => Some("CiA402 HEX-4342P"),
        "compatible-motor-4360-v1" => Some("CiA402 HEX-4360P"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use cobs_can_iap::{CanopenIdentity, PolicyError, SupportPolicy};

    use super::*;

    fn identity(product_code: u32) -> CanopenIdentity {
        CanopenIdentity::new(1, COMPATIBLE_VENDOR_ID, product_code, 1, 42).unwrap()
    }

    #[test]
    fn only_the_independently_evidenced_product_is_enabled() {
        let registry = target_registry().unwrap();
        assert_eq!(registry.targets().len(), 3);
        let enabled = registry
            .targets()
            .iter()
            .filter(|target| matches!(target.support(), SupportPolicy::Enabled(_)))
            .collect::<Vec<_>>();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].product_code(), MOTOR_4310_PRODUCT);
        assert!(registry.authorize(identity(MOTOR_4310_PRODUCT)).is_ok());
        assert!(matches!(
            registry.authorize(identity(MOTOR_4342_PRODUCT)),
            Err(PolicyError::DisabledTarget { .. })
        ));
        let SupportPolicy::Enabled(policy) = enabled[0].support() else {
            unreachable!("the enabled row was filtered above");
        };
        assert_eq!(policy.max_bin_size(), 176_440);
    }
}
