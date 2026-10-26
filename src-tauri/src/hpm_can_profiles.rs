//! Exact, qualified HPM CAN identity. Add product forks only after qualification.
pub const VENDOR: u32 = 0x6865786d;
pub const EVK_PRODUCT: u32 = 0x45564b33;
pub const EVK_PROFILE: &str = "hpm-evk3-v3";
pub fn known(vendor: u32, product: u32) -> bool {
    vendor == VENDOR && product == EVK_PRODUCT
}
#[cfg(test)]
mod tests {
    #[test]
    fn routes_are_exact_and_disjoint() {
        assert!(super::known(super::VENDOR, super::EVK_PRODUCT));
        assert!(!super::known(super::VENDOR, 0));
        assert!(!super::known(0, super::EVK_PRODUCT));
        assert!(
            hexmeow_dfu_targets::target_by_identity(super::VENDOR, super::EVK_PRODUCT).is_none()
        );
    }
}
