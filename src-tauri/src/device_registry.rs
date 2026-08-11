//! GUI device classification backed by the shared firmware target catalog.
//!
//! Exact CANopen `0x1018` vendor/product routing is owned by
//! `hexmeow-dfu-targets`; this module retains the established GUI-facing
//! `DeviceKind` capability API without maintaining a second identity table.

use hexmeow_dfu_targets::{target_by_identity, DeviceClass};
use serde::Serialize;

// Compatibility exports for existing GUI modules and downstream tests. The
// values themselves are owned by the central catalog above.
#[allow(unused_imports)]
pub use hexmeow_dfu_targets::{
    CIA402_MOTOR_4310_PRODUCT_CODE, CIA402_MOTOR_4342_PRODUCT_CODE, CIA402_MOTOR_4360_PRODUCT_CODE,
    CIA402_MOTOR_VENDOR_ID, MEOW_MOTOR_4310_PRODUCT_CODE, MEOW_MOTOR_4342_PRODUCT_CODE,
    MEOW_MOTOR_VENDOR_ID, PRODUCT_ARM_IMU, PRODUCT_IMU_G4, PRODUCT_LIFT, VENDOR_HEXM,
};

/// Which panel a discovered device opens. Unknown exact tuples remain
/// `Unknown`; neither motor type is an implicit vendor-level default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Unknown,
    Cia402Motor,
    MeowMotor,
    Imu,
    Lift,
}

impl DeviceKind {
    /// Snake-case tag the frontend matches on (`"cia402_motor"`, `"meow_motor"`).
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::Unknown => "unknown",
            DeviceKind::Cia402Motor => "cia402_motor",
            DeviceKind::MeowMotor => "meow_motor",
            DeviceKind::Imu => "imu",
            DeviceKind::Lift => "lift",
        }
    }

    /// Only device types backed by the legacy settings implementation may enter
    /// the generic settings command. The new motor type has a separate command
    /// whose manager owns its 0x2001/0x1010:04 transaction.
    pub fn supports_device_settings(self) -> bool {
        matches!(
            self,
            DeviceKind::Cia402Motor | DeviceKind::Imu | DeviceKind::Lift
        )
    }

    /// The existing position-preset path belongs only to exact legacy tuples.
    pub fn supports_position_preset(self) -> bool {
        matches!(self, DeviceKind::Cia402Motor)
    }

    /// Only the legacy motor type may enter existing CiA402 command paths.
    pub fn supports_cia402_controls(self) -> bool {
        matches!(self, DeviceKind::Cia402Motor)
    }
}

impl From<DeviceClass> for DeviceKind {
    fn from(class: DeviceClass) -> Self {
        match class {
            DeviceClass::Imu => Self::Imu,
            DeviceClass::Lift => Self::Lift,
            DeviceClass::Cia402Motor => Self::Cia402Motor,
            DeviceClass::MeowMotor => Self::MeowMotor,
        }
    }
}

/// Classify a node from its exact `0x1018` identity.
pub fn classify(vendor_id: u32, product_code: u32) -> DeviceKind {
    target_by_identity(vendor_id, product_code)
        .map(|target| target.device_class.into())
        .unwrap_or(DeviceKind::Unknown)
}

/// Return the established general device-browser name, when one exists.
///
/// Partner CiA402 motors intentionally keep returning `None`, matching the
/// previous registry behavior; firmware-profile labels remain available via
/// `TargetDescriptor::display_name`.
pub fn display_name(vendor_id: u32, product_code: u32) -> Option<&'static str> {
    target_by_identity(vendor_id, product_code).and_then(|target| target.device_name)
}

#[cfg(test)]
mod tests {
    use hexmeow_dfu_targets::TARGETS;

    use super::*;

    #[test]
    fn every_catalog_identity_uses_its_central_device_class() {
        for target in TARGETS {
            assert_eq!(
                classify(target.vendor_id, target.product_code),
                target.device_class.into()
            );
            assert_eq!(
                display_name(target.vendor_id, target.product_code),
                target.device_name
            );
        }
    }

    #[test]
    fn non_motor_products_require_exact_tuples() {
        assert_eq!(classify(VENDOR_HEXM, PRODUCT_IMU_G4), DeviceKind::Imu);
        assert_eq!(classify(VENDOR_HEXM, PRODUCT_ARM_IMU), DeviceKind::Imu);
        assert_eq!(classify(VENDOR_HEXM, PRODUCT_LIFT), DeviceKind::Lift);
        assert_eq!(classify(VENDOR_HEXM, 0xDEAD_BEEF), DeviceKind::Unknown);
    }

    #[test]
    fn vendor_wildcards_do_not_classify_unknown_products_as_motors() {
        assert_eq!(
            classify(MEOW_MOTOR_VENDOR_ID, 0xDEAD_BEEF),
            DeviceKind::Unknown
        );
        assert_eq!(
            classify(CIA402_MOTOR_VENDOR_ID, 0xDEAD_BEEF),
            DeviceKind::Unknown
        );
    }

    #[test]
    fn exact_motor_products_still_route_to_motor_controls() {
        for product_code in [
            CIA402_MOTOR_4310_PRODUCT_CODE,
            CIA402_MOTOR_4342_PRODUCT_CODE,
            CIA402_MOTOR_4360_PRODUCT_CODE,
        ] {
            assert_eq!(
                classify(CIA402_MOTOR_VENDOR_ID, product_code),
                DeviceKind::Cia402Motor
            );
        }
    }

    #[test]
    fn meow_motors_require_exact_tuples_and_keep_public_names() {
        assert_eq!(
            classify(MEOW_MOTOR_VENDOR_ID, MEOW_MOTOR_4310_PRODUCT_CODE),
            DeviceKind::MeowMotor
        );
        assert_eq!(
            classify(MEOW_MOTOR_VENDOR_ID, MEOW_MOTOR_4342_PRODUCT_CODE),
            DeviceKind::MeowMotor
        );
        assert_eq!(
            display_name(MEOW_MOTOR_VENDOR_ID, MEOW_MOTOR_4310_PRODUCT_CODE),
            Some("hexmeow 4310")
        );
        assert_eq!(
            display_name(MEOW_MOTOR_VENDOR_ID, MEOW_MOTOR_4342_PRODUCT_CODE),
            Some("hexmeow 4342")
        );

        assert_eq!(
            classify(MEOW_MOTOR_VENDOR_ID, 0xDEAD_BEEF),
            DeviceKind::Unknown
        );
        assert_eq!(
            classify(0xDEAD_BEEF, MEOW_MOTOR_4310_PRODUCT_CODE),
            DeviceKind::Unknown
        );
    }

    #[test]
    fn operation_capabilities_follow_exact_classification() {
        let cia402_motor = classify(CIA402_MOTOR_VENDOR_ID, CIA402_MOTOR_4342_PRODUCT_CODE);
        assert!(cia402_motor.supports_device_settings());
        assert!(cia402_motor.supports_position_preset());
        assert!(cia402_motor.supports_cia402_controls());

        let meow_motor = classify(MEOW_MOTOR_VENDOR_ID, MEOW_MOTOR_4310_PRODUCT_CODE);
        assert!(!meow_motor.supports_device_settings());
        assert!(!meow_motor.supports_position_preset());
        assert!(!meow_motor.supports_cia402_controls());
        assert_eq!(meow_motor.as_str(), "meow_motor");

        for known_non_motor in [
            classify(VENDOR_HEXM, PRODUCT_IMU_G4),
            classify(VENDOR_HEXM, PRODUCT_ARM_IMU),
            classify(VENDOR_HEXM, PRODUCT_LIFT),
        ] {
            assert!(known_non_motor.supports_device_settings());
            assert!(!known_non_motor.supports_position_preset());
        }

        let unknown = classify(VENDOR_HEXM, 0xDEAD_BEEF);
        assert!(!unknown.supports_device_settings());
        assert!(!unknown.supports_position_preset());
    }
}
