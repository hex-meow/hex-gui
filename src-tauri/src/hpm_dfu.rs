//! Tauri adapter for the public `hpm-usb-dfu` runtime.
//!
//! Firmware bytes arrive through raw IPC and never grant the WebView a host
//! filesystem path. The long flash call runs on a blocking worker and streams
//! progress through a Tauri channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hpm_usb_dfu::{
    ArtifactKind, DeviceInfo, FlashOutcome, PreparedArtifact, Progress, ProgressStage,
    SecurityMode, GCAN_PRODUCT_CODE, LEGACY_BL_VERSION,
};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::State;

use crate::dfu_gate::{DfuBackend, DfuMutationGate};
use crate::dfu_release::{download_latest, ReleaseSpec};

type CmdResult<T> = Result<T, String>;

const HEXMEOW_VENDOR_ID: u32 = 0x6865_786D;
const HPM_PROFILE_ID: &str = "gs-can-hpm-usb-v1";
const MAX_HPM_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;
const ONLINE_KEY_FINGERPRINT: u32 = 0xBCDC_8DB9;
const ONLINE_PUBKEY_FINGERPRINT: u32 = 0x7065_D71D;

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
    session_lock: tokio::sync::Mutex<()>,
    active: AtomicBool,
    cancel_requested: Arc<AtomicBool>,
}

impl Default for DfuState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(DfuInner::default()),
            session_lock: tokio::sync::Mutex::new(()),
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
    artifact_source: &'static str,
    release_version: Option<String>,
}

impl PreparedDto {
    fn new(
        token: String,
        prepared: &PreparedArtifact,
        artifact_source: &'static str,
        release_version: Option<String>,
    ) -> Self {
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
            artifact_source,
            release_version,
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
    let _session = state.session_lock.lock().await;
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
/// This command performs only bounded in-memory parsing/hashing. It is async
/// solely so the backend session lock can serialize it with probe/download.
#[tauri::command]
pub async fn hpm_dfu_prepare(
    request: Request<'_>,
    state: State<'_, DfuState>,
) -> CmdResult<PreparedDto> {
    let _session = state.session_lock.lock().await;
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
    let token = artifact_token()?;
    let dto = PreparedDto::new(token.clone(), &prepared, "local", None);
    state.inner.lock().unwrap().staged = Some(StagedArtifact { token, prepared });
    Ok(dto)
}

/// Download and stage the stable secure release for the already authorized
/// legacy USB product. No URL, identity, or security mode is accepted from the
/// WebView; an unknown or development device is rejected before HTTP begins.
#[tauri::command]
pub async fn hpm_dfu_prepare_latest(state: State<'_, DfuState>) -> CmdResult<PreparedDto> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let device = {
        let mut inner = state.inner.lock().unwrap();
        inner.staged = None;
        inner
            .probed_device
            .clone()
            .ok_or_else(|| "probe the USB bootloader before downloading firmware".to_string())?
    };
    let release_spec = online_release_spec(&device)?;
    let artifact = download_latest(release_spec).await?;

    // The network wait is long enough for a physical USB swap. Re-open the
    // unique bootloader and repeat GET_INFO before the downloaded bytes can
    // become a staged capability. The flash engine repeats this once more on
    // its mutation handle immediately before ERASE.
    let current_device = tauri::async_runtime::spawn_blocking(hpm_usb_dfu::probe_connected_device)
        .await
        .map_err(|error| format!("USB re-probe worker failed: {error}"))?
        .map_err(|error| error.to_string())?;
    if current_device != device {
        return Err(
            "the connected USB bootloader changed while downloading firmware; scan it again".into(),
        );
    }

    // Remote discovery selects bytes only. The exact same native validator as
    // local selection still binds the package to this device and its key
    // fingerprints before a staged capability can exist. HPM v2 has no public
    // native version binding, so `artifact.version` remains a release label.
    let prepared = hpm_usb_dfu::prepare_artifact_for_device(artifact.bytes, current_device.clone())
        .map_err(|error| error.to_string())?;
    if prepared.summary().kind != ArtifactKind::LegacyHpmOtaV2 {
        return Err("the secure online release is not a legacy .hpmota v2 artifact".into());
    }
    if prepared.summary().source_size != artifact.size
        || prepared.summary().source_sha256_hex != artifact.sha256
    {
        return Err("validated HPM artifact no longer matches its release binding".into());
    }

    let token = artifact_token()?;
    let dto = PreparedDto::new(token.clone(), &prepared, "r2", Some(artifact.version));
    let mut inner = state.inner.lock().unwrap();
    if inner.probed_device.as_ref() != Some(&current_device) {
        return Err("the probed USB device changed while downloading firmware".into());
    }
    inner.staged = Some(StagedArtifact { token, prepared });
    Ok(dto)
}

fn online_release_spec(device: &DeviceInfo) -> CmdResult<ReleaseSpec<'static>> {
    if device.bl_version != LEGACY_BL_VERSION || device.product_code != GCAN_PRODUCT_CODE {
        return Err(format!(
            "USB Bootloader 0x{:04X} is not the exact online gs_can release profile",
            device.bl_version
        ));
    }
    if device.security != SecurityMode::ProductionConfidential {
        return Err(
            "development USB devices use a local plaintext .bin; no online secure release was requested"
                .into(),
        );
    }
    if device.key_fingerprint != ONLINE_KEY_FINGERPRINT
        || device.pubkey_fingerprint != ONLINE_PUBKEY_FINGERPRINT
    {
        return Err(
            "the protected USB device key domain is not authorized for this online release; no R2 request was made"
                .into(),
        );
    }
    Ok(ReleaseSpec {
        profile_id: HPM_PROFILE_ID,
        vendor_id: HEXMEOW_VENDOR_ID,
        product_code: GCAN_PRODUCT_CODE,
        artifact_extension: "hpmota",
        max_artifact_bytes: MAX_HPM_ARTIFACT_BYTES,
    })
}

fn artifact_token() -> CmdResult<String> {
    Ok(format!(
        "{:016x}",
        getrandom::u64().map_err(|error| format!("cannot create artifact token: {error}"))?
    ))
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
    mutation_gate: State<'_, DfuMutationGate>,
    token: String,
    on_event: Channel<ProgressDto>,
) -> CmdResult<OutcomeDto> {
    let session = state.session_lock.lock().await;
    let _mutation_permit = mutation_gate
        .try_acquire(DfuBackend::HpmUsb)
        .map_err(str::to_owned)?;
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
    // Keep probe/prepare/start atomic, but do not make leave or probe wait for
    // the whole blocking flash. They should acquire this lock immediately,
    // observe `active`, and return the normal in-progress error.
    drop(session);

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
pub async fn hpm_dfu_leave(state: State<'_, DfuState>) -> CmdResult<()> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("wait for the current protocol command, then cancel before leaving".into());
    }
    state.cancel_requested.store(false, Ordering::SeqCst);
    *state.inner.lock().unwrap() = DfuInner::default();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hpm_usb_dfu::{
        APP0_ADDRESS, APP0_MAX_SIZE, FLASH_PAGE_SIZE, FLASH_SECTOR_SIZE,
        PLACEHOLDER_KEY_FINGERPRINT, PLACEHOLDER_PUBKEY_FINGERPRINT,
    };

    fn device(security: SecurityMode) -> DeviceInfo {
        DeviceInfo {
            uid: [0x42; 16],
            chip_family_id: 0x5300,
            product_code: GCAN_PRODUCT_CODE,
            hw_version: 0,
            hw_version_valid: false,
            bl_version: LEGACY_BL_VERSION,
            app0_addr: APP0_ADDRESS,
            app0_max_size: APP0_MAX_SIZE,
            sector_size: FLASH_SECTOR_SIZE,
            page_size: FLASH_PAGE_SIZE,
            key_fingerprint: if security == SecurityMode::Development {
                PLACEHOLDER_KEY_FINGERPRINT
            } else {
                ONLINE_KEY_FINGERPRINT
            },
            pubkey_fingerprint: if security == SecurityMode::Development {
                PLACEHOLDER_PUBKEY_FINGERPRINT
            } else {
                ONLINE_PUBKEY_FINGERPRINT
            },
            security,
            otp_app_arv_floor: 0,
        }
    }

    #[test]
    fn exact_secure_legacy_profile_is_the_only_online_route() {
        let spec = online_release_spec(&device(SecurityMode::ProductionConfidential)).unwrap();
        assert_eq!(spec.profile_id, HPM_PROFILE_ID);
        assert_eq!(spec.vendor_id, HEXMEOW_VENDOR_ID);
        assert_eq!(spec.product_code, GCAN_PRODUCT_CODE);
        assert_eq!(spec.artifact_extension, "hpmota");

        assert!(online_release_spec(&device(SecurityMode::Development)).is_err());

        let mut future = device(SecurityMode::ProductionConfidential);
        future.bl_version = LEGACY_BL_VERSION + 1;
        assert!(online_release_spec(&future).is_err());

        let mut wrong_product = device(SecurityMode::ProductionConfidential);
        wrong_product.product_code ^= 1;
        assert!(online_release_spec(&wrong_product).is_err());

        let mut wrong_key_domain = device(SecurityMode::ProductionConfidential);
        wrong_key_domain.key_fingerprint ^= 1;
        assert!(online_release_spec(&wrong_key_domain).is_err());

        let mut wrong_signing_domain = device(SecurityMode::ProductionConfidential);
        wrong_signing_domain.pubkey_fingerprint ^= 1;
        assert!(online_release_spec(&wrong_signing_domain).is_err());
    }
}
