use std::time::Duration;

use thiserror::Error;

use crate::profile::{RegisteredTarget, TargetClassification, TargetRegistry};
use crate::transport::{ObjectAddress, SdoTransport, TransportError};

const OD_IDENTITY: u16 = 0x1018;
const OD_HARDWARE_VERSION: ObjectAddress = ObjectAddress::new(0x2102, 0);
const IDENTITY_SUBCOUNT: u8 = 4;
const UNPROVISIONED: u32 = 0xFFFF_FFFF;

/// A complete, width-checked CANopen identity record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentitySnapshot {
    vendor_id: u32,
    product_code: u32,
    revision_number: u32,
    serial_number: u32,
}

impl IdentitySnapshot {
    pub const fn vendor_id(&self) -> u32 {
        self.vendor_id
    }

    pub const fn product_code(&self) -> u32 {
        self.product_code
    }

    pub const fn revision_number(&self) -> u32 {
        self.revision_number
    }

    pub const fn serial_number(&self) -> u32 {
        self.serial_number
    }

    pub(crate) fn first_sentinel_field(&self) -> Option<&'static str> {
        [
            ("vendor_id", self.vendor_id),
            ("product_code", self.product_code),
            ("revision_number", self.revision_number),
            ("serial_number", self.serial_number),
        ]
        .into_iter()
        .find_map(|(field, value)| (value == UNPROVISIONED).then_some(field))
    }
}

/// The capability token produced only after standard identity and `0x2102`
/// have passed an exact local policy.
#[derive(Debug, Clone)]
pub struct AuthorizedTarget {
    node_id: u8,
    identity: IdentitySnapshot,
    hardware_version: u32,
    target: RegisteredTarget,
}

impl AuthorizedTarget {
    pub const fn node_id(&self) -> u8 {
        self.node_id
    }

    pub const fn identity(&self) -> &IdentitySnapshot {
        &self.identity
    }

    pub const fn hardware_version(&self) -> u32 {
        self.hardware_version
    }

    pub const fn target(&self) -> &RegisteredTarget {
        &self.target
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UnsupportedReason {
    #[error("identity contains the unprovisioned sentinel at {field} (0xFFFF_FFFF)")]
    Sentinel { field: &'static str },
    #[error(
        "no local target profile matches vendor 0x{vendor_id:08X}, product 0x{product_code:08X}"
    )]
    UnknownIdentity { vendor_id: u32, product_code: u32 },
    #[error("target profile {profile_id:?} is locally disabled: {reason}")]
    Disabled { profile_id: String, reason: String },
    #[error(
        "target profile {profile_id:?} does not list hardware version 0x{hardware_version:08X}"
    )]
    UnknownHardware {
        profile_id: String,
        hardware_version: u32,
    },
}

#[derive(Debug, Error)]
pub enum AuthorizationError {
    #[error("SDO upload {object} failed: {source}")]
    Transport {
        object: ObjectAddress,
        #[source]
        source: TransportError,
    },
    #[error("0x1018:00 must be exactly one byte, got {actual} bytes")]
    SubcountWidth { actual: usize },
    #[error("0x1018:00 must equal 4, got {actual}")]
    SubcountValue { actual: u8 },
    #[error("{object} must be exactly four bytes, got {actual} bytes")]
    U32Width {
        object: ObjectAddress,
        actual: usize,
    },
    #[error(transparent)]
    Unsupported(#[from] UnsupportedReason),
    #[error(
        "target identity changed before the first write (previous {previous:?}, current {current:?})"
    )]
    IdentityChanged {
        previous: IdentitySnapshot,
        current: IdentitySnapshot,
    },
    #[error(
        "target hardware version changed before the first write (previous 0x{previous:08X}, current 0x{current:08X})"
    )]
    HardwareChanged { previous: u32, current: u32 },
    #[error(
        "stable device identity changed across the app/bootloader transition (previous {previous:?}, current {current:?})"
    )]
    SessionIdentityChanged {
        previous: IdentitySnapshot,
        current: IdentitySnapshot,
    },
}

/// Read and authorize a node without issuing any SDO download.
///
/// `0x2102` is proprietary and is therefore read only after the complete
/// `0x1018` snapshot hits an enabled local profile.
pub async fn authorize(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    registry: &TargetRegistry,
    timeout: Duration,
) -> Result<AuthorizedTarget, AuthorizationError> {
    let identity = observe_identity(sdo, node_id, timeout).await?;
    let target = match registry.classify(&identity) {
        TargetClassification::Enabled(target) => target,
        TargetClassification::Disabled(target) => {
            let reason = match target.support() {
                crate::profile::SupportPolicy::Disabled { reason } => reason.clone(),
                crate::profile::SupportPolicy::Enabled(_) => {
                    unreachable!("classification and support policy agree")
                }
            };
            return Err(UnsupportedReason::Disabled {
                profile_id: target.profile_id().to_owned(),
                reason,
            }
            .into());
        }
        TargetClassification::Unknown => {
            return Err(UnsupportedReason::UnknownIdentity {
                vendor_id: identity.vendor_id,
                product_code: identity.product_code,
            }
            .into())
        }
        TargetClassification::Sentinel { field } => {
            return Err(UnsupportedReason::Sentinel { field }.into())
        }
    };

    let policy = match target.support() {
        crate::profile::SupportPolicy::Enabled(policy) => policy,
        crate::profile::SupportPolicy::Disabled { .. } => {
            unreachable!("enabled classification contains an enabled policy")
        }
    };

    let hardware_version = read_exact_u32(sdo, node_id, OD_HARDWARE_VERSION, timeout).await?;
    if hardware_version == UNPROVISIONED {
        return Err(UnsupportedReason::Sentinel {
            field: "hardware_version",
        }
        .into());
    }
    if !policy.hardware_versions().contains(&hardware_version) {
        return Err(UnsupportedReason::UnknownHardware {
            profile_id: target.profile_id().to_owned(),
            hardware_version,
        }
        .into());
    }

    Ok(AuthorizedTarget {
        node_id,
        identity,
        hardware_version,
        target: target.clone(),
    })
}

/// Read the complete standard identity record for UI display and local
/// classification.  This function never accesses a proprietary object and
/// never sends an SDO download, even if the snapshot is unknown or contains a
/// sentinel.
pub async fn observe_identity(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    timeout: Duration,
) -> Result<IdentitySnapshot, AuthorizationError> {
    read_identity(sdo, node_id, timeout).await
}

/// Repeat the complete authorization immediately before the first upgrade
/// write, then require the same physical/session identity byte-for-byte.
pub async fn revalidate(
    sdo: &(impl SdoTransport + ?Sized),
    prior: &AuthorizedTarget,
    registry: &TargetRegistry,
    timeout: Duration,
) -> Result<AuthorizedTarget, AuthorizationError> {
    let current = authorize(sdo, prior.node_id, registry, timeout).await?;
    if current.identity != prior.identity {
        return Err(AuthorizationError::IdentityChanged {
            previous: prior.identity,
            current: current.identity,
        });
    }
    if current.hardware_version != prior.hardware_version {
        return Err(AuthorizationError::HardwareChanged {
            previous: prior.hardware_version,
            current: current.hardware_version,
        });
    }
    Ok(current)
}

async fn read_identity(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    timeout: Duration,
) -> Result<IdentitySnapshot, AuthorizationError> {
    let subcount_object = ObjectAddress::new(OD_IDENTITY, 0);
    let subcount = upload(sdo, node_id, subcount_object, timeout).await?;
    if subcount.len() != 1 {
        return Err(AuthorizationError::SubcountWidth {
            actual: subcount.len(),
        });
    }
    if subcount[0] != IDENTITY_SUBCOUNT {
        return Err(AuthorizationError::SubcountValue {
            actual: subcount[0],
        });
    }

    Ok(IdentitySnapshot {
        vendor_id: read_exact_u32(sdo, node_id, ObjectAddress::new(OD_IDENTITY, 1), timeout)
            .await?,
        product_code: read_exact_u32(sdo, node_id, ObjectAddress::new(OD_IDENTITY, 2), timeout)
            .await?,
        revision_number: read_exact_u32(sdo, node_id, ObjectAddress::new(OD_IDENTITY, 3), timeout)
            .await?,
        serial_number: read_exact_u32(sdo, node_id, ObjectAddress::new(OD_IDENTITY, 4), timeout)
            .await?,
    })
}

async fn read_exact_u32(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    object: ObjectAddress,
    timeout: Duration,
) -> Result<u32, AuthorizationError> {
    let bytes = upload(sdo, node_id, object, timeout).await?;
    let bytes: [u8; 4] =
        bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| AuthorizationError::U32Width {
                object,
                actual: bytes.len(),
            })?;
    Ok(u32::from_le_bytes(bytes))
}

/// Confirm the same physical board after an expected app/bootloader transition.
///
/// `0x1018:03` and `0x1008` may legitimately change with the running firmware.
/// Vendor, product, serial and exact `0x2102` remain session-fixation fields.
pub(crate) async fn confirm_same_device_across_firmware(
    sdo: &(impl SdoTransport + ?Sized),
    prior: &AuthorizedTarget,
    timeout: Duration,
) -> Result<IdentitySnapshot, AuthorizationError> {
    let current = observe_identity(sdo, prior.node_id, timeout).await?;
    if let Some(field) = current.first_sentinel_field() {
        return Err(UnsupportedReason::Sentinel { field }.into());
    }
    if current.vendor_id != prior.identity.vendor_id
        || current.product_code != prior.identity.product_code
        || current.serial_number != prior.identity.serial_number
    {
        return Err(AuthorizationError::SessionIdentityChanged {
            previous: prior.identity,
            current,
        });
    }

    // Only after the stable standard identity matches the already-authorized
    // target may this transition check access the proprietary hardware object.
    let hardware_version = read_exact_u32(sdo, prior.node_id, OD_HARDWARE_VERSION, timeout).await?;
    if hardware_version == UNPROVISIONED {
        return Err(UnsupportedReason::Sentinel {
            field: "hardware_version",
        }
        .into());
    }
    if hardware_version != prior.hardware_version {
        return Err(AuthorizationError::HardwareChanged {
            previous: prior.hardware_version,
            current: hardware_version,
        });
    }
    Ok(current)
}

async fn upload(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    object: ObjectAddress,
    timeout: Duration,
) -> Result<Vec<u8>, AuthorizationError> {
    sdo.upload(node_id, object, timeout)
        .await
        .map_err(|source| AuthorizationError::Transport { object, source })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use image_container::VENDOR_ID;

    use super::*;
    use crate::profile::{ArtifactPolicy, RegisteredTarget, TargetRegistry, UpgradePolicy};

    const PRODUCT: u32 = 0x1234_5678;
    const HW: u32 = 0x0002_0001;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Operation {
        Upload(ObjectAddress),
        Download(ObjectAddress),
    }

    struct MockSdo {
        values: Mutex<HashMap<ObjectAddress, Vec<u8>>>,
        operations: Mutex<Vec<Operation>>,
    }

    impl MockSdo {
        fn identity(vendor: u32, product: u32, revision: u32, serial: u32) -> Self {
            let values = HashMap::from([
                (ObjectAddress::new(0x1018, 0), vec![4]),
                (ObjectAddress::new(0x1018, 1), vendor.to_le_bytes().to_vec()),
                (
                    ObjectAddress::new(0x1018, 2),
                    product.to_le_bytes().to_vec(),
                ),
                (
                    ObjectAddress::new(0x1018, 3),
                    revision.to_le_bytes().to_vec(),
                ),
                (ObjectAddress::new(0x1018, 4), serial.to_le_bytes().to_vec()),
                (OD_HARDWARE_VERSION, HW.to_le_bytes().to_vec()),
            ]);
            Self {
                values: Mutex::new(values),
                operations: Mutex::new(Vec::new()),
            }
        }

        fn set(&self, object: ObjectAddress, value: Vec<u8>) {
            self.values.lock().unwrap().insert(object, value);
        }

        fn operations(&self) -> Vec<Operation> {
            self.operations.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SdoTransport for MockSdo {
        async fn upload(
            &self,
            _node_id: u8,
            object: ObjectAddress,
            _timeout: Duration,
        ) -> Result<Vec<u8>, TransportError> {
            self.operations
                .lock()
                .unwrap()
                .push(Operation::Upload(object));
            self.values
                .lock()
                .unwrap()
                .get(&object)
                .cloned()
                .ok_or_else(|| TransportError::new("missing mock object"))
        }

        async fn download(
            &self,
            _node_id: u8,
            object: ObjectAddress,
            _data: &[u8],
            _timeout: Duration,
        ) -> Result<(), TransportError> {
            self.operations
                .lock()
                .unwrap()
                .push(Operation::Download(object));
            Ok(())
        }
    }

    fn enabled_registry() -> TargetRegistry {
        let policy = UpgradePolicy::new(
            "stm32g431",
            vec![HW],
            vec![0x42],
            ArtifactPolicy::UnprotectedV1,
        )
        .unwrap();
        TargetRegistry::new(vec![RegisteredTarget::enabled(
            "test-product",
            VENDOR_ID,
            PRODUCT,
            policy,
        )
        .unwrap()])
        .unwrap()
    }

    fn assert_no_mutation_or_proprietary_access(sdo: &MockSdo) {
        let operations = sdo.operations();
        assert!(
            operations
                .iter()
                .all(|op| matches!(op, Operation::Upload(ObjectAddress { index: 0x1018, .. }))),
            "unexpected operation trace: {operations:?}"
        );
    }

    #[tokio::test]
    async fn unknown_identity_reads_complete_1018_but_never_proprietary_or_download() {
        let sdo = MockSdo::identity(VENDOR_ID, 0xDEAD_BEEF, 0x0001_0000, 7);
        let error = authorize(&sdo, 1, &enabled_registry(), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AuthorizationError::Unsupported(UnsupportedReason::UnknownIdentity { .. })
        ));
        assert_eq!(sdo.operations().len(), 5);
        assert_no_mutation_or_proprietary_access(&sdo);
    }

    #[tokio::test]
    async fn sentinel_identity_never_reaches_proprietary_objects() {
        let sdo = MockSdo::identity(VENDOR_ID, PRODUCT, 0x0001_0000, UNPROVISIONED);
        let error = authorize(&sdo, 1, &enabled_registry(), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AuthorizationError::Unsupported(UnsupportedReason::Sentinel {
                field: "serial_number"
            })
        ));
        assert_no_mutation_or_proprietary_access(&sdo);
    }

    #[tokio::test]
    async fn disabled_known_target_never_reads_hardware_version() {
        let registry = TargetRegistry::new(vec![RegisteredTarget::disabled(
            "not-qualified",
            VENDOR_ID,
            PRODUCT,
            "hardware mapping has not been qualified",
        )
        .unwrap()])
        .unwrap();
        let sdo = MockSdo::identity(VENDOR_ID, PRODUCT, 0x0001_0000, 7);
        let error = authorize(&sdo, 1, &registry, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AuthorizationError::Unsupported(UnsupportedReason::Disabled { .. })
        ));
        assert_no_mutation_or_proprietary_access(&sdo);
    }

    #[tokio::test]
    async fn short_u32_is_rejected_instead_of_zero_extended() {
        let sdo = MockSdo::identity(VENDOR_ID, PRODUCT, 0x0001_0000, 7);
        sdo.set(ObjectAddress::new(0x1018, 2), vec![0x78, 0x56]);
        let error = authorize(&sdo, 1, &enabled_registry(), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AuthorizationError::U32Width {
                object: ObjectAddress {
                    index: 0x1018,
                    subindex: 2
                },
                actual: 2
            }
        ));
        assert_no_mutation_or_proprietary_access(&sdo);
    }

    #[tokio::test]
    async fn successful_authorization_reads_hardware_but_never_downloads() {
        let sdo = MockSdo::identity(VENDOR_ID, PRODUCT, 0x0001_0000, 7);
        let authorized = authorize(&sdo, 1, &enabled_registry(), Duration::from_millis(20))
            .await
            .unwrap();
        assert_eq!(authorized.hardware_version(), HW);
        assert_eq!(
            sdo.operations(),
            vec![
                Operation::Upload(ObjectAddress::new(0x1018, 0)),
                Operation::Upload(ObjectAddress::new(0x1018, 1)),
                Operation::Upload(ObjectAddress::new(0x1018, 2)),
                Operation::Upload(ObjectAddress::new(0x1018, 3)),
                Operation::Upload(ObjectAddress::new(0x1018, 4)),
                Operation::Upload(OD_HARDWARE_VERSION),
            ]
        );
    }

    #[tokio::test]
    async fn revalidation_detects_serial_swap_before_any_write() {
        let sdo = MockSdo::identity(VENDOR_ID, PRODUCT, 0x0001_0000, 7);
        let registry = enabled_registry();
        let first = authorize(&sdo, 1, &registry, Duration::from_millis(20))
            .await
            .unwrap();
        sdo.set(ObjectAddress::new(0x1018, 4), 8u32.to_le_bytes().to_vec());
        let error = revalidate(&sdo, &first, &registry, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthorizationError::IdentityChanged { .. }));
        assert!(sdo
            .operations()
            .iter()
            .all(|operation| !matches!(operation, Operation::Download(_))));
    }
}
