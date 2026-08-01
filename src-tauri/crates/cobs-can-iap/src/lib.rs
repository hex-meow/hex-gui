//! Strict, product-gated COBS firmware updates over Classic CAN.

mod artifact;
mod engine;
mod identity;
mod policy;
mod wire;

pub use artifact::{ArtifactError, EncryptionMode, ImgArtifact, ImgLimits, IMG_TAG_SIZE};
pub use engine::{
    flash, CancellationToken, FlashError, FlashEvent, FlashOptions, FlashOutcome, FlashStage,
};
pub use identity::{CanopenIdentity, IapIdentity, IdentityError};
pub use policy::{
    AuthorizedTarget, IapPolicy, PolicyError, PreparedUpgrade, ReadyToFlash, RegisteredTarget,
    SupportPolicy, TargetClassification, TargetRegistry,
};
pub use wire::{Frame, FrameAssembler, FrameError, FunctionCode, MAX_DATA_LEN};
