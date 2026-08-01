use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IdentityError {
    #[error("node ID {0} is outside 1..=127")]
    InvalidNode(u8),
    #[error("CANopen identity field {0} is an unprovisioned sentinel")]
    Sentinel(&'static str),
    #[error("Enter-IAP identity is {actual} bytes, expected exactly 12")]
    InvalidIapLength { actual: usize },
    #[error("Enter-IAP identity is unavailable (all fields are 0xFFFFFFFF)")]
    IapIdentityUnavailable,
}

/// A complete, strictly decoded CANopen 0x1018 snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CanopenIdentity {
    node_id: u8,
    vendor_id: u32,
    product_code: u32,
    revision_number: u32,
    serial_number: u32,
}

impl CanopenIdentity {
    pub fn new(
        node_id: u8,
        vendor_id: u32,
        product_code: u32,
        revision_number: u32,
        serial_number: u32,
    ) -> Result<Self, IdentityError> {
        if !(1..=127).contains(&node_id) {
            return Err(IdentityError::InvalidNode(node_id));
        }
        for (name, value) in [
            ("vendor_id", vendor_id),
            ("product_code", product_code),
            ("revision_number", revision_number),
            ("serial_number", serial_number),
        ] {
            if value == u32::MAX {
                return Err(IdentityError::Sentinel(name));
            }
        }
        Ok(Self {
            node_id,
            vendor_id,
            product_code,
            revision_number,
            serial_number,
        })
    }

    pub fn node_id(self) -> u8 {
        self.node_id
    }

    pub fn vendor_id(self) -> u32 {
        self.vendor_id
    }

    pub fn product_code(self) -> u32 {
        self.product_code
    }

    pub fn revision_number(self) -> u32 {
        self.revision_number
    }

    pub fn serial_number(self) -> u32 {
        self.serial_number
    }
}

/// The three little-endian U32 values returned by Enter-IAP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IapIdentity {
    device_id: u32,
    firmware_id: u32,
    current_version: u32,
}

impl IapIdentity {
    pub fn parse(bytes: &[u8]) -> Result<Self, IdentityError> {
        if bytes.len() != 12 {
            return Err(IdentityError::InvalidIapLength {
                actual: bytes.len(),
            });
        }
        let identity = Self {
            device_id: read_u32(bytes, 0),
            firmware_id: read_u32(bytes, 4),
            current_version: read_u32(bytes, 8),
        };
        if identity.device_id == u32::MAX
            && identity.firmware_id == u32::MAX
            && identity.current_version == u32::MAX
        {
            return Err(IdentityError::IapIdentityUnavailable);
        }
        Ok(identity)
    }

    pub fn device_id(self) -> u32 {
        self.device_id
    }

    pub fn firmware_id(self) -> u32 {
        self.firmware_id
    }

    pub fn current_version(self) -> u32 {
        self.current_version
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed slice"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_captured_enter_identity_layout() {
        let bytes = hex::decode("0100aaaa2510252008000000").unwrap();
        let identity = IapIdentity::parse(&bytes).unwrap();
        assert_eq!(identity.device_id(), 0xAAAA_0001);
        assert_eq!(identity.firmware_id(), 0x2025_1025);
        assert_eq!(identity.current_version(), 8);
    }

    #[test]
    fn rejects_all_ff_recovery_identity() {
        assert_eq!(
            IapIdentity::parse(&[0xFF; 12]).unwrap_err(),
            IdentityError::IapIdentityUnavailable
        );
    }

    #[test]
    fn rejects_canopen_sentinel() {
        assert_eq!(
            CanopenIdentity::new(1, 1, u32::MAX, 1, 1).unwrap_err(),
            IdentityError::Sentinel("product_code")
        );
    }
}
