//! HPM USB legacy/V3 and Classic-CAN V3 IPC adapter. Raw bytes only; native
//! validation and a token bind each write to the captured transport/identity.
use crate::can_lease::{CanOwner, CanTransportGate};
use crate::dfu_gate::{DfuBackend, DfuMutationGate};
use crate::dfu_release::{download_latest, ReleaseSpec};
use hpm_usb_dfu::{
    v3, ArtifactKind, DeviceInfo, FlashOutcome, PreparedArtifact, Progress, ProgressStage,
    SecurityMode, GCAN_PRODUCT_CODE, LEGACY_BL_VERSION,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tauri::{
    ipc::{Channel, InvokeBody, Request},
    State,
};
type CmdResult<T> = Result<T, String>;
const HEXMEOW_VENDOR_ID: u32 = 0x6865_786D;
const HPM_PROFILE_ID: &str = "gs-can-hpm-usb-v1";
const MAX_HPM_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;
const ONLINE_KEY_FINGERPRINT: u32 = 0xBCDC_8DB9;
const ONLINE_PUBKEY_FINGERPRINT: u32 = 0x7065_D71D;

#[derive(Clone)]
enum Identity {
    Legacy(DeviceInfo),
    V3(v3::DeviceInfo),
}
enum Artifact {
    Legacy(PreparedArtifact),
    V3(v3::Artifact),
}
struct StagedArtifact {
    token: String,
    prepared: Artifact,
}
#[derive(Default)]
struct DfuInner {
    probed_device: Option<Identity>,
    staged: Option<StagedArtifact>,
    session: Option<Arc<Mutex<v3::Session>>>,
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
struct ActiveReset<'a>(&'a AtomicBool);
impl Drop for ActiveReset<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
fn activate(s: &DfuState) -> CmdResult<ActiveReset<'_>> {
    let _inner = s.inner.lock().unwrap();
    s.active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .map_err(|_| "an HPM operation is already running")?;
    s.cancel_requested.store(false, Ordering::SeqCst);
    Ok(ActiveReset(&s.active))
}
async fn clear(s: &DfuState) -> CmdResult<()> {
    let previous = std::mem::take(&mut *s.inner.lock().unwrap()).session;
    if let Some(previous) = previous {
        tauri::async_runtime::spawn_blocking(move || previous.lock().unwrap().close())
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
fn retain(s: &DfuState, session: v3::Session) -> DeviceDto {
    let dto = DeviceDto::v3(&session.info, session.node_id());
    let identity = Identity::V3(session.info.clone());
    let session = Arc::new(Mutex::new(session));
    let mut inner = s.inner.lock().unwrap();
    inner.probed_device = Some(identity);
    inner.session = Some(session);
    dto
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceDto {
    uid: String,
    chip_family_id: u32,
    chip_family_id_hex: String,
    product_code: u32,
    product_code_hex: String,
    product_code_ascii: String,
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
    protocol_version: u8,
    transport: &'static str,
    node_id: Option<u8>,
    capabilities: u32,
}
impl From<&DeviceInfo> for DeviceDto {
    fn from(i: &DeviceInfo) -> Self {
        Self {
            uid: hex::encode_upper(i.uid),
            chip_family_id: i.chip_family_id,
            chip_family_id_hex: format!("0x{:08X}", i.chip_family_id),
            product_code: i.product_code,
            product_code_hex: format!("0x{:08X}", i.product_code),
            product_code_ascii: "gcan".into(),
            hardware_version: i.hw_version,
            hardware_version_hex: format!("0x{:08X}", i.hw_version),
            hardware_version_valid: i.hw_version_valid,
            bootloader_version: format!("{}.{}", i.bl_version >> 8, i.bl_version & 255),
            app0_address_hex: format!("0x{:08X}", i.app0_addr),
            app0_max_size: i.app0_max_size,
            sector_size: i.sector_size,
            page_size: i.page_size,
            key_fingerprint_hex: format!("0x{:08X}", i.key_fingerprint),
            pubkey_fingerprint_hex: format!("0x{:08X}", i.pubkey_fingerprint),
            security_mode: match i.security {
                SecurityMode::Development => "development",
                SecurityMode::ProductionConfidential => "production_confidential",
            },
            otp_app_arv_floor: i.otp_app_arv_floor,
            otp_app_arv_floor_state: if i.otp_app_arv_floor == u32::MAX {
                "corrupt_informational"
            } else {
                "informational_not_enforced"
            },
            protocol_version: 2,
            transport: "usb",
            node_id: None,
            capabilities: 0,
        }
    }
}
impl DeviceDto {
    fn v3(i: &v3::DeviceInfo, node: Option<u8>) -> Self {
        let product_code_ascii = i
            .product
            .to_be_bytes()
            .iter()
            .copied()
            .filter(|b| b.is_ascii_graphic())
            .map(char::from)
            .collect();
        Self {
            uid: hex::encode_upper(i.uid),
            chip_family_id: i.chip_family,
            chip_family_id_hex: format!("0x{:08X}", i.chip_family),
            product_code: i.product,
            product_code_hex: format!("0x{:08X}", i.product),
            product_code_ascii,
            hardware_version: i.hardware,
            hardware_version_hex: format!("0x{:08X}", i.hardware),
            hardware_version_valid: i.hardware_valid,
            bootloader_version: "2.0".into(),
            app0_address_hex: "0x80000000".into(),
            app0_max_size: hpm_usb_dfu::APP0_MAX_SIZE,
            sector_size: 4096,
            page_size: 256,
            key_fingerprint_hex: "—".into(),
            pubkey_fingerprint_hex: format!("0x{:08X}", i.pubkey_fingerprint),
            security_mode: if i.encrypted {
                "v3_encrypted_unsupported"
            } else {
                "signed_plaintext"
            },
            otp_app_arv_floor: i.floor,
            otp_app_arv_floor_state: if i.floor > 32 {
                "corrupt_enforced"
            } else {
                "enforced"
            },
            protocol_version: 3,
            transport: if node.is_some() { "can" } else { "usb" },
            node_id: node,
            capabilities: i.capabilities,
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
        p: &PreparedArtifact,
        source: &'static str,
        release: Option<String>,
    ) -> Self {
        let s = p.summary();
        Self {
            token,
            device: DeviceDto::from(p.device()),
            artifact_kind: match s.kind {
                ArtifactKind::DevelopmentRaw => "development_raw",
                ArtifactKind::LegacyHpmOtaV2 => "legacy_hpmota_v2",
            },
            source_sha256: s.source_sha256_hex.clone(),
            wire_image_sha256: s.wire_image_sha256_hex.clone(),
            source_size: s.source_size,
            wire_image_size: s.wire_image_size,
            erase_size: s.erase_size,
            app_arv: s.app_arv,
            app_arv_state: if s.app_arv.is_some() {
                "metadata_only_not_enforced"
            } else {
                "not_present"
            },
            pack_tool_version: s.pack_tool_version.clone(),
            artifact_source: source,
            release_version: release,
        }
    }
    fn v3(token: String, a: &v3::Artifact, node: Option<u8>) -> Self {
        Self {
            token,
            device: DeviceDto::v3(a.device(), node),
            artifact_kind: "signed_hpmota_v3",
            source_sha256: a.source_sha256.clone(),
            wire_image_sha256: hex::encode(Sha256::digest(a.image())),
            source_size: a.source_size,
            wire_image_size: a.image().len(),
            erase_size: a.image().len(),
            app_arv: Some(a.arv),
            app_arv_state: "signature_verified_enforced",
            pack_tool_version: None,
            artifact_source: "local",
            release_version: None,
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
    fn from(p: Progress) -> Self {
        Self {
            stage: match p.stage {
                ProgressStage::Connecting => "connecting",
                ProgressStage::Revalidating => "revalidating",
                ProgressStage::Erasing => "erasing",
                ProgressStage::Writing => "writing",
                ProgressStage::VerifyingCrc32 => "verifying_crc32",
                ProgressStage::WritingKnData => "writing_kn_data",
                ProgressStage::RequestingJump => "requesting_jump",
            },
            completed: p.completed,
            total: p.total,
            cancellable: p.cancellable,
        }
    }
}
impl From<v3::Progress> for ProgressDto {
    fn from(p: v3::Progress) -> Self {
        Self {
            stage: match p.stage {
                v3::Stage::Capturing => "capturing",
                v3::Stage::Revalidating => "revalidating",
                v3::Stage::Preparing => "preparing",
                v3::Stage::Writing => "writing",
                v3::Stage::Committing => "committing",
                v3::Stage::RequestingJump => "requesting_jump",
            },
            completed: p.completed,
            total: p.total,
            cancellable: p.cancellable,
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
    fn from(o: FlashOutcome) -> Self {
        let (status, recoverable) = match o {
            FlashOutcome::JumpAckedStartupUnconfirmed => ("jump_acked_startup_unconfirmed", false),
            FlashOutcome::JumpOutcomeUnknown => ("jump_outcome_unknown", false),
            FlashOutcome::CancelledBeforeErase => ("cancelled_before_erase", true),
            FlashOutcome::CancelledRecoverable => ("cancelled_recoverable", true),
        };
        Self {
            status,
            startup_confirmed: false,
            recoverable_bootloader_expected: recoverable,
        }
    }
}

#[tauri::command]
pub async fn hpm_dfu_probe(
    state: State<'_, DfuState>,
    mutation_gate: State<'_, DfuMutationGate>,
) -> CmdResult<DeviceDto> {
    let _lock = state.session_lock.lock().await;
    let _active = activate(&state)?;
    let _permit = mutation_gate
        .try_acquire(DfuBackend::HpmUsb)
        .map_err(str::to_owned)?;
    clear(&state).await?;
    let cancel = state.cancel_requested.clone();
    let found = tauri::async_runtime::spawn_blocking(move || {
        v3::capture_usb(&cancel, Duration::from_secs(30))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    match found {
        v3::UsbCapture::Legacy(info) => {
            let dto = DeviceDto::from(&info);
            state.inner.lock().unwrap().probed_device = Some(Identity::Legacy(info));
            Ok(dto)
        }
        v3::UsbCapture::V3(session) => Ok(retain(&state, session)),
    }
}
#[tauri::command]
pub async fn hpm_dfu_capture_can(
    state: State<'_, DfuState>,
    recovery: State<'_, crate::can_recovery::RecoveryState>,
    recovery_token: Option<String>,
    mutation_gate: State<'_, DfuMutationGate>,
    can_gate: State<'_, CanTransportGate>,
    spec: String,
    node_id: Option<u8>,
    assign_node_id: Option<u8>,
    vendor: u32,
    product: u32,
    uid: Option<String>,
) -> CmdResult<DeviceDto> {
    crate::can_recovery::require_hpm(&recovery, recovery_token.as_deref(), vendor, product)?;
    for node in [node_id, assign_node_id].into_iter().flatten() {
        crate::can_node_policy::check(node)?;
    }
    let _lock = state.session_lock.lock().await;
    let _active = activate(&state)?;
    let _permit = mutation_gate
        .try_acquire(DfuBackend::HpmCan)
        .map_err(str::to_owned)?;
    clear(&state).await?;
    let uid = uid
        .map(|s| {
            hex::decode(s.trim())
                .map_err(|_| "UID must be 16 bytes of hex")
                .and_then(|v| v.try_into().map_err(|_| "UID must be 16 bytes of hex"))
        })
        .transpose()?;
    let lease = can_gate.try_acquire(CanOwner::DfuUpdate)?;
    let bus = crate::backend::open_classic_1m_bus(spec.trim(), lease)
        .await
        .map_err(|e| e.to_string())?;
    let session = crate::hpm_v3_can::capture(
        bus,
        node_id,
        assign_node_id,
        vendor,
        product,
        uid,
        state.cancel_requested.clone(),
        None,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(retain(&state, session))
}
#[tauri::command]
pub async fn hpm_dfu_capture_selected_can(
    state: State<'_, DfuState>,
    can_state: State<'_, crate::stm32_can_dfu::CanDfuState>,
    mutation_gate: State<'_, DfuMutationGate>,
    can_gate: State<'_, CanTransportGate>,
) -> CmdResult<DeviceDto> {
    let (spec, node, identity) = crate::stm32_can_dfu::selected_hpm_context(&can_state)?;
    let _lock = state.session_lock.lock().await;
    let _active = activate(&state)?;
    let _permit = mutation_gate
        .try_acquire(DfuBackend::HpmCan)
        .map_err(str::to_owned)?;
    clear(&state).await?;
    let lease = can_gate.try_acquire(CanOwner::DfuUpdate)?;
    let bus = crate::backend::open_classic_1m_bus(&spec, lease)
        .await
        .map_err(|e| e.to_string())?;
    let session = crate::hpm_v3_can::capture(
        bus,
        Some(node),
        None,
        0,
        0,
        None,
        state.cancel_requested.clone(),
        Some(identity),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(retain(&state, session))
}
#[tauri::command]
pub async fn hpm_dfu_prepare(
    request: Request<'_>,
    state: State<'_, DfuState>,
) -> CmdResult<PreparedDto> {
    let _lock = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let bytes = match request.body() {
        InvokeBody::Raw(b) if b.len() <= MAX_HPM_ARTIFACT_BYTES => b.clone(),
        _ => return Err("firmware must be a raw Uint8Array, at most 2 MiB".into()),
    };
    let (identity, node) = {
        let mut i = state.inner.lock().unwrap();
        i.staged = None;
        (
            i.probed_device
                .clone()
                .ok_or("capture/probe the bootloader first")?,
            i.session.as_ref().and_then(|s| s.lock().ok()?.node_id()),
        )
    };
    let token = artifact_token()?;
    let (prepared, dto) = match identity {
        Identity::Legacy(info) => {
            let p =
                hpm_usb_dfu::prepare_artifact_for_device(bytes, info).map_err(|e| e.to_string())?;
            let dto = PreparedDto::new(token.clone(), &p, "local", None);
            (Artifact::Legacy(p), dto)
        }
        Identity::V3(info) => {
            let p = v3::Artifact::parse(bytes, info).map_err(|e| e.to_string())?;
            let dto = PreparedDto::v3(token.clone(), &p, node);
            (Artifact::V3(p), dto)
        }
    };
    state.inner.lock().unwrap().staged = Some(StagedArtifact { token, prepared });
    Ok(dto)
}
#[tauri::command]
pub async fn hpm_dfu_prepare_latest(state: State<'_, DfuState>) -> CmdResult<PreparedDto> {
    let _lock = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let device = {
        let mut i = state.inner.lock().unwrap();
        i.staged = None;
        match i.probed_device.clone(){Some(Identity::Legacy(d))=>d,_=>return Err("online legacy gs_can releases are unavailable for this identity; V3 uses a local signed .hpmota/3".into())}
    };
    let spec = online_release_spec(&device)?;
    let artifact = download_latest(spec).await?;
    let current = tauri::async_runtime::spawn_blocking(hpm_usb_dfu::probe_connected_device)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if current != device {
        return Err("USB device changed during download".into());
    }
    let prepared = hpm_usb_dfu::prepare_artifact_for_device(artifact.bytes, current)
        .map_err(|e| e.to_string())?;
    if prepared.summary().kind != ArtifactKind::LegacyHpmOtaV2
        || prepared.summary().source_size != artifact.size
        || prepared.summary().source_sha256_hex != artifact.sha256
    {
        return Err("online legacy artifact binding mismatch".into());
    }
    let token = artifact_token()?;
    let dto = PreparedDto::new(token.clone(), &prepared, "r2", Some(artifact.version));
    state.inner.lock().unwrap().staged = Some(StagedArtifact {
        token,
        prepared: Artifact::Legacy(prepared),
    });
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
    getrandom::u64()
        .map(|v| format!("{v:016x}"))
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn hpm_dfu_start(
    state: State<'_, DfuState>,
    mutation_gate: State<'_, DfuMutationGate>,
    token: String,
    on_event: Channel<ProgressDto>,
) -> CmdResult<OutcomeDto> {
    let lock = state.session_lock.lock().await;
    let _active = activate(&state)?;
    let (prepared, client, backend) = {
        let mut i = state.inner.lock().unwrap();
        if i.staged.as_ref().is_none_or(|s| s.token != token) {
            return Err("artifact token is stale; validate the file again".into());
        }
        let client = i.session.clone();
        let backend = if client
            .as_ref()
            .is_some_and(|s| s.lock().unwrap().node_id().is_some())
        {
            DfuBackend::HpmCan
        } else {
            DfuBackend::HpmUsb
        };
        (i.staged.take().unwrap().prepared, client, backend)
    };
    let _permit = mutation_gate.try_acquire(backend).map_err(str::to_owned)?;
    drop(lock);
    let cancel = state.cancel_requested.clone();
    let result = tauri::async_runtime::spawn_blocking(move || match prepared {
        Artifact::Legacy(p) => hpm_usb_dfu::flash_connected(&p, &cancel, |p| {
            let _ = on_event.send(ProgressDto::from(p));
        }),
        Artifact::V3(p) => {
            let client = client
                .ok_or_else(|| hpm_usb_dfu::DfuError::Protocol("capture session ended".into()))?;
            let mut s = client.lock().unwrap();
            let result = s.flash(&p, &cancel, |p| {
                let _ = on_event.send(ProgressDto::from(p));
            });
            s.close();
            result
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    {
        let mut inner = state.inner.lock().unwrap();
        inner.session = None;
        inner.staged = None;
        inner.probed_device = None;
    }
    result.map(OutcomeDto::from).map_err(|e| {
        format!(
            "{e}. If writing started, capture the bootloader and perform a complete update again."
        )
    })
}
#[tauri::command]
pub fn hpm_dfu_cancel(state: State<'_, DfuState>) -> bool {
    let _i = state.inner.lock().unwrap();
    let active = state.is_active();
    if active {
        state.cancel_requested.store(true, Ordering::SeqCst);
    }
    active
}
#[tauri::command]
pub async fn hpm_dfu_leave(state: State<'_, DfuState>) -> CmdResult<()> {
    let _lock = state.session_lock.lock().await;
    if state.is_active() {
        return Err("wait for the current operation and cancel before leaving".into());
    }
    clear(&state).await
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
