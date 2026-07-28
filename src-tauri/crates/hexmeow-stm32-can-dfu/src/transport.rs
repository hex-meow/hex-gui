use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

/// A CANopen object address, retained in errors and test traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectAddress {
    pub index: u16,
    pub subindex: u8,
}

impl ObjectAddress {
    pub const fn new(index: u16, subindex: u8) -> Self {
        Self { index, subindex }
    }
}

impl fmt::Display for ObjectAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04X}:{:02X}", self.index, self.subindex)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportErrorKind {
    /// The write outcome is not known (timeout, lost response, or bus I/O).
    Ambiguous,
    /// The remote SDO server explicitly rejected the request.
    DefinitiveRejection,
}

/// Transport failure with the minimum outcome classification required for
/// safe write recovery.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct TransportError {
    kind: TransportErrorKind,
    message: String,
}

impl TransportError {
    /// Construct an ambiguous transport result. This is the conservative
    /// default for custom transports and test doubles.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: TransportErrorKind::Ambiguous,
            message: message.into(),
        }
    }

    /// Construct an error for an explicit server-side rejection.
    pub fn definitive_rejection(message: impl Into<String>) -> Self {
        Self {
            kind: TransportErrorKind::DefinitiveRejection,
            message: message.into(),
        }
    }

    pub const fn is_definitive_rejection(&self) -> bool {
        matches!(self.kind, TransportErrorKind::DefinitiveRejection)
    }
}

/// Injectable SDO I/O used by discovery and the streaming engine.
///
/// Discovery only calls [`SdoTransport::upload`].  Keeping `download` in this
/// trait lets mocks prove the zero-write invariant and avoids tying the policy
/// code to a particular CAN adapter.
#[async_trait]
pub trait SdoTransport: Send + Sync {
    async fn upload(
        &self,
        node_id: u8,
        object: ObjectAddress,
        timeout: Duration,
    ) -> Result<Vec<u8>, TransportError>;

    async fn download(
        &self,
        node_id: u8,
        object: ObjectAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), TransportError>;
}

/// Adapter from any `can-transport::CanBus` to the injectable SDO interface.
#[cfg(feature = "can-bus")]
pub struct CanBusSdo<'a, B: can_transport::CanBus + ?Sized> {
    bus: &'a B,
}

#[cfg(feature = "can-bus")]
impl<'a, B: can_transport::CanBus + ?Sized> CanBusSdo<'a, B> {
    pub const fn new(bus: &'a B) -> Self {
        Self { bus }
    }
}

#[cfg(feature = "can-bus")]
#[async_trait]
impl<B: can_transport::CanBus + ?Sized> SdoTransport for CanBusSdo<'_, B> {
    async fn upload(
        &self,
        node_id: u8,
        object: ObjectAddress,
        timeout: Duration,
    ) -> Result<Vec<u8>, TransportError> {
        canopen_sdo::asynch::upload_bytes(
            self.bus,
            node_id,
            object.index,
            object.subindex,
            Some(timeout),
        )
        .await
        .map_err(classify_canopen_error)
    }

    async fn download(
        &self,
        node_id: u8,
        object: ObjectAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), TransportError> {
        canopen_sdo::asynch::download_bytes(
            self.bus,
            node_id,
            object.index,
            object.subindex,
            data,
            Some(timeout),
        )
        .await
        .map_err(classify_canopen_error)
    }
}

#[cfg(feature = "can-bus")]
fn classify_canopen_error(error: canopen_sdo::asynch::AsyncSdoError) -> TransportError {
    let message = error.to_string();
    match error {
        canopen_sdo::asynch::AsyncSdoError::Sdo(canopen_sdo::SdoError::ServerAborted(_)) => {
            TransportError::definitive_rejection(message)
        }
        _ => TransportError::new(message),
    }
}
