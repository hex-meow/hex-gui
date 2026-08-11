//! Thin adapter over the shared build-time firmware target catalog.

use hexmeow_dfu_targets::CatalogBuildError;
use hexmeow_stm32_can_dfu::TargetRegistry;

pub(crate) fn target_registry() -> Result<TargetRegistry, CatalogBuildError> {
    hexmeow_dfu_targets::standard_target_registry()
}

pub(crate) fn display_name_for_profile(profile_id: &str) -> Option<&'static str> {
    hexmeow_dfu_targets::display_name_for_profile(profile_id)
}

#[cfg(test)]
mod tests {
    use hexmeow_stm32_can_dfu::SupportPolicy;

    use super::*;

    #[test]
    fn wrapper_exposes_only_catalog_standard_rows() {
        let registry = target_registry().unwrap();
        assert_eq!(registry.targets().len(), 3);
        assert_eq!(
            registry
                .targets()
                .iter()
                .filter(|target| matches!(target.support(), SupportPolicy::Enabled(_)))
                .count(),
            2
        );
    }
}
