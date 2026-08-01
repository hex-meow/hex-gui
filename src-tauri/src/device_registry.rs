//! GUI device registry (CANopen `0x1018` identity → host device kind).
//!
//! Motor discovery + identity reading already lives in `hex-motor`
//! (`KNOWN_DEVICES`). This table is the GUI-owned companion: it lists the
//! **non-motor** hex-meow devices the GUI knows how to display. Classification
//! always uses the exact `(vendor_id, product_code)` tuple; a vendor-wide
//! wildcard must never make an unknown product safe to control as a motor.
//!
//! A device kind shares **one** frontend panel across all its product codes —
//! add a row here for every new IMU (or other non-motor) product code and they
//! all open the same IMU panel. See `docs/device-identity.md`.

use serde::Serialize;

/// hex-meow vendor id — ASCII "hexm" (`'h' 'e' 'x' 'm'`), i.e. `0x6865786D`.
pub const VENDOR_HEXM: u32 = 0x6865_786D;

pub const PRODUCT_IMU_G4: u32 = 0x0069_6D75;
pub const PRODUCT_ARM_IMU: u32 = 0x6169_6D75;
pub const PRODUCT_LIFT: u32 = 0x006C_6674;

/// Exact identities that are safe to route to the legacy CiA402 controls.
const CIA402_MOTOR_IDENTITIES: &[(u32, u32)] = &[
    (0x4859_444C, 0xAAAA_0001),
    (0x4859_444C, 0xAAAA_0002),
    (0x4859_444C, 0xAAAA_0005),
];

/// Public product-family identities for the new, non-CiA402 motor driver.
pub const MEOW_MOTOR_VENDOR_ID: u32 = 0x0068_6578;
pub const MEOW_MOTOR_4310_PRODUCT_CODE: u32 = 0x6C64_BC78;
pub const MEOW_MOTOR_4342_PRODUCT_CODE: u32 = 0x6C64_BCAA;

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

/// One exact non-motor device identity.
pub struct KnownDevice {
    pub vendor_id: u32,
    pub product_code: u32,
    pub kind: DeviceKind,
    pub name: &'static str,
}

/// The non-motor devices this GUI can display. Every IMU product code routes to
/// the single IMU panel — add new rows as new IMU variants ship.
pub const NON_MOTOR_DEVICES: &[KnownDevice] = &[
    KnownDevice {
        vendor_id: VENDOR_HEXM,
        product_code: PRODUCT_IMU_G4,
        kind: DeviceKind::Imu,
        name: "hex-meow IMU G4",
    },
    KnownDevice {
        vendor_id: VENDOR_HEXM,
        product_code: PRODUCT_ARM_IMU,
        kind: DeviceKind::Imu,
        name: "hex-meow arm IMU",
    },
    KnownDevice {
        vendor_id: VENDOR_HEXM,
        product_code: PRODUCT_LIFT,
        kind: DeviceKind::Lift,
        name: "hex-meow lift controller",
    },
];

/// Exact identities and public display names for the new motor type.
pub const MEOW_MOTOR_DEVICES: &[KnownDevice] = &[
    KnownDevice {
        vendor_id: MEOW_MOTOR_VENDOR_ID,
        product_code: MEOW_MOTOR_4310_PRODUCT_CODE,
        kind: DeviceKind::MeowMotor,
        name: "hexmeow 4310",
    },
    KnownDevice {
        vendor_id: MEOW_MOTOR_VENDOR_ID,
        product_code: MEOW_MOTOR_4342_PRODUCT_CODE,
        kind: DeviceKind::MeowMotor,
        name: "hexmeow 4342",
    },
];

/// Classify a node from its exact `0x1018` identity.
///
/// Only an exact legacy motor entry is safe to route to CiA402 controls; new
/// motor identities remain a distinct type even before their driver is wired.
pub fn classify(vendor_id: u32, product_code: u32) -> DeviceKind {
    if let Some(device) = NON_MOTOR_DEVICES
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.product_code == product_code)
    {
        return device.kind;
    }

    if let Some(device) = MEOW_MOTOR_DEVICES
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.product_code == product_code)
    {
        return device.kind;
    }

    if CIA402_MOTOR_IDENTITIES.contains(&(vendor_id, product_code)) {
        DeviceKind::Cia402Motor
    } else {
        DeviceKind::Unknown
    }
}

pub fn display_name(vendor_id: u32, product_code: u32) -> Option<&'static str> {
    NON_MOTOR_DEVICES
        .iter()
        .chain(MEOW_MOTOR_DEVICES)
        .find(|device| device.vendor_id == vendor_id && device.product_code == product_code)
        .map(|device| device.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_motor_products_require_exact_tuples() {
        assert_eq!(classify(VENDOR_HEXM, PRODUCT_IMU_G4), DeviceKind::Imu);
        assert_eq!(classify(VENDOR_HEXM, PRODUCT_ARM_IMU), DeviceKind::Imu);
        assert_eq!(classify(VENDOR_HEXM, PRODUCT_LIFT), DeviceKind::Lift);
        assert_eq!(classify(VENDOR_HEXM, 0xDEAD_BEEF), DeviceKind::Unknown);
    }

    #[test]
    fn vendor_wildcards_do_not_classify_unknown_products_as_motors() {
        assert_eq!(classify(0x0068_6578, 0xDEAD_BEEF), DeviceKind::Unknown);
        assert_eq!(classify(0x4859_444C, 0xDEAD_BEEF), DeviceKind::Unknown);
    }

    #[test]
    fn exact_motor_products_still_route_to_motor_controls() {
        assert_eq!(classify(0x4859_444C, 0xAAAA_0001), DeviceKind::Cia402Motor);
    }

    #[test]
    fn meow_motors_require_exact_tuples_and_have_fixed_public_names() {
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
        let cia402_motor = classify(0x4859_444C, 0xAAAA_0002);
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
