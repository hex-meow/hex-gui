//! Non-motor device registry (CANopen `0x1018` identity → host device kind).
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

/// Exact identities that are safe to route to CiA402 controls.
const MOTOR_IDENTITIES: &[(u32, u32)] = &[
    (0x4859_444C, 0xAAAA_0001),
    (0x4859_444C, 0xAAAA_0002),
    (0x4859_444C, 0xAAAA_0005),
];

/// Which panel a discovered device opens. Unknown exact tuples remain
/// `Unknown`; `Motor` is never an implicit vendor-level default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Unknown,
    Motor,
    Imu,
    Lift,
}

impl DeviceKind {
    /// Lowercase tag the frontend matches on (`"motor"`, `"imu"`).
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::Unknown => "unknown",
            DeviceKind::Motor => "motor",
            DeviceKind::Imu => "imu",
            DeviceKind::Lift => "lift",
        }
    }

    /// Unknown tuples remain inventory-only in the settings workspace.
    pub fn supports_device_settings(self) -> bool {
        !matches!(self, DeviceKind::Unknown)
    }

    /// Position preset is a registered operation only for exact motor tuples.
    pub fn supports_position_preset(self) -> bool {
        matches!(self, DeviceKind::Motor)
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

/// Classify a node from its exact `0x1018` identity.
///
/// Only an exact motor entry is safe to route to CiA402 controls.
pub fn classify(vendor_id: u32, product_code: u32) -> DeviceKind {
    if let Some(device) = NON_MOTOR_DEVICES
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.product_code == product_code)
    {
        return device.kind;
    }

    if MOTOR_IDENTITIES.contains(&(vendor_id, product_code)) {
        DeviceKind::Motor
    } else {
        DeviceKind::Unknown
    }
}

pub fn display_name(vendor_id: u32, product_code: u32) -> Option<&'static str> {
    NON_MOTOR_DEVICES
        .iter()
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
        assert_eq!(classify(0x4859_444C, 0xAAAA_0001), DeviceKind::Motor);
    }

    #[test]
    fn operation_capabilities_follow_exact_classification() {
        let motor = classify(0x4859_444C, 0xAAAA_0002);
        assert!(motor.supports_device_settings());
        assert!(motor.supports_position_preset());

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
