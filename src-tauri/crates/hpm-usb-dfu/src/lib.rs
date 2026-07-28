//! Public, secret-free host runtime for the current HPM USB bootloader.
//!
//! This crate deliberately supports one legacy product profile only:
//! USB `34b7:beef` with an exact `GET_INFO.bl_version == 0x0100`.
//! That legacy profile is assigned the host-side synthetic product code
//! `0x6763616e` (ASCII `gcan`). A future bootloader version is not inferred to
//! be compatible: it must get an explicit profile before this crate can erase.
//!
//! The current protected-device artifact is `.hpmota` v2. The format does not
//! authenticate an `encrypted` flag to the public host, so the crate calls it
//! a legacy v2 artifact rather than claiming that encryption was proven. The
//! device remains the final ECDSA verification boundary. Development devices
//! accept only structurally valid plaintext APP0 binaries.

mod artifact;
mod protocol;
mod usb;

use std::sync::atomic::{AtomicBool, Ordering};

pub use artifact::{ArtifactKind, ArtifactSummary, PreparedArtifact};
pub use protocol::{
    parse_chip_info, product_code_for_bl_version, DeviceInfo, SecurityMode, GCAN_PRODUCT_CODE,
    LEGACY_BL_VERSION,
};

use thiserror::Error;

pub const USB_VID: u16 = 0x34B7;
pub const USB_PID: u16 = 0xBEEF;

pub const APP0_ADDRESS: u32 = 0x8000_0000;
pub const APP0_MAX_SIZE: u32 = 0x000C_0000;
pub const FLASH_SECTOR_SIZE: u32 = 4096;
pub const FLASH_PAGE_SIZE: u32 = 256;
pub const WRITE_CHUNK_SIZE: usize = 4096;

pub const PLACEHOLDER_KEY_FINGERPRINT: u32 = 0x190A_55AD;
pub const PLACEHOLDER_PUBKEY_FINGERPRINT: u32 = 0x758D_6336;

#[derive(Debug, Error)]
pub enum DfuError {
    #[error(
        "HPM USB bootloader {vid:04X}:{pid:04X} was not found; enter Bootloader mode and reconnect USB"
    )]
    DeviceNotFound { vid: u16, pid: u16 },

    #[error(
        "found {count} matching HPM USB bootloaders; leave exactly one connected before upgrading"
    )]
    MultipleDevices { count: usize },

    #[error("unknown bootloader version 0x{0:04X}; refusing to treat it as gs_can")]
    UnknownBootloader(u16),

    #[error("device is not the supported legacy gs_can profile: {0}")]
    InvalidDevice(String),

    #[error("invalid firmware artifact: {0}")]
    InvalidArtifact(String),

    #[error("USB error: {0}")]
    Usb(String),

    #[error("bootloader protocol error: {0}")]
    Protocol(String),

    #[error("device rejected command 0x{cmd:02X} with status {status} ({name})")]
    DeviceStatus {
        cmd: u8,
        status: u16,
        name: &'static str,
    },

    #[error("the connected device changed after preparation; prepare the artifact again")]
    DeviceChanged,

    #[error("device CRC32 mismatch after writing: expected 0x{expected:08X}, got 0x{actual:08X}")]
    VerifyMismatch { expected: u32, actual: u32 },
}

pub type Result<T> = std::result::Result<T, DfuError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressStage {
    Connecting,
    Revalidating,
    Erasing,
    Writing,
    VerifyingCrc32,
    WritingKnData,
    RequestingJump,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub stage: ProgressStage,
    pub completed: u64,
    pub total: u64,
    /// Cancellation is cooperative. `false` means a command is currently in
    /// flight and the host must wait for its ACK before stopping.
    pub cancellable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashOutcome {
    /// JUMP_APP returned a matching cmd/seq OK response. The USB protocol
    /// still cannot prove that the application subsequently became healthy.
    JumpAckedStartupUnconfirmed,
    /// The JUMP_APP OUT transfer completed, but its response was lost or
    /// malformed. The application may or may not have started.
    JumpOutcomeUnknown,
    /// Cancellation arrived before ERASE was sent, so flash was untouched.
    CancelledBeforeErase,
    /// Cancellation was honored after at least one destructive command ACK.
    /// The device is expected to remain recoverable in the Bootloader.
    CancelledRecoverable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpDisposition {
    Acked,
    OutcomeUnknown,
}

/// Narrow protocol surface used by the flash state machine. It is public so
/// other open-source frontends can reuse the exact ordering and test it with a
/// mock transport; callers cannot construct a `PreparedArtifact` without
/// passing the artifact/device validator.
pub trait BootloaderTransport {
    fn get_info(&mut self) -> Result<DeviceInfo>;
    fn erase(&mut self, address: u32, size: u32) -> Result<()>;
    fn write(&mut self, address: u32, data: &[u8]) -> Result<()>;
    fn verify(&mut self, address: u32, size: u32, expected_crc32: u32) -> Result<u32>;
    fn write_kn_data(&mut self, blob: &[u8; 128]) -> Result<()>;
    fn jump_app(&mut self) -> Result<JumpDisposition>;
}

/// Probe exactly one connected, explicitly supported HPM USB device.
pub fn probe_connected_device() -> Result<DeviceInfo> {
    let mut transport = usb::UsbBootloader::open_unique()?;
    transport.get_info()
}

/// Parse and authorize a user-selected artifact against the currently
/// connected device. This does no erase or write.
pub fn prepare_connected_artifact(bytes: Vec<u8>) -> Result<PreparedArtifact> {
    let device = probe_connected_device()?;
    artifact::prepare_for_device(bytes, device)
}

/// Validator entry point for downloaded bytes and tests. Passing a `DeviceInfo`
/// does not weaken authorization: the identity itself is rechecked here, and
/// `flash_connected` re-reads it from the same USB handle immediately before
/// ERASE.
pub fn prepare_artifact_for_device(bytes: Vec<u8>, device: DeviceInfo) -> Result<PreparedArtifact> {
    protocol::validate_known_device(&device)?;
    artifact::prepare_for_device(bytes, device)
}

/// Open the single USB bootloader, revalidate the exact prepared identity, and
/// run the bounded stop-and-wait flash sequence.
pub fn flash_connected<F>(
    prepared: &PreparedArtifact,
    cancel: &AtomicBool,
    mut progress: F,
) -> Result<FlashOutcome>
where
    F: FnMut(Progress),
{
    progress(Progress {
        stage: ProgressStage::Connecting,
        completed: 0,
        total: 1,
        cancellable: true,
    });
    let mut transport = usb::UsbBootloader::open_unique()?;
    execute_flash(&mut transport, prepared, cancel, progress)
}

/// Execute the shared flash state machine on an already opened transport.
///
/// Destructive requests are never automatically retried. Cancellation is
/// sampled before ERASE and between acknowledged protocol commands.
pub fn execute_flash<T, F>(
    transport: &mut T,
    prepared: &PreparedArtifact,
    cancel: &AtomicBool,
    mut progress: F,
) -> Result<FlashOutcome>
where
    T: BootloaderTransport,
    F: FnMut(Progress),
{
    progress(Progress {
        stage: ProgressStage::Revalidating,
        completed: 0,
        total: 1,
        cancellable: true,
    });
    let current = transport.get_info()?;
    protocol::validate_known_device(&current)?;
    if current != *prepared.device() {
        return Err(DfuError::DeviceChanged);
    }

    if cancel.load(Ordering::SeqCst) {
        return Ok(FlashOutcome::CancelledBeforeErase);
    }

    let sector = current.sector_size as usize;
    let erase_len = prepared
        .image()
        .len()
        .checked_add(sector - 1)
        .map(|n| n / sector * sector)
        .ok_or_else(|| DfuError::InvalidArtifact("wire image size overflow".into()))?;
    if erase_len == 0 || erase_len > current.app0_max_size as usize {
        return Err(DfuError::InvalidArtifact(format!(
            "wire image rounds to {erase_len} bytes, outside APP0 capacity {}",
            current.app0_max_size
        )));
    }

    let mut padded = Vec::with_capacity(erase_len);
    padded.extend_from_slice(prepared.image());
    padded.resize(erase_len, 0xFF);

    progress(Progress {
        stage: ProgressStage::Erasing,
        completed: 0,
        total: erase_len as u64,
        cancellable: false,
    });
    transport.erase(current.app0_addr, erase_len as u32)?;
    progress(Progress {
        stage: ProgressStage::Erasing,
        completed: erase_len as u64,
        total: erase_len as u64,
        cancellable: true,
    });

    if cancel.load(Ordering::SeqCst) {
        return Ok(FlashOutcome::CancelledRecoverable);
    }

    for (index, chunk) in padded.chunks(WRITE_CHUNK_SIZE).enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Ok(FlashOutcome::CancelledRecoverable);
        }
        let offset = index * WRITE_CHUNK_SIZE;
        progress(Progress {
            stage: ProgressStage::Writing,
            completed: offset as u64,
            total: padded.len() as u64,
            cancellable: false,
        });
        transport.write(current.app0_addr + offset as u32, chunk)?;
        progress(Progress {
            stage: ProgressStage::Writing,
            completed: (offset + chunk.len()) as u64,
            total: padded.len() as u64,
            cancellable: true,
        });
    }

    if cancel.load(Ordering::SeqCst) {
        return Ok(FlashOutcome::CancelledRecoverable);
    }

    let expected_crc32 = crc32fast::hash(&padded);
    progress(Progress {
        stage: ProgressStage::VerifyingCrc32,
        completed: 0,
        total: padded.len() as u64,
        cancellable: false,
    });
    let actual_crc32 = transport.verify(current.app0_addr, padded.len() as u32, expected_crc32)?;
    if actual_crc32 != expected_crc32 {
        return Err(DfuError::VerifyMismatch {
            expected: expected_crc32,
            actual: actual_crc32,
        });
    }
    progress(Progress {
        stage: ProgressStage::VerifyingCrc32,
        completed: padded.len() as u64,
        total: padded.len() as u64,
        cancellable: true,
    });

    if cancel.load(Ordering::SeqCst) {
        return Ok(FlashOutcome::CancelledRecoverable);
    }

    if let Some(kn_data) = prepared.kn_data() {
        progress(Progress {
            stage: ProgressStage::WritingKnData,
            completed: 0,
            total: kn_data.len() as u64,
            cancellable: false,
        });
        transport.write_kn_data(kn_data)?;
        progress(Progress {
            stage: ProgressStage::WritingKnData,
            completed: kn_data.len() as u64,
            total: kn_data.len() as u64,
            cancellable: true,
        });

        if cancel.load(Ordering::SeqCst) {
            return Ok(FlashOutcome::CancelledRecoverable);
        }
    }

    progress(Progress {
        stage: ProgressStage::RequestingJump,
        completed: 0,
        total: 1,
        cancellable: false,
    });
    match transport.jump_app()? {
        JumpDisposition::Acked => Ok(FlashOutcome::JumpAckedStartupUnconfirmed),
        JumpDisposition::OutcomeUnknown => Ok(FlashOutcome::JumpOutcomeUnknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct MockTransport {
        info: Option<DeviceInfo>,
        log: Vec<&'static str>,
        verify_delta: u32,
    }

    impl BootloaderTransport for MockTransport {
        fn get_info(&mut self) -> Result<DeviceInfo> {
            self.log.push("info");
            Ok(self.info.clone().unwrap())
        }

        fn erase(&mut self, _address: u32, _size: u32) -> Result<()> {
            self.log.push("erase");
            Ok(())
        }

        fn write(&mut self, _address: u32, _data: &[u8]) -> Result<()> {
            self.log.push("write");
            Ok(())
        }

        fn verify(&mut self, _address: u32, _size: u32, expected_crc32: u32) -> Result<u32> {
            self.log.push("verify");
            Ok(expected_crc32.wrapping_add(self.verify_delta))
        }

        fn write_kn_data(&mut self, _blob: &[u8; 128]) -> Result<()> {
            self.log.push("kn");
            Ok(())
        }

        fn jump_app(&mut self) -> Result<JumpDisposition> {
            self.log.push("jump");
            Ok(JumpDisposition::Acked)
        }
    }

    fn dev_device() -> DeviceInfo {
        DeviceInfo {
            uid: [0x5A; 16],
            chip_family_id: 0x5300,
            product_code: GCAN_PRODUCT_CODE,
            hw_version: 0,
            hw_version_valid: false,
            bl_version: LEGACY_BL_VERSION,
            app0_addr: APP0_ADDRESS,
            app0_max_size: APP0_MAX_SIZE,
            sector_size: FLASH_SECTOR_SIZE,
            page_size: FLASH_PAGE_SIZE,
            key_fingerprint: PLACEHOLDER_KEY_FINGERPRINT,
            pubkey_fingerprint: PLACEHOLDER_PUBKEY_FINGERPRINT,
            security: SecurityMode::Development,
            otp_app_arv_floor: 0,
        }
    }

    fn raw_app() -> Vec<u8> {
        let mut image = vec![0xFF; 0x3400];
        image[0..4].copy_from_slice(&0xFCF9_0002u32.to_le_bytes());
        image
    }

    #[test]
    fn flash_order_is_erase_write_verify_jump_for_dev() {
        let device = dev_device();
        let prepared = prepare_artifact_for_device(raw_app(), device.clone()).unwrap();
        let mut mock = MockTransport {
            info: Some(device),
            ..Default::default()
        };
        let outcome = execute_flash(&mut mock, &prepared, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(outcome, FlashOutcome::JumpAckedStartupUnconfirmed);
        assert_eq!(mock.log.first(), Some(&"info"));
        assert_eq!(mock.log.get(1), Some(&"erase"));
        assert!(mock.log.contains(&"write"));
        assert_eq!(mock.log[mock.log.len() - 2..], ["verify", "jump"]);
        assert!(!mock.log.contains(&"kn"));
    }

    #[test]
    fn cancellation_at_write_boundary_never_reaches_verify_or_jump() {
        let device = dev_device();
        let prepared = prepare_artifact_for_device(raw_app(), device.clone()).unwrap();
        let mut mock = MockTransport {
            info: Some(device),
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let outcome = execute_flash(&mut mock, &prepared, &cancel, |event| {
            if event.stage == ProgressStage::Writing
                && event.completed >= WRITE_CHUNK_SIZE as u64
                && event.cancellable
            {
                cancel.store(true, Ordering::SeqCst);
            }
        })
        .unwrap();
        assert_eq!(outcome, FlashOutcome::CancelledRecoverable);
        assert_eq!(mock.log, ["info", "erase", "write"]);
    }

    #[test]
    fn crc_failure_never_writes_kn_or_jumps() {
        let device = dev_device();
        let prepared = prepare_artifact_for_device(raw_app(), device.clone()).unwrap();
        let mut mock = MockTransport {
            info: Some(device),
            verify_delta: 1,
            ..Default::default()
        };
        assert!(matches!(
            execute_flash(&mut mock, &prepared, &AtomicBool::new(false), |_| {}),
            Err(DfuError::VerifyMismatch { .. })
        ));
        assert!(mock.log.ends_with(&["verify"]));
        assert!(!mock.log.contains(&"kn"));
        assert!(!mock.log.contains(&"jump"));
    }
}
