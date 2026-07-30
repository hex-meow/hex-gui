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
use crate::package::Stm32ImageMode;
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
    /// Plaintext-v1 chunk size. Encrypted v2 always uses the authenticated
    /// record boundaries declared by the validated header.
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

/// Flash a prepared STM32 v1 or authenticated encrypted-v2 artifact.
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

    progress(FlashEvent::Stage(FlashStage::Streaming));
    let streamed_bytes = match ready.package().image_mode() {
        Stm32ImageMode::PlaintextV1 => {
            let mut wire = ready.package().image().to_vec();
            let remainder = wire.len() % WRITE_GRANULARITY;
            if remainder != 0 {
                wire.resize(wire.len() + WRITE_GRANULARITY - remainder, 0xFF);
            }
            progress(FlashEvent::Progress {
                written: 0,
                total: wire.len(),
            });
            stream_v1(sdo, node_id, &wire, options, cancellation, &mut progress).await?;
            wire.len()
        }
        Stm32ImageMode::EncryptedV2 => {
            let header = image_container::Header::parse(ready.package().header())
                .map_err(|_| FlashError::InvalidReadyTarget)?;
            let wire = ready.package().image();
            progress(FlashEvent::Progress {
                written: 0,
                total: wire.len(),
            });
            stream_encrypted_v2(
                sdo,
                node_id,
                &header,
                wire,
                options,
                cancellation,
                &mut progress,
            )
            .await?;
            wire.len()
        }
        Stm32ImageMode::SignedV2 => return Err(FlashError::InvalidReadyTarget),
    };

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
        streamed_bytes,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EncryptedRecord {
    wire_offset: usize,
    wire_len: usize,
}

fn encrypted_record_layout(
    header: &image_container::Header,
    wire_len: usize,
) -> Result<Vec<EncryptedRecord>, FlashError> {
    if header.format_version() != image_container::FORMAT_VERSION_V2
        || !header.flag_encrypted()
        || header.record_plain_size() != image_container::V2_RECORD_PLAIN_SIZE
        || header.record_tag_size() != image_container::V2_RECORD_TAG_SIZE
        || header.wire_size() as usize != wire_len
    {
        return Err(FlashError::InvalidEncryptedGeometry);
    }
    let padded_plaintext = image_container::padded_plain_size(header.image_size())
        .ok_or(FlashError::InvalidEncryptedGeometry)? as usize;
    if padded_plaintext == 0 {
        return Err(FlashError::InvalidEncryptedGeometry);
    }

    let max_plain = usize::from(image_container::V2_RECORD_PLAIN_SIZE);
    let tag_len = usize::from(image_container::V2_RECORD_TAG_SIZE);
    let mut plain_offset = 0usize;
    let mut wire_offset = 0usize;
    let mut records = Vec::new();
    while plain_offset < padded_plaintext {
        let plain_len = (padded_plaintext - plain_offset).min(max_plain);
        let wire_len = plain_len
            .checked_add(tag_len)
            .ok_or(FlashError::InvalidEncryptedGeometry)?;
        let next_wire = wire_offset
            .checked_add(wire_len)
            .ok_or(FlashError::InvalidEncryptedGeometry)?;
        records.push(EncryptedRecord {
            wire_offset,
            wire_len,
        });
        plain_offset += plain_len;
        wire_offset = next_wire;
    }
    if wire_offset != header.wire_size() as usize {
        return Err(FlashError::InvalidEncryptedGeometry);
    }
    Ok(records)
}

/// Stream opaque secure-v2 bytes as complete `ciphertext || tag` records.
///
/// The host never decrypts a record. Exact record framing is nevertheless a
/// protocol requirement because the Bootloader authenticates and commits one
/// record per SDO download. After an ambiguous result, only the attempted
/// record's start or end is a valid authoritative wire offset.
async fn stream_encrypted_v2(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    header: &image_container::Header,
    wire: &[u8],
    options: &FlashOptions,
    cancellation: &CancellationToken,
    progress: &mut impl FnMut(FlashEvent),
) -> Result<(), FlashError> {
    let records = encrypted_record_layout(header, wire.len())?;
    let mut record_pos = 0usize;
    let mut retries_left = options.max_retries;

    while record_pos < records.len() {
        check_cancel(cancellation, FlashStage::Streaming)?;
        let record = records[record_pos];
        let end = record.wire_offset + record.wire_len;
        let data = &wire[record.wire_offset..end];
        match sdo
            .download(node_id, OD_FW_DATA, data, options.chunk_timeout)
            .await
        {
            Ok(()) => {
                record_pos += 1;
                progress(FlashEvent::Progress {
                    written: end,
                    total: wire.len(),
                });
            }
            Err(source) if source.is_definitive_rejection() => {
                return Err(FlashError::Transport {
                    operation: "streaming encrypted firmware record",
                    source,
                })
            }
            Err(source) => {
                check_cancel(cancellation, FlashStage::Streaming)?;
                let authoritative =
                    read_exact_u32(sdo, node_id, OD_FW_BYTES, options.operation_timeout)
                        .await
                        .map_err(|progress_error| FlashError::AmbiguousChunk {
                            offset: record.wire_offset,
                            write_error: source.to_string(),
                            progress_error: progress_error.to_string(),
                        })? as usize;
                if authoritative == end {
                    // The whole authenticated record committed; only its SDO
                    // acknowledgement was lost.
                    record_pos += 1;
                    progress(FlashEvent::Progress {
                        written: end,
                        total: wire.len(),
                    });
                    continue;
                }
                if authoritative != record.wire_offset {
                    return Err(FlashError::InvalidEncryptedAuthoritativeOffset {
                        offset: authoritative,
                        attempted_start: record.wire_offset,
                        attempted_end: end,
                    });
                }
                if retries_left == 0 {
                    return Err(FlashError::RetriesExhausted {
                        offset: record.wire_offset,
                        authoritative_offset: authoritative,
                    });
                }
                retries_left -= 1;
                // Secure Bootloaders accept only this idempotent echo of the
                // counter they just reported; rewinding/skipping is forbidden.
                write_offset_verified(sdo, node_id, authoritative, options.operation_timeout)
                    .await?;
                progress(FlashEvent::Resynchronized {
                    attempted_offset: record.wire_offset,
                    authoritative_offset: authoritative,
                    retries_left,
                });
            }
        }
    }

    let final_offset = read_exact_u32(sdo, node_id, OD_FW_BYTES, options.operation_timeout)
        .await
        .map_err(|source| FlashError::Transport {
            operation: "reading final encrypted-wire byte count",
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
    let policy = match ready.target().target().support() {
        crate::SupportPolicy::Enabled(policy) => policy,
        crate::SupportPolicy::Disabled { .. } => return Err(FlashError::InvalidReadyTarget),
    };
    let expected_names = policy
        .firmware_policy(ready.package().manifest().firmware_id)
        .ok_or(FlashError::InvalidReadyTarget)?
        .application_names();
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
            Ok(name) => {
                if !expected_names.iter().any(|expected| expected == &name) {
                    return Err(FlashError::ApplicationNameMismatch {
                        expected: expected_names.to_vec(),
                        actual: name,
                    });
                }
                match confirm_same_device_across_firmware(
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
                            return Err(FlashError::ApplicationRevisionMismatch {
                                expected,
                                actual,
                            });
                        }
                        return Ok(identity);
                    }
                    Err(error) => (error.to_string(), None),
                }
            }
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
    #[error("secure-v2 package has invalid encrypted record geometry")]
    InvalidEncryptedGeometry,
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
    #[error(
        "device reported encrypted-wire offset {offset}, but only attempted record boundaries {attempted_start} or {attempted_end} are valid"
    )]
    InvalidEncryptedAuthoritativeOffset {
        offset: usize,
        attempted_start: usize,
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
    #[error("application name mismatch after start: expected one of {expected:?}, got {actual:?}")]
    ApplicationNameMismatch {
        expected: Vec<String>,
        actual: String,
    },
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use image_container::{
        HeaderBuilder, HeaderV2Builder, FORMAT_VERSION_V1, FORMAT_VERSION_V2, TARGET_MCU_G0B1,
        V2_ENCRYPTION_KEY_ID, V2_SIGNING_KEY_ID, VENDOR_ID,
    };
    use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey, VerifyingKey};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        authorize, read_package_bytes, revalidate_prepared, ArtifactPolicy, FirmwarePolicy,
        ImageMeta, Manifest, MemberRef, PackageLimits, PayloadFormat, PreparedUpgrade,
        RegisteredTarget, TargetRegistry, UpgradePolicy,
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
        RejectWithoutCommit,
        RejectAndLoseProgress,
        ReportWrongOffset(usize),
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
        application_name: String,
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
                    application_name: "test-application".to_owned(),
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

        fn set_application_name(&self, name: &str) {
            self.state.lock().unwrap().application_name = name.to_owned();
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
                    Firmware::Application => state.application_name.as_bytes().to_vec(),
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
                    if failure == DataFailure::RejectWithoutCommit {
                        return Err(TransportError::new("record was not committed"));
                    }
                    if let DataFailure::ReportWrongOffset(offset) = failure {
                        state.offset = offset;
                        return Err(TransportError::new("device reported a wrong offset"));
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

    fn signing_key(fill: u8) -> SigningKey {
        SigningKey::from_slice(&[fill; 32]).unwrap()
    }

    fn raw_verifying_key(signing_key: &SigningKey) -> [u8; 64] {
        let encoded = VerifyingKey::from(signing_key).to_encoded_point(false);
        let bytes = encoded.as_bytes();
        assert_eq!(bytes[0], 0x04);
        let mut raw = [0u8; 64];
        raw.copy_from_slice(&bytes[1..]);
        raw
    }

    fn secure_plaintext() -> Vec<u8> {
        (0..481)
            .map(|index| (index as u8).wrapping_mul(17).wrapping_add(3))
            .collect()
    }

    fn secure_unsigned_header(
        encrypted: bool,
        signing_key_id: u32,
        encryption_key_id: u32,
    ) -> image_container::Header {
        let plaintext = secure_plaintext();
        let builder = if encrypted {
            HeaderV2Builder::encrypted([0xA5; 12]).encryption_key_id(encryption_key_id)
        } else {
            HeaderV2Builder::signed_only()
        };
        builder
            .product_code(PRODUCT)
            .min_hardware_rev(0x0002_0000)
            .firmware_id(FIRMWARE_ID)
            .firmware_version(0x0001_0001)
            .load_address(0x0800_AA00)
            .target_mcu(TARGET_MCU_G0B1)
            .security_epoch(0)
            .signing_key_id(signing_key_id)
            .plaintext(&plaintext)
            .finish()
            .unwrap()
    }

    fn signature_raw(unsigned: &image_container::Header, signing_key: &SigningKey) -> [u8; 64] {
        let digest = unsigned.signature_digest().unwrap();
        let signature: Signature = signing_key.sign_prehash(&digest).unwrap();
        let signature = signature.normalize_s().unwrap_or(signature);
        let mut raw = [0u8; 64];
        raw.copy_from_slice(&signature.to_bytes());
        raw
    }

    fn sign_header(
        unsigned: image_container::Header,
        signing_key: &SigningKey,
    ) -> image_container::Header {
        let signature = signature_raw(&unsigned, signing_key);
        unsigned.with_signature(signature).unwrap()
    }

    fn high_s_equivalent(mut signature: [u8; 64]) -> [u8; 64] {
        const P256_ORDER: [u8; 32] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xBC, 0xE6, 0xFA, 0xAD, 0xA7, 0x17, 0x9E, 0x84, 0xF3, 0xB9, 0xCA, 0xC2,
            0xFC, 0x63, 0x25, 0x51,
        ];
        let mut high_s = [0u8; 32];
        let mut borrow = 0i16;
        for index in (0..32).rev() {
            let difference =
                i16::from(P256_ORDER[index]) - i16::from(signature[32 + index]) - borrow;
            if difference < 0 {
                high_s[index] = (difference + 256) as u8;
                borrow = 1;
            } else {
                high_s[index] = difference as u8;
                borrow = 0;
            }
        }
        assert_eq!(borrow, 0);
        signature[32..].copy_from_slice(&high_s);
        signature
    }

    fn secure_package_bytes(
        signing_key: &SigningKey,
        manifest_key: &[u8; 64],
        encrypted: bool,
        signature_override: Option<[u8; 64]>,
        signing_key_id: u32,
        encryption_key_id: u32,
    ) -> (Vec<u8>, Vec<u8>) {
        let plaintext = secure_plaintext();
        let unsigned = secure_unsigned_header(encrypted, signing_key_id, encryption_key_id);
        let header = match signature_override {
            Some(signature) => unsigned.with_signature(signature).unwrap(),
            None => sign_header(unsigned, signing_key),
        };
        let wire = if encrypted {
            (0..header.wire_size() as usize)
                .map(|index| (index as u8).wrapping_mul(29).wrapping_add(0x51))
                .collect::<Vec<_>>()
        } else {
            let mut wire = plaintext;
            wire.resize(header.wire_size() as usize, 0xFF);
            wire
        };
        let manifest = Manifest {
            format: crate::FORMAT.to_owned(),
            mcu: crate::MCU_STM32G0B1.to_owned(),
            vendor_id: VENDOR_ID,
            product_code: PRODUCT,
            min_hardware_rev: 0x0002_0000,
            firmware_id: FIRMWARE_ID,
            firmware_version: 0x0001_0001,
            image: ImageMeta {
                member: "image.bin".to_owned(),
                size: wire.len() as u64,
                sha256: sha256_hex(&wire),
                crc32: Some(image_container::image_crc32_of(&wire)),
            },
            header: Some(MemberRef {
                member: "header.bin".to_owned(),
            }),
            envelope: None,
            key_fingerprint: None,
            pubkey_fingerprint: Some(sha256_hex(manifest_key)),
            app_arv: None,
            payload_format: Some(PayloadFormat {
                stm32_header_version: Some(FORMAT_VERSION_V2),
                hpm_kn_version: None,
            }),
            tool_version: "core-secure-engine-test".to_owned(),
            built_at: None,
        };
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append(&mut archive, "manifest.json", &manifest);
            append(&mut archive, "image.bin", &wire);
            append(&mut archive, "header.bin", header.as_bytes());
            archive.finish().unwrap();
        }
        (bytes, wire)
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
            vec![FirmwarePolicy::new(FIRMWARE_ID, vec!["test-application".to_owned()]).unwrap()],
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

    fn secure_registry_with_ids(
        verifying_key: [u8; 64],
        signing_key_id: u32,
        encryption_key_id: u32,
        security_epoch: u32,
    ) -> TargetRegistry {
        let policy = UpgradePolicy::new(
            crate::MCU_STM32G0B1,
            vec![HARDWARE],
            vec![FirmwarePolicy::new(FIRMWARE_ID, vec!["test-application".to_owned()]).unwrap()],
            ArtifactPolicy::encrypted_v2(
                verifying_key,
                signing_key_id,
                encryption_key_id,
                security_epoch,
            ),
        )
        .unwrap();
        TargetRegistry::new(vec![RegisteredTarget::enabled(
            "secure-engine-test",
            VENDOR_ID,
            PRODUCT,
            policy,
        )
        .unwrap()])
        .unwrap()
    }

    fn secure_registry(verifying_key: [u8; 64]) -> TargetRegistry {
        secure_registry_with_ids(verifying_key, V2_SIGNING_KEY_ID, V2_ENCRYPTION_KEY_ID, 0)
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

    async fn secure_ready(
        mock: &MockSdo,
        package_bytes: &[u8],
        verifying_key: [u8; 64],
    ) -> ReadyToFlash {
        let registry = secure_registry(verifying_key);
        let target = authorize(mock, 1, &registry, Duration::from_millis(5))
            .await
            .unwrap();
        let package = read_package_bytes(package_bytes, PackageLimits::default()).unwrap();
        let prepared = PreparedUpgrade::bind(target, package).unwrap();
        revalidate_prepared(mock, &prepared, &registry, Duration::from_millis(5))
            .await
            .unwrap()
    }

    async fn bind_secure(
        mock: &MockSdo,
        package_bytes: &[u8],
        verifying_key: [u8; 64],
    ) -> Result<PreparedUpgrade, crate::ProfileError> {
        let registry = secure_registry(verifying_key);
        let target = authorize(mock, 1, &registry, Duration::from_millis(5))
            .await
            .unwrap();
        let package = read_package_bytes(package_bytes, PackageLimits::default()).unwrap();
        PreparedUpgrade::bind(target, package)
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
    async fn encrypted_v2_binding_authenticates_header_and_exposes_exact_sizes() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (package_bytes, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        let prepared = bind_secure(&mock, &package_bytes, verifying_key)
            .await
            .unwrap();

        assert_eq!(prepared.package().image_mode(), Stm32ImageMode::EncryptedV2);
        assert_eq!(prepared.package().plaintext_size(), 481);
        assert_eq!(prepared.package().wire_size(), 536);
    }

    #[tokio::test]
    async fn encrypted_v2_binding_rejects_malformed_signature() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (package_bytes, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            Some([0u8; 64]),
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        let error = bind_secure(&mock, &package_bytes, verifying_key)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::ProfileError::MalformedHeaderSignature
        ));
    }

    #[tokio::test]
    async fn encrypted_v2_binding_rejects_malleable_high_s_signature() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let unsigned = secure_unsigned_header(true, V2_SIGNING_KEY_ID, V2_ENCRYPTION_KEY_ID);
        let high_s = high_s_equivalent(signature_raw(&unsigned, &signing));
        let (package_bytes, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            Some(high_s),
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        let error = bind_secure(&mock, &package_bytes, verifying_key)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::ProfileError::NonCanonicalHeaderSignature
        ));
    }

    #[tokio::test]
    async fn encrypted_v2_binding_rejects_wrong_public_key_even_if_manifest_claims_it() {
        let signer = signing_key(1);
        let wrong_signer = signing_key(2);
        let wrong_key = raw_verifying_key(&wrong_signer);
        let (package_bytes, wire) = secure_package_bytes(
            &signer,
            &wrong_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        let error = bind_secure(&mock, &package_bytes, wrong_key)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::ProfileError::HeaderSignatureVerificationFailed
        ));
    }

    #[tokio::test]
    async fn encrypted_v2_binding_rejects_manifest_public_key_fingerprint_mismatch() {
        let signer = signing_key(1);
        let verifying_key = raw_verifying_key(&signer);
        let other_key = raw_verifying_key(&signing_key(2));
        let (package_bytes, wire) = secure_package_bytes(
            &signer,
            &other_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        let error = bind_secure(&mock, &package_bytes, verifying_key)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::ProfileError::PublicKeyFingerprintMismatch
        ));
    }

    #[tokio::test]
    async fn encrypted_v2_policy_rejects_signed_only_package() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (package_bytes, wire) =
            secure_package_bytes(&signing, &verifying_key, false, None, V2_SIGNING_KEY_ID, 0);
        let mock = MockSdo::new(wire);
        let error = bind_secure(&mock, &package_bytes, verifying_key)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::ProfileError::ArtifactModeMismatch {
                actual: Stm32ImageMode::SignedV2,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn encrypted_v2_binding_requires_bootloader_key_ids() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (wrong_signing_id, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            None,
            V2_SIGNING_KEY_ID + 1,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        let error = bind_secure(&mock, &wrong_signing_id, verifying_key)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::ProfileError::SigningKeyIdMismatch {
                expected: 1,
                actual: 2
            }
        ));

        let (wrong_encryption_id, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID + 1,
        );
        let mock = MockSdo::new(wire);
        let error = bind_secure(&mock, &wrong_encryption_id, verifying_key)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::ProfileError::EncryptionKeyIdMismatch {
                expected: 1,
                actual: 2
            }
        ));
    }

    #[tokio::test]
    async fn encrypted_v2_binding_requires_profile_security_epoch() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (package_bytes, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        let registry =
            secure_registry_with_ids(verifying_key, V2_SIGNING_KEY_ID, V2_ENCRYPTION_KEY_ID, 1);
        let target = authorize(&mock, 1, &registry, Duration::from_millis(5))
            .await
            .unwrap();
        let package = read_package_bytes(&package_bytes, PackageLimits::default()).unwrap();
        let error = PreparedUpgrade::bind(target, package).unwrap_err();
        assert!(matches!(
            error,
            crate::ProfileError::SecurityEpochMismatch {
                expected: 1,
                actual: 0
            }
        ));
    }

    #[tokio::test]
    async fn encrypted_v2_streams_exact_records_without_tail_padding_or_duplicate_ack_retry() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (package_bytes, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire.clone());
        mock.set_bootloader_name(BL_NAME_STM32G0B1);
        mock.set_data_failure(DataFailure::CommitThenLoseAck);
        let ready = secure_ready(&mock, &package_bytes, verifying_key).await;
        mock.clear_operations();

        let outcome = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap();

        let writes = mock
            .operations()
            .into_iter()
            .filter_map(|operation| match operation {
                Operation::Download(address, data) if address == OD_FW_DATA => Some(data),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            writes.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![256, 256, 24]
        );
        assert_eq!(writes.concat(), wire);
        assert_eq!(mock.streamed(), wire);
        assert_eq!(outcome.streamed_bytes, 536);
    }

    #[tokio::test]
    async fn encrypted_v2_resyncs_only_at_the_reported_record_start() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (package_bytes, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        mock.set_bootloader_name(BL_NAME_STM32G0B1);
        mock.set_data_failure(DataFailure::RejectWithoutCommit);
        let ready = secure_ready(&mock, &package_bytes, verifying_key).await;
        mock.clear_operations();
        let mut events = Vec::new();

        flash(
            &mock,
            ready,
            &options(),
            &CancellationToken::new(),
            |event| events.push(event),
        )
        .await
        .unwrap();

        assert!(events.contains(&FlashEvent::Resynchronized {
            attempted_offset: 0,
            authoritative_offset: 0,
            retries_left: 1,
        }));
        let operations = mock.operations();
        let data_writes = operations
            .iter()
            .filter(|operation| {
                matches!(operation, Operation::Download(address, _) if *address == OD_FW_DATA)
            })
            .count();
        assert_eq!(data_writes, 4);
        assert!(operations.iter().any(|operation| {
            matches!(
                operation,
                Operation::Download(address, data)
                    if *address == OD_FW_OFFSET && data == &0u32.to_le_bytes()
            )
        }));
    }

    #[tokio::test]
    async fn encrypted_v2_rejects_authoritative_offset_outside_attempted_record() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (package_bytes, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        mock.set_bootloader_name(BL_NAME_STM32G0B1);
        mock.set_data_failure(DataFailure::ReportWrongOffset(512));
        let ready = secure_ready(&mock, &package_bytes, verifying_key).await;
        mock.clear_operations();

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FlashError::InvalidEncryptedAuthoritativeOffset {
                offset: 512,
                attempted_start: 0,
                attempted_end: 256
            }
        ));
        assert!(!mock.operations().iter().any(|operation| {
            matches!(operation, Operation::Download(address, _) if *address == OD_FW_OFFSET)
        }));
    }

    #[tokio::test]
    async fn encrypted_v2_never_blindly_retries_without_authoritative_offset() {
        let signing = signing_key(1);
        let verifying_key = raw_verifying_key(&signing);
        let (package_bytes, wire) = secure_package_bytes(
            &signing,
            &verifying_key,
            true,
            None,
            V2_SIGNING_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        let mock = MockSdo::new(wire);
        mock.set_bootloader_name(BL_NAME_STM32G0B1);
        mock.set_data_failure(DataFailure::RejectAndLoseProgress);
        let ready = secure_ready(&mock, &package_bytes, verifying_key).await;
        mock.clear_operations();

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FlashError::AmbiguousChunk { offset: 0, .. }
        ));
        let operations = mock.operations();
        assert_eq!(
            operations
                .iter()
                .filter(|operation| {
                    matches!(operation, Operation::Download(address, _) if *address == OD_FW_DATA)
                })
                .count(),
            1
        );
        assert!(!operations.iter().any(|operation| {
            matches!(operation, Operation::Download(address, _) if *address == OD_FW_OFFSET)
        }));
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
    async fn application_name_must_match_the_firmware_id_policy() {
        let image = valid_image(128);
        let mock = MockSdo::new(padded(&image));
        let ready = ready(&mock, &image).await;
        mock.clear_operations();
        mock.set_application_name("different-firmware");

        let error = flash(&mock, ready, &options(), &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FlashError::ApplicationNameMismatch {
                ref expected,
                ref actual
            } if expected == &["test-application".to_owned()] && actual == "different-firmware"
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
