//! Reusable safety gates for hexmeow STM32 DFU over CANopen.
//!
//! The crate deliberately separates read-only discovery from every proprietary
//! object and every SDO download:
//!
//! 1. read the complete standard `0x1018:00..=04` identity with exact widths;
//! 2. match `(vendor_id, product_code)` against a caller-supplied local registry;
//! 3. only for an enabled exact match, read `0x2102` and require an explicitly
//!    listed hardware version;
//! 4. parse a bounded `.meowpkg` from memory and bind it to that authorized
//!    target.
//!
//! Unknown, disabled, malformed, or sentinel identities never cause an SDO
//! download and never access a proprietary object. [`flash`] is the only DFU
//! mutation entry point in this crate and consumes a freshly revalidated,
//! non-constructible [`ReadyToFlash`] capability.

mod engine;
mod identity;
mod package;
mod profile;
mod transport;

pub use engine::{
    flash, CancellationToken, FlashError, FlashEvent, FlashOptions, FlashOutcome, FlashStage,
};
pub use identity::{
    authorize, observe_identity, revalidate, AuthorizationError, AuthorizedTarget,
    IdentitySnapshot, UnsupportedReason,
};
pub use package::{
    read_package_bytes, ImageMeta, IntegrityCheckedPackage, Manifest, MemberRef, PackageError,
    PackageLimits, PayloadFormat, Stm32ImageMode, FORMAT, MCU_STM32G0B1, MCU_STM32G431,
    MCU_STM32G474,
};
pub use profile::{
    revalidate_prepared, ArtifactPolicy, PreparedUpgrade, ProfileError, ReadyError, ReadyToFlash,
    RegisteredTarget, SupportPolicy, TargetClassification, TargetRegistry, UpgradePolicy,
};
pub use transport::{ObjectAddress, SdoTransport, TransportError};

#[cfg(feature = "can-bus")]
pub use transport::CanBusSdo;
