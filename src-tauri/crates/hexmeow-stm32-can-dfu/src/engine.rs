//! Output-free STM32 CANopen DFU streaming state machine.
//!
//! The only public mutation entry point consumes [`ReadyToFlash`], which can
//! only be produced by a fresh [`crate::revalidate_prepared`] call.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::identity::{
    confirm_same_device_across_firmware, revalidate, AuthorizationError, IdentitySnapshot,
};
use crate::profile::{ReadyToFlash, TargetRegistry};
use crate::transport::{ObjectAddress, SdoTransport, TransportError};

const OD_DEVICE_NAME: ObjectAddress = ObjectAddress::new(0x1008, 0);
const OD_PROGRAM_CONTROL: ObjectAddress = ObjectAddress::new(0x1F51, 1);
const OD_FLASH_STATUS: ObjectAddress = ObjectAddress::new(0x1F57, 1);
const OD_FW_OFFSET: ObjectAddress = ObjectAddress::new(od_consts::OD_FW_DOWNLOAD, 2);
const OD_FW_DATA: ObjectAddress = ObjectAddress::new(od_consts::OD_FW_DOWNLOAD, 3);
const OD_FW_HEADER: ObjectAddress = ObjectAddress::new(od_consts::OD_FW_DOWNLOAD, 4);
const OD_FW_BYTES: ObjectAddress = ObjectAddress::new(od_consts::OD_FW_DOWNLOAD, 5);

const PC_STOP: u8 = 0x00;
const PC_START: u8 = 0x01;
const PC_CLEAR: u8 = 0x03;
const BL_NAME_PREFIX: &str = "hexmeow-bl-";
const BL_NAME_STM32G4: &str = "hexmeow-bl-stm32g4";
const BL_NAME_STM32G0B1: &str = "hexmeow-bl-stm32g0b1";
const WRITE_GRANULARITY: usize = 8;
const MAX_SDO_CHUNK: usize = 256;
const MAX_DEVICE_NAME: usize = 64;
const HARD_MAX_AUTHORIZATION_AGE: Duration = Duration::from_secs(2);
const MAX_CONFIGURED_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// Cooperative cancellation shared between a UI command and its cancel
/// handler. An in-flight SDO transaction finishes; cancellation is checked
/// before every subsequent state transition or write.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub struct FlashOptions {
    pub chunk_size: usize,
    /// Total number of data resynchronizations permitted during one stream.
    pub max_retries: u32,
    pub operation_timeout: Duration,
    pub chunk_timeout: Duration,
    pub start_timeout: Duration,
    pub bootloader_timeout: Duration,
    pub application_timeout: Duration,
    /// Delay after START so the final response can leave the wire and the
    /// application can initialize before it is probed.
    pub application_settle_delay: Duration,
    pub poll_interval: Duration,
    /// A `ReadyToFlash` token must be consumed promptly after final identity
    /// revalidation.
    pub max_authorization_age: Duration,
}

impl Default for FlashOptions {
    fn default() -> Self {
        Self {
            chunk_size: MAX_SDO_CHUNK,
            max_retries: 3,
            operation_timeout: Duration::from_secs(30),
            chunk_timeout: Duration::from_secs(30),
            start_timeout: Duration::from_secs(30),
            bootloader_timeout: Duration::from_secs(15),
            application_timeout: Duration::from_secs(15),
            application_settle_delay: Duration::from_secs(1),
            poll_interval: Duration::from_millis(200),
            max_authorization_age: Duration::from_secs(2),
        }
    }
}

impl FlashOptions {
    fn validate(&self) -> Result<(), FlashError> {
        if self.chunk_size == 0
            || self.chunk_size > MAX_SDO_CHUNK
            || self.chunk_size % WRITE_GRANULARITY != 0
        {
            return Err(FlashError::InvalidOptions(format!(
                "chunk_size must be an 8-byte multiple in 8..={MAX_SDO_CHUNK}, got {}",
                self.chunk_size
            )));
        }
        for (name, value) in [
            ("operation_timeout", self.operation_timeout),
            ("chunk_timeout", self.chunk_timeout),
            ("start_timeout", self.start_timeout),
            ("bootloader_timeout", self.bootloader_timeout),
            ("application_timeout", self.application_timeout),
            ("poll_interval", self.poll_interval),
            ("max_authorization_age", self.max_authorization_age),
        ] {
            if value.is_zero() {
                return Err(FlashError::InvalidOptions(format!(
                    "{name} must be non-zero"
                )));
            }
            if value > MAX_CONFIGURED_DURATION {
                return Err(FlashError::InvalidOptions(format!(
                    "{name} must not exceed {MAX_CONFIGURED_DURATION:?}"
                )));
            }
        }
        if self.application_settle_delay > MAX_CONFIGURED_DURATION {
            return Err(FlashError::InvalidOptions(format!(
                "application_settle_delay must not exceed {MAX_CONFIGURED_DURATION:?}"
            )));
        }
        if self.max_authorization_age > HARD_MAX_AUTHORIZATION_AGE {
            return Err(FlashError::InvalidOptions(format!(
                "max_authorization_age must not exceed the hard safety limit {HARD_MAX_AUTHORIZATION_AGE:?}"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashStage {
    Claiming,
    WaitingForBootloader,
    BootloaderAuthorized,
    Header,
    Clear,
    Streaming,
    VerifyingAndStarting,
    WaitingForApplication,
    ApplicationConfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlashEvent {
    Stage(FlashStage),
    Progress {
        written: usize,
        total: usize,
    },
    Resynchronized {
        attempted_offset: usize,
        authoritative_offset: usize,
        retries_left: u32,
    },
    StartAcknowledgement {
        acknowledged: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashOutcome {
    pub application_identity: IdentitySnapshot,
    pub hardware_version: u32,
    pub streamed_bytes: usize,
    /// False means the SDO result was ambiguous but the same physical board
    /// subsequently answered as an application.
    pub start_acknowledged: bool,
}

/// Flash a prepared STM32 v1 artifact.
///
/// This function owns every DFU SDO download in the public core and cannot be
/// called with an unknown identity: `ReadyToFlash` has private fields and is
/// created only by `revalidate_prepared`.
pub async fn flash(
    sdo: &(impl SdoTransport + ?Sized),
    ready: ReadyToFlash,
    options: &FlashOptions,
    cancellation: &CancellationToken,
    mut progress: impl FnMut(FlashEvent),
) -> Result<FlashOutcome, FlashError> {
    options.validate()?;
    let authorization_age = ready.authorization_age();
    if authorization_age > options.max_authorization_age {
        return Err(FlashError::StaleAuthorization {
            age: authorization_age,
            maximum: options.max_authorization_age,
        });
    }
    check_cancel(cancellation, FlashStage::Claiming)?;

    let node_id = ready.target().node_id();
    progress(FlashEvent::Stage(FlashStage::Claiming));
    check_cancel(cancellation, FlashStage::Claiming)?;

    // `ReadyToFlash` cannot safely carry a trait-object transport identity.
    // Bind the capability to the actual transport supplied to this call by
    // repeating the complete exact identity + hardware authorization here.
    // This happens after the caller callback and immediately before STOP, so a
    // delayed callback or a token accidentally routed to another adapter can
    // never trigger an unknown device.
    let final_registry = TargetRegistry::new(vec![ready.target().target().clone()])
        .map_err(|_| FlashError::InvalidReadyTarget)?;
    revalidate(
        sdo,
        ready.target(),
        &final_registry,
        options.operation_timeout,
    )
    .await?;
    check_cancel(cancellation, FlashStage::Claiming)?;

    // Claim unconditionally after authorization. Apps reset into the BL; an
    // already-running BL treats this as an idempotent stay request. The ACK
    // can be lost during reset, so bootloader identity is authoritative.
    let initial_claim = match sdo
        .download(
            node_id,
            OD_PROGRAM_CONTROL,
            &[PC_STOP],
            options.operation_timeout,
        )
        .await
    {
        Ok(()) => None,
        Err(source) if source.is_definitive_rejection() => {
            return Err(FlashError::Transport {
                operation: "claiming the bootloader",
                source,
            })
        }
        Err(source) => Some(source.to_string()),
    };

    progress(FlashEvent::Stage(FlashStage::WaitingForBootloader));
    let expected_bootloader_name = expected_bootloader_name(&ready)?;
    wait_for_bootloader(
        sdo,
        node_id,
        expected_bootloader_name,
        options,
        cancellation,
        initial_claim.as_deref(),
    )
    .await?;

    // Before header/clear/data, require the same vendor/product/serial and
    // exact 0x2102. Revision and name may differ between app and bootloader.
    confirm_same_device_across_firmware(sdo, ready.target(), options.operation_timeout).await?;

    // Re-claim the now-confirmed BL so its bounded listen window cannot expire
    // during package setup. This is idempotent and precedes all erase.
    write_control_retry(
        sdo,
        node_id,
        PC_STOP,
        options.operation_timeout,
        options.max_retries,
        cancellation,
        FlashStage::BootloaderAuthorized,
    )
    .await?;
    progress(FlashEvent::Stage(FlashStage::BootloaderAuthorized));

    check_cancel(cancellation, FlashStage::Header)?;
    progress(FlashEvent::Stage(FlashStage::Header));
    write_header_verified(
        sdo,
        node_id,
        ready.package().header(),
        options,
        cancellation,
    )
    .await?;

    check_cancel(cancellation, FlashStage::Clear)?;
    progress(FlashEvent::Stage(FlashStage::Clear));
    write_control_retry(
        sdo,
        node_id,
        PC_CLEAR,
        options.operation_timeout,
        options.max_retries,
        cancellation,
        FlashStage::Clear,
    )
    .await?;

    let mut wire = ready.package().image().to_vec();
    let remainder = wire.len() % WRITE_GRANULARITY;
    if remainder != 0 {
        wire.resize(wire.len() + WRITE_GRANULARITY - remainder, 0xFF);
    }

    progress(FlashEvent::Stage(FlashStage::Streaming));
    progress(FlashEvent::Progress {
        written: 0,
        total: wire.len(),
    });
    stream_v1(sdo, node_id, &wire, options, cancellation, &mut progress).await?;

    check_cancel(cancellation, FlashStage::VerifyingAndStarting)?;
    progress(FlashEvent::Stage(FlashStage::VerifyingAndStarting));
    check_cancel(cancellation, FlashStage::VerifyingAndStarting)?;
    let start_acknowledged = match sdo
        .download(
            node_id,
            OD_PROGRAM_CONTROL,
            &[PC_START],
            options.start_timeout,
        )
        .await
    {
        Ok(()) => true,
        Err(source) if source.is_definitive_rejection() => {
            return Err(FlashError::Transport {
                operation: "starting the application",
                source,
            })
        }
        Err(_) => false,
    };
    progress(FlashEvent::StartAcknowledgement {
        acknowledged: start_acknowledged,
    });

    progress(FlashEvent::Stage(FlashStage::WaitingForApplication));
    if !options.application_settle_delay.is_zero() {
        tokio::time::sleep(options.application_settle_delay).await;
    }
    let application_identity = wait_for_application(sdo, &ready, options).await?;
    progress(FlashEvent::Stage(FlashStage::ApplicationConfirmed));

    Ok(FlashOutcome {
        application_identity,
        hardware_version: ready.target().hardware_version(),
        streamed_bytes: wire.len(),
        start_acknowledged,
    })
}

async fn write_control_retry(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    value: u8,
    timeout: Duration,
    max_retries: u32,
    cancellation: &CancellationToken,
    stage: FlashStage,
) -> Result<(), FlashError> {
    let mut attempt = 0u32;
    loop {
        check_cancel(cancellation, stage)?;
        match sdo
            .download(node_id, OD_PROGRAM_CONTROL, &[value], timeout)
            .await
        {
            Ok(()) => return Ok(()),
            Err(source) if source.is_definitive_rejection() => {
                return Err(FlashError::Transport {
                    operation: "writing program control",
                    source,
                })
            }
            Err(_) if attempt < max_retries => attempt += 1,
            Err(source) => {
                return Err(FlashError::Transport {
                    operation: "writing program control",
                    source,
                })
            }
        }
    }
}

async fn write_header_verified(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    header: &[u8; image_container::HEADER_LEN],
    options: &FlashOptions,
    cancellation: &CancellationToken,
) -> Result<(), FlashError> {
    let mut attempt = 0u32;
    loop {
        check_cancel(cancellation, FlashStage::Header)?;
        match sdo
            .download(node_id, OD_FW_HEADER, header, options.operation_timeout)
            .await
        {
            Ok(()) => return Ok(()),
            Err(source) if source.is_definitive_rejection() => {
                return Err(FlashError::Transport {
                    operation: "writing the container header",
                    source,
                })
            }
            Err(write_error) => {
                // Header writes are non-destructive. Resolve a lost ACK by
                // reading the device copy; never assume success.
                if let Ok(device_header) = sdo
                    .upload(node_id, OD_FW_HEADER, options.operation_timeout)
                    .await
                {
                    if device_header == header {
                        return Ok(());
                    }
                }
                if attempt >= options.max_retries {
                    return Err(FlashError::HeaderNotAccepted {
                        last_error: write_error.to_string(),
                    });
                }
                attempt += 1;
            }
        }
    }
}

async fn stream_v1(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    wire: &[u8],
    options: &FlashOptions,
    cancellation: &CancellationToken,
    progress: &mut impl FnMut(FlashEvent),
) -> Result<(), FlashError> {
    let mut offset = 0usize;
    let mut retries_left = options.max_retries;

    while offset < wire.len() {
        check_cancel(cancellation, FlashStage::Streaming)?;
        let end = (offset + options.chunk_size).min(wire.len());
        match sdo
            .download(
                node_id,
                OD_FW_DATA,
                &wire[offset..end],
                options.chunk_timeout,
            )
            .await
        {
            Ok(()) => {
                offset = end;
                progress(FlashEvent::Progress {
                    written: offset,
                    total: wire.len(),
                });
            }
            Err(source) if source.is_definitive_rejection() => {
                return Err(FlashError::Transport {
                    operation: "streaming firmware data",
                    source,
                })
            }
            Err(source) => {
                check_cancel(cancellation, FlashStage::Streaming)?;
                let authoritative =
                    read_exact_u32(sdo, node_id, OD_FW_BYTES, options.operation_timeout)
                        .await
                        .map_err(|progress_error| FlashError::AmbiguousChunk {
                            offset,
                            write_error: source.to_string(),
                            progress_error: progress_error.to_string(),
                        })? as usize;
                if authoritative > wire.len()
                    || authoritative > end
                    || authoritative % WRITE_GRANULARITY != 0
                {
                    return Err(FlashError::InvalidAuthoritativeOffset {
                        offset: authoritative,
                        total: wire.len(),
                        attempted_end: end,
                    });
                }
                if authoritative == end {
                    // The complete chunk committed and only its ACK was lost.
                    // Do not resend it and do not spend a retry.
                    offset = end;
                    progress(FlashEvent::Progress {
                        written: offset,
                        total: wire.len(),
                    });
                    continue;
                }
                if retries_left == 0 {
                    return Err(FlashError::RetriesExhausted {
                        offset,
                        authoritative_offset: authoritative,
                    });
                }
                retries_left -= 1;
                write_offset_verified(sdo, node_id, authoritative, options.operation_timeout)
                    .await?;
                progress(FlashEvent::Resynchronized {
                    attempted_offset: offset,
                    authoritative_offset: authoritative,
                    retries_left,
                });
                offset = authoritative;
            }
        }
    }

    let final_offset = read_exact_u32(sdo, node_id, OD_FW_BYTES, options.operation_timeout)
        .await
        .map_err(|source| FlashError::Transport {
            operation: "reading final authoritative byte count",
            source,
        })? as usize;
    if final_offset != wire.len() {
        return Err(FlashError::FinalOffsetMismatch {
            expected: wire.len(),
            actual: final_offset,
        });
    }
    Ok(())
}

async fn write_offset_verified(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    offset: usize,
    timeout: Duration,
) -> Result<(), FlashError> {
    let offset_u32: u32 = offset
        .try_into()
        .map_err(|_| FlashError::OffsetTooLarge(offset))?;
    match sdo
        .download(node_id, OD_FW_OFFSET, &offset_u32.to_le_bytes(), timeout)
        .await
    {
        Ok(()) => Ok(()),
        Err(source) if source.is_definitive_rejection() => Err(FlashError::Transport {
            operation: "resynchronizing the firmware offset",
            source,
        }),
        Err(write_error) => {
            let readback = read_exact_u32(sdo, node_id, OD_FW_OFFSET, timeout)
                .await
                .map_err(|read_error| FlashError::OffsetResyncAmbiguous {
                    offset,
                    write_error: write_error.to_string(),
                    read_error: read_error.to_string(),
                })?;
            if readback == offset_u32 {
                Ok(())
            } else {
                Err(FlashError::OffsetResyncMismatch {
                    expected: offset_u32,
                    actual: readback,
                })
            }
        }
    }
}

async fn wait_for_bootloader(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    expected_name: &'static str,
    options: &FlashOptions,
    cancellation: &CancellationToken,
    initial_claim_error: Option<&str>,
) -> Result<(), FlashError> {
    let deadline = Instant::now()
        .checked_add(options.bootloader_timeout)
        .ok_or_else(|| {
            FlashError::InvalidOptions("bootloader_timeout exceeds the monotonic clock".into())
        })?;
    let claim_detail = initial_claim_error.map(|error| format!("initial claim failed: {error}"));
    loop {
        check_cancel(cancellation, FlashStage::WaitingForBootloader)?;
        let last_observation = match read_device_name(sdo, node_id, options.operation_timeout).await
        {
            Ok(name) if name == expected_name => return Ok(()),
            Ok(name) => format!("last device name was {name:?}"),
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            let detail = match claim_detail.as_deref() {
                Some(claim) => format!("{claim}; {last_observation}"),
                None => last_observation,
            };
            return Err(FlashError::FirmwareWaitTimeout {
                expected: expected_name,
                detail,
            });
        }
        tokio::time::sleep(options.poll_interval).await;
    }
}

async fn wait_for_application(
    sdo: &(impl SdoTransport + ?Sized),
    ready: &ReadyToFlash,
    options: &FlashOptions,
) -> Result<IdentitySnapshot, FlashError> {
    let deadline = Instant::now()
        .checked_add(options.application_timeout)
        .ok_or_else(|| {
            FlashError::InvalidOptions("application_timeout exceeds the monotonic clock".into())
        })?;
    loop {
        let (last_observation, bootloader_status) = match read_device_name(
            sdo,
            ready.target().node_id(),
            options.operation_timeout,
        )
        .await
        {
            Ok(name) if name.starts_with(BL_NAME_PREFIX) => {
                let status = read_exact_u32(
                    sdo,
                    ready.target().node_id(),
                    OD_FLASH_STATUS,
                    options.operation_timeout,
                )
                .await
                .ok();
                (
                    format!("bootloader still active with flash status {status:?}"),
                    Some(status),
                )
            }
            Ok(_) => match confirm_same_device_across_firmware(
                sdo,
                ready.target(),
                options.operation_timeout,
            )
            .await
            {
                Ok(identity) => {
                    let expected = ready.package().manifest().firmware_version;
                    let actual = identity.revision_number();
                    if actual != expected {
                        return Err(FlashError::ApplicationRevisionMismatch { expected, actual });
                    }
                    return Ok(identity);
                }
                Err(error) => (error.to_string(), None),
            },
            Err(error) => (error.to_string(), None),
        };
        if Instant::now() >= deadline {
            if let Some(status) = bootloader_status {
                return Err(FlashError::StartRejected { status });
            }
            return Err(FlashError::FirmwareWaitTimeout {
                expected: "application",
                detail: last_observation,
            });
        }
        tokio::time::sleep(options.poll_interval).await;
    }
}

fn expected_bootloader_name(ready: &ReadyToFlash) -> Result<&'static str, FlashError> {
    let policy = match ready.target().target().support() {
        crate::SupportPolicy::Enabled(policy) => policy,
        crate::SupportPolicy::Disabled { .. } => return Err(FlashError::InvalidReadyTarget),
    };
    match policy.mcu() {
        crate::MCU_STM32G431 | crate::MCU_STM32G474 => Ok(BL_NAME_STM32G4),
        crate::MCU_STM32G0B1 => Ok(BL_NAME_STM32G0B1),
        _ => Err(FlashError::InvalidReadyTarget),
    }
}

async fn read_device_name(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    timeout: Duration,
) -> Result<String, FlashError> {
    let bytes = sdo
        .upload(node_id, OD_DEVICE_NAME, timeout)
        .await
        .map_err(|source| FlashError::Transport {
            operation: "reading 0x1008 device name",
            source,
        })?;
    if bytes.len() > MAX_DEVICE_NAME {
        return Err(FlashError::DeviceNameTooLong {
            actual: bytes.len(),
            maximum: MAX_DEVICE_NAME,
        });
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end])
        .map(str::to_owned)
        .map_err(|_| FlashError::InvalidDeviceName)
}

async fn read_exact_u32(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    object: ObjectAddress,
    timeout: Duration,
) -> Result<u32, TransportError> {
    let bytes = sdo.upload(node_id, object, timeout).await?;
    let bytes: [u8; 4] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        TransportError::new(format!(
            "{object} must be exactly four bytes, got {}",
            bytes.len()
        ))
    })?;
    Ok(u32::from_le_bytes(bytes))
}

fn check_cancel(cancellation: &CancellationToken, stage: FlashStage) -> Result<(), FlashError> {
    if cancellation.is_cancelled() {
        Err(FlashError::Cancelled { stage })
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum FlashError {
    #[error("invalid flash options: {0}")]
    InvalidOptions(String),
    #[error("authorization token is stale (age {age:?}, maximum {maximum:?})")]
    StaleAuthorization { age: Duration, maximum: Duration },
    #[error("upgrade cancelled during {stage:?}; device remains recoverable in the bootloader")]
    Cancelled { stage: FlashStage },
    #[error("{operation}: {source}")]
    Transport {
        operation: &'static str,
        #[source]
        source: TransportError,
    },
    #[error(transparent)]
    Authorization(#[from] AuthorizationError),
    #[error("timed out waiting for {expected}: {detail}")]
    FirmwareWaitTimeout {
        expected: &'static str,
        detail: String,
    },
    #[error("device name is {actual} bytes, above the {maximum}-byte bound")]
    DeviceNameTooLong { actual: usize, maximum: usize },
    #[error("device name is not valid UTF-8")]
    InvalidDeviceName,
    #[error("container header could not be confirmed by readback: {last_error}")]
    HeaderNotAccepted { last_error: String },
    #[error(
        "data write at offset {offset} was ambiguous ({write_error}); authoritative progress read also failed ({progress_error}); refusing blind retry"
    )]
    AmbiguousChunk {
        offset: usize,
        write_error: String,
        progress_error: String,
    },
    #[error(
        "device reported invalid authoritative offset {offset} for total {total} after attempted end {attempted_end}"
    )]
    InvalidAuthoritativeOffset {
        offset: usize,
        total: usize,
        attempted_end: usize,
    },
    #[error("data retries exhausted at offset {offset}; device reports {authoritative_offset}")]
    RetriesExhausted {
        offset: usize,
        authoritative_offset: usize,
    },
    #[error("offset {0} does not fit the protocol u32")]
    OffsetTooLarge(usize),
    #[error(
        "offset resync to {offset} was ambiguous ({write_error}); readback failed ({read_error})"
    )]
    OffsetResyncAmbiguous {
        offset: usize,
        write_error: String,
        read_error: String,
    },
    #[error("offset resync readback mismatch: expected {expected}, got {actual}")]
    OffsetResyncMismatch { expected: u32, actual: u32 },
    #[error("final device byte count mismatch: expected {expected}, got {actual}")]
    FinalOffsetMismatch { expected: usize, actual: usize },
    #[error("bootloader rejected start; flash status word: {status:?}")]
    StartRejected { status: Option<u32> },
    #[error("ready target contains an unsupported or disabled MCU policy")]
    InvalidReadyTarget,
    #[error(
        "application revision mismatch after start: expected 0x{expected:08X}, got 0x{actual:08X}"
    )]
    ApplicationRevisionMismatch { expected: u32, actual: u32 },
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use image_container::{HeaderBuilder, FORMAT_VERSION_V1, VENDOR_ID};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        authorize, read_package_bytes, revalidate_prepared, ArtifactPolicy, ImageMeta, Manifest,
        MemberRef, PackageLimits, PayloadFormat, PreparedUpgrade, RegisteredTarget, TargetRegistry,
        UpgradePolicy,
    };

    const PRODUCT: u32 = 0x1234_5678;
    const HARDWARE: u32 = 0x0002_0001;
    const FIRMWARE_ID: u32 = 0x42;
    const SERIAL: u32 = 7;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Firmware {
        Application,
        Bootloader,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DataFailure {
        None,
        CommitThenLoseAck,
        RejectAndLoseProgress,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Operation {
        Upload(ObjectAddress),
        Download(ObjectAddress, Vec<u8>),
    }

    struct MockState {
        firmware: Firmware,
        vendor_id: u32,
        product_code: u32,
        app_revision: u32,
        serial: u32,
        header: Vec<u8>,
        stream: Vec<u8>,
        offset: usize,
        expected_wire: Vec<u8>,
        operations: Vec<Operation>,
        data_failure: DataFailure,
        fail_next_progress_read: bool,
        lose_start_ack: bool,
        swap_serial_on_claim: bool,
        bootloader_name: String,
        installed_revision: u32,
        reject_header: bool,
        reject_start: bool,
    }

    struct MockSdo {
        state: Mutex<MockState>,
    }

    impl MockSdo {
        fn new(expected_wire: Vec<u8>) -> Self {
            Self {
                state: Mutex::new(MockState {
                    firmware: Firmware::Application,
                    vendor_id: VENDOR_ID,
                    product_code: PRODUCT,
                    app_revision: 0x0001_0000,
                    serial: SERIAL,
                    header: Vec::new(),
                    stream: Vec::new(),
                    offset: 0,
                    expected_wire,
                    operations: Vec::new(),
                    data_failure: DataFailure::None,
                    fail_next_progress_read: false,
                    lose_start_ack: false,
                    swap_serial_on_claim: false,
                    bootloader_name: BL_NAME_STM32G4.to_owned(),
                    installed_revision: 0x0001_0001,
                    reject_header: false,
                    reject_start: false,
                }),
            }
        }

        fn clear_operations(&self) {
            self.state.lock().unwrap().operations.clear();
        }

        fn operations(&self) -> Vec<Operation> {
            self.state.lock().unwrap().operations.clone()
        }

        fn set_data_failure(&self, failure: DataFailure) {
            self.state.lock().unwrap().data_failure = failure;
        }

        fn set_lose_start_ack(&self) {
            self.state.lock().unwrap().lose_start_ack = true;
        }

        fn set_swap_serial_on_claim(&self) {
            self.state.lock().unwrap().swap_serial_on_claim = true;
        }

        fn set_bootloader_name(&self, name: &str) {
            self.state.lock().unwrap().bootloader_name = name.to_owned();
        }

        fn set_installed_revision(&self, revision: u32) {
            self.state.lock().unwrap().installed_revision = revision;
        }

        fn set_vendor_id(&self, vendor_id: u32) {
            self.state.lock().unwrap().vendor_id = vendor_id;
        }

        fn set_serial(&self, serial: u32) {
            self.state.lock().unwrap().serial = serial;
        }

        fn set_definitive_header_rejection(&self, previous_header: Vec<u8>) {
            let mut state = self.state.lock().unwrap();
            state.header = previous_header;
            state.reject_header = true;
        }

        fn set_definitive_start_rejection(&self) {
            self.state.lock().unwrap().reject_start = true;
        }

        fn streamed(&self) -> Vec<u8> {
            self.state.lock().unwrap().stream.clone()
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
            let mut state = self.state.lock().unwrap();
            state.operations.push(Operation::Upload(object));
            if object == OD_FW_BYTES && state.fail_next_progress_read {
                state.fail_next_progress_read = false;
                return Err(TransportError::new("lost authoritative progress"));
            }
            let value = match object {
                ObjectAddress {
                    index: 0x1018,
                    subindex: 0,
                } => vec![4],
                ObjectAddress {
                    index: 0x1018,
                    subindex: 1,
                } => state.vendor_id.to_le_bytes().to_vec(),
                ObjectAddress {
                    index: 0x1018,
                    subindex: 2,
                } => state.product_code.to_le_bytes().to_vec(),
                ObjectAddress {
                    index: 0x1018,
                    subindex: 3,
                } => match state.firmware {
                    Firmware::Application => state.app_revision,
                    Firmware::Bootloader => 0x0000_0100,
                }
                .to_le_bytes()
                .to_vec(),
                ObjectAddress {
                    index: 0x1018,
                    subindex: 4,
                } => state.serial.to_le_bytes().to_vec(),
                ObjectAddress {
                    index: 0x2102,
                    subindex: 0,
                } => HARDWARE.to_le_bytes().to_vec(),
                OD_DEVICE_NAME => match state.firmware {
                    Firmware::Application => b"test-application".to_vec(),
                    Firmware::Bootloader => state.bootloader_name.as_bytes().to_vec(),
                },
                OD_FW_HEADER => state.header.clone(),
                OD_FW_BYTES | OD_FW_OFFSET => (state.offset as u32).to_le_bytes().to_vec(),
                OD_FLASH_STATUS => 0u32.to_le_bytes().to_vec(),
                _ => return Err(TransportError::new(format!("unknown upload {object}"))),
            };
            Ok(value)
        }

        async fn download(
            &self,
            _node_id: u8,
            object: ObjectAddress,
            data: &[u8],
            _timeout: Duration,
        ) -> Result<(), TransportError> {
            let mut state = self.state.lock().unwrap();
            state
                .operations
                .push(Operation::Download(object, data.to_vec()));
            match object {
                OD_PROGRAM_CONTROL if data == [PC_STOP] => {
                    state.firmware = Firmware::Bootloader;
                    if state.swap_serial_on_claim {
                        state.swap_serial_on_claim = false;
                        state.serial = state.serial.wrapping_add(1);
                    }
                    Ok(())
                }
                OD_PROGRAM_CONTROL if data == [PC_CLEAR] => {
                    if state.header.len() != image_container::HEADER_LEN {
                        return Err(TransportError::new("header missing"));
                    }
                    state.stream.clear();
                    state.offset = 0;
                    Ok(())
                }
                OD_PROGRAM_CONTROL if data == [PC_START] => {
                    if state.reject_start {
                        return Err(TransportError::definitive_rejection(
                            "server rejected START",
                        ));
                    }
                    if state.offset != state.expected_wire.len()
                        || state.stream != state.expected_wire
                    {
                        return Err(TransportError::new("image incomplete"));
                    }
                    state.firmware = Firmware::Application;
                    state.app_revision = state.installed_revision;
                    if state.lose_start_ack {
                        state.lose_start_ack = false;
                        Err(TransportError::new("jump lost the start acknowledgement"))
                    } else {
                        Ok(())
                    }
                }
                OD_FW_HEADER => {
                    if state.reject_header {
                        return Err(TransportError::definitive_rejection(
                            "server rejected header",
                        ));
                    }
                    state.header = data.to_vec();
                    Ok(())
                }
                OD_FW_DATA => {
                    let failure = state.data_failure;
                    state.data_failure = DataFailure::None;
                    if failure == DataFailure::RejectAndLoseProgress {
                        state.fail_next_progress_read = true;
                        return Err(TransportError::new("data result ambiguous"));
                    }
                    let start = state.offset;
                    let end = start + data.len();
                    if state.stream.len() < end {
                        state.stream.resize(end, 0xFF);
                    }
                    state.stream[start..end].copy_from_slice(data);
                    state.offset = end;
                    if failure == DataFailure::CommitThenLoseAck {
                        Err(TransportError::new("data ACK lost"))
                    } else {
                        Ok(())
                    }
                }
                OD_FW_OFFSET => {
                    let raw: [u8; 4] = data
                        .try_into()
                        .map_err(|_| TransportError::new("bad offset width"))?;
                    state.offset = u32::from_le_bytes(raw) as usize;
                    Ok(())
                }
                _ => Err(TransportError::new(format!("unknown download {object}"))),
            }
        }
    }

    fn valid_image(len: usize) -> Vec<u8> {
        assert!(len >= 16);
        let mut image = (0..len)
            .map(|index| (index as u8).wrapping_mul(31).wrapping_add(7))
            .collect::<Vec<_>>();
        // G431 application RAM ends at RAM_TOP-32 = 0x2000_7FE0.
        image[0..4].copy_from_slice(&0x2000_7FD8u32.to_le_bytes());
        image[4..8].copy_from_slice(&0x0800_9209u32.to_le_bytes());
        image
    }

    fn padded(image: &[u8]) -> Vec<u8> {
        let mut wire = image.to_vec();
        let remainder = wire.len() % WRITE_GRANULARITY;
        if remainder != 0 {
            wire.resize(wire.len() + WRITE_GRANULARITY - remainder, 0xFF);
        }
        wire
    }

    fn sha256_hex(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn package_bytes(image: &[u8]) -> Vec<u8> {
        let header = HeaderBuilder::new()
            .product_code(PRODUCT)
            .min_hardware_rev(0x0002_0000)
            .firmware_id(FIRMWARE_ID)
            .firmware_version(0x0001_0001)
            .load_address(0x0800_9200)
            .image(image)
            .finish();
        let manifest = Manifest {
            format: crate::FORMAT.to_owned(),
            mcu: crate::MCU_STM32G431.to_owned(),
            vendor_id: VENDOR_ID,
            product_code: PRODUCT,
            min_hardware_rev: 0x0002_0000,
            firmware_id: FIRMWARE_ID,
            firmware_version: 0x0001_0001,
            image: ImageMeta {
                member: "image.bin".to_owned(),
                size: image.len() as u64,
                sha256: sha256_hex(image),
                crc32: Some(image_container::image_crc32_of(image)),
            },
            header: Some(MemberRef {
                member: "header.bin".to_owned(),
            }),
            envelope: None,
            key_fingerprint: None,
            pubkey_fingerprint: None,
            app_arv: None,
            payload_format: Some(PayloadFormat {
                stm32_header_version: Some(FORMAT_VERSION_V1),
                hpm_kn_version: None,
            }),
            tool_version: "core-engine-test".to_owned(),
            built_at: None,
        };
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append(&mut archive, "manifest.json", &manifest);
            append(&mut archive, "image.bin", image);
            append(&mut archive, "header.bin", header.as_bytes());
            archive.finish().unwrap();
        }
        bytes
    }

    fn append<W: Write>(archive: &mut tar::Builder<W>, name: &str, data: &[u8]) {
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, name, data).unwrap();
    }

    fn registry() -> TargetRegistry {
        let policy = UpgradePolicy::new(
            crate::MCU_STM32G431,
            vec![HARDWARE],
            vec![FIRMWARE_ID],
            ArtifactPolicy::UnprotectedV1,
        )
        .unwrap();
        TargetRegistry::new(vec![RegisteredTarget::enabled(
            "engine-test",
            VENDOR_ID,
            PRODUCT,
            policy,
        )
        .unwrap()])
        .unwrap()
    }

    async fn ready(mock: &MockSdo, image: &[u8]) -> ReadyToFlash {
        let registry = registry();
        let target = authorize(mock, 1, &registry, Duration::from_millis(5))
            .await
            .unwrap();
        let package = read_package_bytes(&package_bytes(image), PackageLimits::default()).unwrap();
        let prepared = PreparedUpgrade::bind(target, package).unwrap();
        revalidate_prepared(mock, &prepared, &registry, Duration::from_millis(5))
            .await
            .unwrap()
    }

    fn options() -> FlashOptions {
        FlashOptions {
            chunk_size: 64,
            max_retries: 2,
            operation_timeout: Duration::from_millis(5),
            chunk_timeout: Duration::from_millis(5),
            start_timeout: Duration::from_millis(5),
            bootloader_timeout: Duration::from_millis(20),
            application_timeout: Duration::from_millis(20),
            application_settle_delay: Duration::ZERO,
            poll_interval: Duration::from_millis(1),
            max_authorization_age: Duration::from_secs(1),
        }
    }

    fn downloaded_objects(operations: &[Operation]) -> Vec<ObjectAddress> {
        operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Download(object, _) => Some(*object),
                Operation::Upload(_) => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn happy_path_handles_lost_chunk_and_start_acks_without_duplicate_data() {
        let image = valid_image(513);
        let expected_wire = padded(&image);
        let mock = MockSdo::new(expected_wire.clone());
        mock.set_data_failure(DataFailure::CommitThenLoseAck);
        mock.set_lose_start_ack();
        let ready = ready(&mock, &image).await;
        mock.clear_operations();

        let mut events = Vec::new();
        let outcome = flash(
            &mock,
            ready,
            &options(),
            &CancellationToken::new(),
            |event| events.push(event),
        )
        .await
        .unwrap();

        assert!(!outcome.start_acknowledged);
        assert_eq!(outcome.streamed_bytes, expected_wire.len());
        assert_eq!(outcome.application_identity.revision_number(), 0x0001_0001);
        assert_eq!(mock.streamed(), expected_wire);
        assert!(events.contains(&FlashEvent::Progress {
            written: outcome.streamed_bytes,
            total: outcome.streamed_bytes,
        }));

        let downloads = downloaded_objects(&mock.operations());
        assert_eq!(downloads[0], OD_PROGRAM_CONTROL);
        assert_eq!(downloads[1], OD_PROGRAM_CONTROL);
        assert_eq!(downloads[2], OD_FW_HEADER);
        assert_eq!(downloads[3], OD_PROGRAM_CONTROL);
        assert_eq!(*downloads.last().unwrap(), OD_PROGRAM_CONTROL);
        let data_writes = downloads
            .iter()
            .filter(|object| **object == OD_FW_DATA)
            .count();
        assert_eq!(data_writes, expected_wire.len().div_ceil(64));
    }

    #[tokio::test]
    async fn ready_token_routed_to_unknown_transport_causes_zero_downloads() {
        let image = valid_image(128);
        let authorized_bus = MockSdo::new(padded(&image));
        let ready = ready(&authorized_bus, &image).await;

        let unknown_bus = MockSdo::new(padded(&image));
        unknown_bus.set_vendor_id(0x1122_3344);
        let error = flash(
            &unknown_bus,
            ready,
            &options(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            FlashError::Authorization(AuthorizationError::Unsupported(
                crate::UnsupportedReason::UnknownIdentity { .. }
            ))
        ));
        let operations = unknown_bus.operations();
        assert!(downloaded_objects(&operations).is_empty());
        assert!(!operations
            .iter()
            .any(|operation| matches!(operation, Operation::Upload(object) if *object == ObjectAddress::new(0x2102, 0))));
    }

    #[tokio::test]
    async fn progress_callback_identity_change_is_caught_before_first_download() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();

        let error = flash(
            &mock,
            ready,
            &options(),
            &CancellationToken::new(),
            |event| {
                if event == FlashEvent::Stage(FlashStage::Claiming) {
                    mock.set_serial(SERIAL + 1);
                }
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            FlashError::Authorization(AuthorizationError::IdentityChanged { .. })
        ));
        assert!(downloaded_objects(&mock.operations()).is_empty());
    }

    #[tokio::test]
    async fn definitive_header_rejection_never_uses_stale_readback() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        let previous_header = ready.package().header().to_vec();
        mock.clear_operations();
        mock.set_definitive_header_rejection(previous_header);

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FlashError::Transport {
                operation: "writing the container header",
                ..
            }
        ));
        let downloads = downloaded_objects(&mock.operations());
        assert!(downloads.contains(&OD_FW_HEADER));
        assert!(!mock.operations().iter().any(|operation| {
            matches!(
                operation,
                Operation::Download(address, data)
                    if *address == OD_PROGRAM_CONTROL && data == &[PC_CLEAR]
            )
        }));
    }

    #[tokio::test]
    async fn definitive_start_rejection_is_not_treated_as_lost_ack() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();
        mock.set_definitive_start_rejection();

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FlashError::Transport {
                operation: "starting the application",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn serial_swap_after_claim_aborts_before_header_clear_or_data() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();
        mock.set_swap_serial_on_claim();

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FlashError::Authorization(AuthorizationError::SessionIdentityChanged { .. })
        ));
        assert_eq!(
            downloaded_objects(&mock.operations()),
            vec![OD_PROGRAM_CONTROL]
        );
    }

    #[tokio::test]
    async fn wrong_mcu_bootloader_name_never_reaches_header_or_clear() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();
        mock.set_bootloader_name(BL_NAME_STM32G0B1);

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FlashError::FirmwareWaitTimeout {
                expected: BL_NAME_STM32G4,
                ..
            }
        ));
        assert_eq!(
            downloaded_objects(&mock.operations()),
            vec![OD_PROGRAM_CONTROL]
        );
    }

    #[tokio::test]
    async fn application_revision_must_match_validated_header_version() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();
        mock.set_installed_revision(0x0001_0002);

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FlashError::ApplicationRevisionMismatch {
                expected: 0x0001_0001,
                actual: 0x0001_0002
            }
        ));
    }

    #[tokio::test]
    async fn missing_authoritative_progress_never_blindly_retries_data() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();
        mock.set_data_failure(DataFailure::RejectAndLoseProgress);

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(error, FlashError::AmbiguousChunk { .. }));
        let downloads = downloaded_objects(&mock.operations());
        assert_eq!(
            downloads
                .iter()
                .filter(|object| **object == OD_FW_DATA)
                .count(),
            1
        );
        assert!(!downloads.contains(&OD_FW_OFFSET));
        assert!(!mock.operations().iter().any(|operation| {
            matches!(
                operation,
                Operation::Download(address, data)
                    if *address == OD_PROGRAM_CONTROL && data == &[PC_START]
            )
        }));
    }

    #[tokio::test]
    async fn cancellation_after_progress_stops_before_the_next_chunk_and_start() {
        let image = valid_image(256);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();
        let token = CancellationToken::new();
        let callback_token = token.clone();

        let error = flash(&mock, ready, &options(), &token, |event| {
            if matches!(event, FlashEvent::Progress { written, .. } if written > 0) {
                callback_token.cancel();
            }
        })
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            FlashError::Cancelled {
                stage: FlashStage::Streaming
            }
        ));
        let operations = mock.operations();
        assert_eq!(
            downloaded_objects(&operations)
                .iter()
                .filter(|object| **object == OD_FW_DATA)
                .count(),
            1
        );
        assert!(!operations.iter().any(|operation| {
            matches!(
                operation,
                Operation::Download(address, data)
                    if *address == OD_PROGRAM_CONTROL && data == &[PC_START]
            )
        }));
    }

    #[tokio::test]
    async fn cancellation_from_claiming_callback_happens_before_the_first_write() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();
        let token = CancellationToken::new();
        let callback_token = token.clone();

        let error = flash(&mock, ready, &options(), &token, |event| {
            if event == FlashEvent::Stage(FlashStage::Claiming) {
                callback_token.cancel();
            }
        })
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            FlashError::Cancelled {
                stage: FlashStage::Claiming
            }
        ));
        assert!(downloaded_objects(&mock.operations()).is_empty());
    }
}
