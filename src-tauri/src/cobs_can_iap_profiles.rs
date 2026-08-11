//! Thin adapter over the shared build-time firmware target catalog.

use cobs_can_iap::TargetRegistry;
use hexmeow_dfu_targets::CatalogBuildError;

pub(crate) fn target_registry() -> Result<TargetRegistry, CatalogBuildError> {
    hexmeow_dfu_targets::motor_img_target_registry()
}

pub(crate) fn display_name_for_profile(profile_id: &str) -> Option<&'static str> {
    hexmeow_dfu_targets::display_name_for_profile(profile_id)
}

#[cfg(test)]
mod tests {
    use cobs_can_iap::{CanopenIdentity, PolicyError, SupportPolicy};

    use super::*;

    fn identity(vendor_id: u32, product_code: u32) -> CanopenIdentity {
        CanopenIdentity::new(1, vendor_id, product_code, 1, 42).unwrap()
    }

    #[test]
    fn wrapper_enables_qualified_4310_and_4342_rows_only() {
        let registry = target_registry().unwrap();
        assert_eq!(registry.targets().len(), 5);
        assert_eq!(
            registry
                .targets()
                .iter()
                .filter(|target| matches!(target.support(), SupportPolicy::Enabled(_)))
                .count(),
            4
        );

        for (vendor_id, product_code) in [
            (0x4859_444C, 0xAAAA_0001),
            (0x4859_444C, 0xAAAA_0002),
            (0x0068_6578, 0x6C64_BC78),
            (0x0068_6578, 0x6C64_BCAA),
        ] {
            assert!(registry
                .authorize(identity(vendor_id, product_code))
                .is_ok());
        }
        assert!(matches!(
            registry.authorize(identity(0x4859_444C, 0xAAAA_0005)),
            Err(PolicyError::DisabledTarget { .. })
        ));
    }
}
