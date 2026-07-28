//! Tauri adapter for the public `hpm-usb-dfu` runtime.
//!
//! Firmware bytes arrive through raw IPC and never grant the WebView a host
//! filesystem path. The long flash call runs on a blocking worker and streams
//! progress through a Tauri channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hpm_usb_dfu::{
    ArtifactKind, DeviceInfo, FlashOutcome, PreparedArtifact, Progress, ProgressStage, SecurityMode,
};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::State;

type CmdResult<T> = Result<T, String>;

#[derive(Default)]
struct DfuInner {
    probed_device: Option<DeviceInfo>,
    staged: Option<StagedArtifact>,
}

struct StagedArtifact {
    token: String,
    prepared: PreparedArtifact,
}

pub struct DfuState {
    inner: Mutex<DfuInner>,
    active: AtomicBool,
    cancel_requested: Arc<AtomicBool>,
}

impl Default for DfuState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(DfuInner::default()),
            active: AtomicBool::new(false),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl DfuState {
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceDto {
    uid: String,
    chip_family_id: u32,
    chip_family_id_hex: String,
    product_code: u32,
    product_code_hex: String,
    product_code_ascii: &'static str,
    hardware_version: u32,
    hardware_version_hex: String,
    hardware_version_valid: bool,
    bootloader_version: String,
    app0_address_hex: String,
    app0_max_size: u32,
    sector_size: u32,
    page_size: u32,
    key_fingerprint_hex: String,
    pubkey_fingerprint_hex: String,
    security_mode: &'static str,
    otp_app_arv_floor: u32,
    otp_app_arv_floor_state: &'static str,
}

impl From<&DeviceInfo> for DeviceDto {
    fn from(info: &DeviceInfo) -> Self {
        Self {
            uid: hex::encode_upper(info.uid),
            chip_family_id: info.chip_family_id,
            chip_family_id_hex: format!("0x{:08X}", info.chip_family_id),
            product_code: info.product_code,
            product_code_hex: format!("0x{:08X}", info.product_code),
            product_code_ascii: "gcan",
            hardware_version: info.hw_version,
            hardware_version_hex: format!("0x{:08X}", info.hw_version),
            hardware_version_valid: info.hw_version_valid,
            bootloader_version: format!("{}.{}", info.bl_version >> 8, info.bl_version & 0xFF),
            app0_address_hex: format!("0x{:08X}", info.app0_addr),
            app0_max_size: info.app0_max_size,
            sector_size: info.sector_size,
            page_size: info.page_size,
            key_fingerprint_hex: format!("0x{:08X}", info.key_fingerprint),
            pubkey_fingerprint_hex: format!("0x{:08X}", info.pubkey_fingerprint),
            security_mode: match info.security {
                SecurityMode::Development => "development",
                SecurityMode::ProductionConfidential => "production_confidential",
            },
            otp_app_arv_floor: info.otp_app_arv_floor,
            otp_app_arv_floor_state: if info.otp_app_arv_floor == u32::MAX {
                "corrupt_informational"
            } else {
                "informational_not_enforced"
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PreparedDto {
    token: String,
    device: DeviceDto,
    artifact_kind: &'static str,
    source_sha256: String,
    wire_image_sha256: String,
    source_size: usize,
    wire_image_size: usize,
    erase_size: usize,
    app_arv: Option<u32>,
    app_arv_state: &'static str,
    pack_tool_version: Option<String>,
}

impl PreparedDto {
    fn new(token: String, prepared: &PreparedArtifact) -> Self {
        let summary = prepared.summary();
        Self {
            token,
            device: DeviceDto::from(prepared.device()),
            artifact_kind: match summary.kind {
                ArtifactKind::DevelopmentRaw => "development_raw",
                ArtifactKind::LegacyHpmOtaV2 => "legacy_hpmota_v2",
            },
            source_sha256: summary.source_sha256_hex.clone(),
            wire_image_sha256: summary.wire_image_sha256_hex.clone(),
            source_size: summary.source_size,
            wire_image_size: summary.wire_image_size,
            erase_size: summary.erase_size,
            app_arv: summary.app_arv,
            app_arv_state: if summary.app_arv.is_some() {
                "metadata_only_not_enforced"
            } else {
                "not_present"
            },
            pack_tool_version: summary.pack_tool_version.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressDto {
    stage: &'static str,
    completed: u64,
    total: u64,
    cancellable: bool,
}

impl From<Progress> for ProgressDto {
    fn from(progress: Progress) -> Self {
        Self {
            stage: match progress.stage {
                ProgressStage::Connecting => "connecting",
                ProgressStage::Revalidating => "revalidating",
                ProgressStage::Erasing => "erasing",
                ProgressStage::Writing => "writing",
                ProgressStage::VerifyingCrc32 => "verifying_crc32",
                ProgressStage::WritingKnData => "writing_kn_data",
                ProgressStage::RequestingJump => "requesting_jump",
            },
            completed: progress.completed,
            total: progress.total,
            cancellable: progress.cancellable,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OutcomeDto {
    status: &'static str,
    startup_confirmed: bool,
    recoverable_bootloader_expected: bool,
}

impl From<FlashOutcome> for OutcomeDto {
    fn from(outcome: FlashOutcome) -> Self {
        match outcome {
            FlashOutcome::JumpAckedStartupUnconfirmed => Self {
                status: "jump_acked_startup_unconfirmed",
                startup_confirmed: false,
                recoverable_bootloader_expected: false,
            },
            FlashOutcome::JumpOutcomeUnknown => Self {
                status: "jump_outcome_unknown",
                startup_confirmed: false,
                recoverable_bootloader_expected: false,
            },
            FlashOutcome::CancelledBeforeErase => Self {
                status: "cancelled_before_erase",
                startup_confirmed: false,
                recoverable_bootloader_expected: true,
            },
            FlashOutcome::CancelledRecoverable => Self {
                status: "cancelled_recoverable",
                startup_confirmed: false,
                recoverable_bootloader_expected: true,
            },
        }
    }
}

#[tauri::command]
pub async fn hpm_dfu_probe(state: State<'_, DfuState>) -> CmdResult<DeviceDto> {
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    {
        let mut inner = state.inner.lock().unwrap();
        inner.probed_device = None;
        inner.staged = None;
    }
    let device = tauri::async_runtime::spawn_blocking(hpm_usb_dfu::probe_connected_device)
        .await
        .map_err(|error| format!("USB probe worker failed: {error}"))?
        .map_err(|error| error.to_string())?;
    let dto = DeviceDto::from(&device);
    state.inner.lock().unwrap().probed_device = Some(device);
    Ok(dto)
}

/// Stage raw IPC bytes against the most recently probed identity.
///
/// This command is synchronous by design: it performs only bounded in-memory
/// parsing/hashing. USB probing is the separate async command above.
#[tauri::command]
pub fn hpm_dfu_prepare(request: Request<'_>, state: State<'_, DfuState>) -> CmdResult<PreparedDto> {
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let bytes = match request.body() {
        InvokeBody::Raw(bytes) => bytes.clone(),
        InvokeBody::Json(_) => {
            return Err("firmware must be sent as a raw Uint8Array IPC body".into())
        }
    };
    let device = {
        let mut inner = state.inner.lock().unwrap();
        inner.staged = None;
        inner
            .probed_device
            .clone()
            .ok_or_else(|| "probe the USB bootloader before selecting firmware".to_string())?
    };
    let prepared = hpm_usb_dfu::prepare_artifact_for_device(bytes, device)
        .map_err(|error| error.to_string())?;
    let token = format!(
        "{:016x}",
        getrandom::u64().map_err(|error| format!("cannot create artifact token: {error}"))?
    );
    let dto = PreparedDto::new(token.clone(), &prepared);
    state.inner.lock().unwrap().staged = Some(StagedArtifact { token, prepared });
    Ok(dto)
}

struct ActiveReset<'a>(&'a AtomicBool);

impl Drop for ActiveReset<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[tauri::command]
pub async fn hpm_dfu_start(
    state: State<'_, DfuState>,
    token: String,
    on_event: Channel<ProgressDto>,
) -> CmdResult<OutcomeDto> {
    // Cancellation takes the same short lock, so a cancel that observes
    // `active=true` cannot be lost behind the per-run flag reset.
    let (prepared, _active_reset) = {
        let inner = state.inner.lock().unwrap();
        state
            .active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "an upgrade is already running".to_string())?;
        let active_reset = ActiveReset(&state.active);
        state.cancel_requested.store(false, Ordering::SeqCst);
        let staged = inner
            .staged
            .as_ref()
            .ok_or_else(|| "no validated artifact is staged".to_string())?;
        if staged.token != token {
            return Err("artifact token is stale; select and validate the file again".into());
        }
        (staged.prepared.clone(), active_reset)
    };

    let cancel_requested = Arc::clone(&state.cancel_requested);
    let result = tauri::async_runtime::spawn_blocking(move || {
        hpm_usb_dfu::flash_connected(&prepared, &cancel_requested, |progress| {
            // Losing the WebView/channel must not abort an in-flight erase or
            // write. The backend continues to a protocol-safe terminal state.
            let _ = on_event.send(ProgressDto::from(progress));
        })
    })
    .await
    .map_err(|error| format!("USB upgrade worker failed: {error}"))?
    .map_err(|error| {
        format!(
            "{error}. If ERASE had started, keep the device in Bootloader mode and run a complete upgrade again"
        )
    })?;
    Ok(OutcomeDto::from(result))
}

#[tauri::command]
pub fn hpm_dfu_cancel(state: State<'_, DfuState>) -> bool {
    let _inner = state.inner.lock().unwrap();
    let active = state.is_active();
    if active {
        state.cancel_requested.store(true, Ordering::SeqCst);
    }
    active
}

#[tauri::command]
pub fn hpm_dfu_leave(state: State<'_, DfuState>) -> CmdResult<()> {
    if state.is_active() {
        return Err("wait for the current protocol command, then cancel before leaving".into());
    }
    state.cancel_requested.store(false, Ordering::SeqCst);
    *state.inner.lock().unwrap() = DfuInner::default();
    Ok(())
}
