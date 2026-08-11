//! Tauri adapter for identity-routed CAN firmware-update backends.
//!
//! Discovery is shared and read-only: heartbeat candidates must return a
//! complete 0x1018 record before an exact local registry selects a backend.
//! Standard STM32 `.meowpkg` and compatible encrypted IMG files retain
//! independent parsers, policy tables, capability types, and flash engines.
//! Unknown, incomplete, disabled, or changed identities never receive a
//! protocol-specific update frame.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use async_trait::async_trait;
use can_transport::{
    CanBus, CanBusState, CanCapabilities, CanFilter, CanFrame, CanId, CanIoError, CanLinkConfig,
    CanRx,
};
use cobs_can_iap::{
    flash as flash_cobs_iap, CancellationToken as CobsCancellationToken,
    CanopenIdentity as CobsCanopenIdentity, FlashError as CobsFlashError,
    FlashEvent as CobsFlashEvent, FlashOptions as CobsFlashOptions, FlashStage as CobsFlashStage,
    ImgArtifact, ImgLimits, PreparedUpgrade as CobsPreparedUpgrade,
    SupportPolicy as CobsSupportPolicy, TargetClassification as CobsTargetClassification,
    TargetRegistry as CobsTargetRegistry,
};
use hexmeow_dfu_targets::{target_by_profile_id, PreflightPolicy, TargetBackend};
use hexmeow_stm32_can_dfu::{
    authorize, flash, observe_identity, read_package_bytes, revalidate_prepared, AuthorizedTarget,
    CanBusSdo, CancellationToken, FlashError, FlashEvent, FlashOptions, FlashStage,
    IdentitySnapshot, PackageLimits, PreparedUpgrade, Stm32ImageMode, SupportPolicy,
    TargetClassification, TargetRegistry,
};
use motor_img_release::{
    identity_root as motor_identity_root, parse_latest_json as parse_motor_latest_json,
    parse_release_json as parse_motor_release_json, verify_latest as verify_motor_latest,
    verify_release as verify_motor_release, LatestDocument as MotorLatestDocument,
    ReleaseDocument as MotorReleaseDocument,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::{Manager, State};

use crate::can_lease::{CanOwner, CanTransportGate};
use crate::cobs_can_iap_profiles::{
    display_name_for_profile as cobs_display_name_for_profile,
    target_registry as cobs_target_registry,
};
use crate::dfu_gate::{DfuBackend, DfuMutationGate};
use crate::motor_factory_backup::{
    persist_backup, read_stable_snapshot, BackupArtifact, BackupContext, BackupIdentity,
    PersistedBackup,
};
use crate::stm32_can_profiles::{display_name_for_profile, target_registry};

type CmdResult<T> = std::result::Result<T, String>;

const DISCOVERY_WINDOW: Duration = Duration::from_millis(2_500);
const IDENTITY_TIMEOUT: Duration = Duration::from_millis(750);
const HEARTBEAT_BASE: u16 = 0x700;
const HEARTBEAT_MASK: u16 = 0x780;
const MAX_DISCOVERY_NODES: usize = 32;
const MAX_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;
const MAX_LATEST_BYTES: usize = 16 * 1024;
const MAX_RELEASE_BYTES: usize = 64 * 1024;
const R2_ORIGIN: &str = "https://downloads.hexmeow.com";
const LATEST_FORMAT: &str = "hexmeow-dfu-latest/1";
const RELEASE_FORMAT: &str = "hexmeow-dfu-release/1";
const R2_CHANNEL: &str = "stable";
const R2_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const R2_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const TSDO_BASE: u32 = 0x580;
const TSDO_FAMILY_MASK: u32 = 0x780;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LatestDocument {
    format: String,
    profile_id: String,
    vendor_id: String,
    product_code: String,
    channel: String,
    version: String,
    release: LatestReleaseRef,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LatestReleaseRef {
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseDocument {
    format: String,
    profile_id: String,
    vendor_id: String,
    product_code: String,
    version: String,
    tag: String,
    candidate_source_commit: String,
    artifact: ReleaseArtifactRef,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseArtifactRef {
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug)]
struct ValidatedLatest {
    version: String,
    firmware_version: u32,
    release_path: String,
    release_sha256: String,
    release_bytes: usize,
}

#[derive(Debug)]
struct ValidatedRelease {
    artifact_path: String,
    artifact_sha256: String,
    artifact_bytes: usize,
}

#[derive(Default)]
struct CanDfuInner {
    spec: Option<String>,
    discovered: HashMap<u8, DiscoveredTarget>,
    selected: Option<SelectedTarget>,
    staged: Option<StagedArtifact>,
}

#[derive(Clone)]
struct DiscoveredTarget {
    dto: DeviceDto,
    authorized: Option<SelectedTarget>,
}

#[derive(Clone)]
enum SelectedTarget {
    Stm32(AuthorizedTarget),
    CobsIap(cobs_can_iap::AuthorizedTarget),
}

impl SelectedTarget {
    fn node_id(&self) -> u8 {
        match self {
            Self::Stm32(target) => target.node_id(),
            Self::CobsIap(target) => target.identity().node_id(),
        }
    }
}

#[derive(Clone)]
enum StagedArtifact {
    Stm32 {
        token: String,
        prepared: PreparedUpgrade,
        dto: PreparedDto,
    },
    CobsIap {
        token: String,
        prepared: CobsPreparedUpgrade,
        dto: PreparedDto,
    },
}

/// Evidence that lets the GUI compare an artifact's intended post-flash
/// 0x1018:03 value with the complete identity read during discovery.
///
/// A supplier IMG header version is deliberately absent: that field is not
/// proven to use the CANopen revision encoding. Only a signed online release
/// can supply that binding for Motor IMG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevisionEvidence {
    StandardMeowpkg(u32),
    SignedMotorRelease(u32),
    UnprovenLocalMotorImg,
}

impl StagedArtifact {
    fn token(&self) -> &str {
        match self {
            Self::Stm32 { token, .. } | Self::CobsIap { token, .. } => token,
        }
    }

    fn backend(&self) -> DfuBackend {
        match self {
            Self::Stm32 { .. } => DfuBackend::Stm32Can,
            Self::CobsIap { .. } => DfuBackend::CobsCanIap,
        }
    }

    fn dto(&self) -> &PreparedDto {
        match self {
            Self::Stm32 { dto, .. } | Self::CobsIap { dto, .. } => dto,
        }
    }

    fn dto_mut(&mut self) -> &mut PreparedDto {
        match self {
            Self::Stm32 { dto, .. } | Self::CobsIap { dto, .. } => dto,
        }
    }
}

#[derive(Clone)]
enum ActiveCancellation {
    Stm32(CancellationToken),
    CobsIap(CobsCancellationToken),
}

impl ActiveCancellation {
    fn cancel(&self) {
        match self {
            Self::Stm32(token) => token.cancel(),
            Self::CobsIap(token) => token.cancel(),
        }
    }
}

pub struct CanDfuState {
    inner: Mutex<CanDfuInner>,
    session_lock: tokio::sync::Mutex<()>,
    active: AtomicBool,
    cancellable: AtomicBool,
    cancellation: Mutex<Option<ActiveCancellation>>,
}

impl Default for CanDfuState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(CanDfuInner::default()),
            session_lock: tokio::sync::Mutex::new(()),
            active: AtomicBool::new(false),
            cancellable: AtomicBool::new(false),
            cancellation: Mutex::new(None),
        }
    }
}

impl CanDfuState {
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn request_cancel(&self) -> bool {
        if !self.is_active() || !self.cancellable.load(Ordering::SeqCst) {
            return false;
        }
        let cancellation = self.cancellation.lock().unwrap();
        let Some(cancellation) = cancellation.as_ref() else {
            return false;
        };
        cancellation.cancel();
        true
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceDto {
    node_id: u8,
    node_id_hex: String,
    device_name: Option<String>,
    vendor_id: u32,
    vendor_id_hex: String,
    product_code: u32,
    product_code_hex: String,
    software_revision: u32,
    software_revision_hex: String,
    serial_number: u32,
    serial_number_hex: String,
    hardware_version: Option<u32>,
    hardware_version_hex: Option<String>,
    authorization: &'static str,
    backend: Option<&'static str>,
    profile_id: Option<String>,
    display_name: Option<&'static str>,
    reason: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryIssueDto {
    node_id: u8,
    node_id_hex: String,
    reason: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryDto {
    devices: Vec<DeviceDto>,
    issues: Vec<DiscoveryIssueDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparedDto {
    token: String,
    device: DeviceDto,
    backend: &'static str,
    artifact_kind: &'static str,
    artifact_sha256: String,
    artifact_size: usize,
    mcu: Option<String>,
    format_version: Option<u16>,
    encrypted: bool,
    img_device_id: Option<u32>,
    img_device_id_hex: Option<String>,
    firmware_id: u32,
    firmware_id_hex: String,
    firmware_version: u32,
    firmware_version_hex: String,
    plaintext_size: Option<usize>,
    wire_size: usize,
    img_start_address_hex: Option<String>,
    img_end_address_hex: Option<String>,
    version_warning: &'static str,
    artifact_source: &'static str,
    release_version: Option<String>,
    expected_postflash_revision: Option<u32>,
    expected_postflash_revision_hex: Option<String>,
    manual_risk_required: bool,
    manual_risk_acknowledged: bool,
    factory_backup_required: bool,
}

#[derive(Debug, Serialize)]
pub struct ProgressDto {
    stage: &'static str,
    completed: u64,
    total: u64,
    cancellable: bool,
}

#[derive(Debug, Serialize)]
pub struct OutcomeDto {
    status: &'static str,
    startup_confirmed: bool,
    recoverable_bootloader_expected: bool,
    factory_backup_path: Option<String>,
    factory_backup_sha256: Option<String>,
    factory_data_status: Option<&'static str>,
}

/// Read-only CAN discovery. The bus is dropped before this command returns.
#[tauri::command]
pub async fn stm32_can_dfu_discover(
    state: State<'_, CanDfuState>,
    can_gate: State<'_, CanTransportGate>,
    spec: String,
) -> CmdResult<DiscoveryDto> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let spec = spec.trim().to_owned();
    if spec.is_empty() {
        return Err("CAN interface must not be empty".into());
    }
    let can_lease = can_gate.try_acquire(CanOwner::DfuDiscovery)?;
    *state.inner.lock().unwrap() = CanDfuInner::default();
    let bus = crate::backend::open_classic_1m_bus(&spec, can_lease)
        .await
        .map_err(|error| error.to_string())?;
    let bus = with_exact_sdo_filter(bus);
    let nodes = observe_heartbeat_nodes(bus.as_ref(), DISCOVERY_WINDOW)
        .await
        .map_err(|error| error.to_string())?;
    let registry = target_registry().map_err(|error| error.to_string())?;
    let cobs_registry = cobs_target_registry().map_err(|error| error.to_string())?;
    ensure_disjoint_backend_routes(&registry, &cobs_registry)?;
    let sdo = CanBusSdo::new(bus.as_ref());

    let mut devices = Vec::new();
    let mut issues = Vec::new();
    let mut discovered = HashMap::new();
    for node_id in nodes {
        let identity = match observe_identity(&sdo, node_id, IDENTITY_TIMEOUT).await {
            Ok(identity) => identity,
            Err(error) => {
                issues.push(DiscoveryIssueDto {
                    node_id,
                    node_id_hex: format!("0x{node_id:02X}"),
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let record = classify_target(&sdo, node_id, identity, &registry, &cobs_registry).await;
        devices.push(record.dto.clone());
        discovered.insert(node_id, record);
    }
    devices.sort_by_key(|device| device.node_id);
    issues.sort_by_key(|issue| issue.node_id);

    let reply = DiscoveryDto { devices, issues };
    let mut inner = state.inner.lock().unwrap();
    inner.spec = Some(spec);
    inner.discovered = discovered;
    Ok(reply)
}

#[tauri::command]
pub async fn stm32_can_dfu_select(state: State<'_, CanDfuState>, node_id: u8) -> CmdResult<()> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let mut inner = state.inner.lock().unwrap();
    inner.selected = None;
    inner.staged = None;
    let record = inner
        .discovered
        .get(&node_id)
        .ok_or_else(|| "the node is not part of the current discovery session".to_owned())?;
    let authorized = record.authorized.clone().ok_or_else(|| {
        format!(
            "node 0x{node_id:02X} is not enabled for update: {}",
            record.dto.reason
        )
    })?;
    inner.selected = Some(authorized);
    Ok(())
}

#[tauri::command]
pub async fn stm32_can_dfu_prepare(
    request: Request<'_>,
    state: State<'_, CanDfuState>,
) -> CmdResult<PreparedDto> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let bytes = match request.body() {
        InvokeBody::Raw(bytes) => {
            if bytes.len() > MAX_ARTIFACT_BYTES {
                return Err(format!(
                    "firmware is {} bytes, above the {}-byte IPC limit",
                    bytes.len(),
                    MAX_ARTIFACT_BYTES
                ));
            }
            bytes.clone()
        }
        InvokeBody::Json(_) => {
            return Err("firmware must be sent as a raw Uint8Array IPC body".into())
        }
    };
    let (target, device) = selected_context(&state)?;
    let (staged, dto) = prepare_artifact_bytes(target, device, bytes, "local", None, None)?;
    state.inner.lock().unwrap().staged = Some(staged);
    Ok(dto)
}

/// Record the attended user's one-use acknowledgement for a manually selected
/// supplier IMG. The acknowledgement is held only in the Rust staged
/// capability and is invalidated by any new discovery, target, or artifact.
#[tauri::command]
pub async fn stm32_can_dfu_acknowledge_manual(
    state: State<'_, CanDfuState>,
    token: String,
) -> CmdResult<PreparedDto> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let mut inner = state.inner.lock().unwrap();
    let staged = inner
        .staged
        .as_mut()
        .ok_or_else(|| "no validated local artifact is staged".to_owned())?;
    if staged.token() != token {
        return Err("artifact token is stale; select and validate the file again".into());
    }
    if !staged.dto().manual_risk_required {
        return Err(
            "the staged artifact does not require a manual IMG risk acknowledgement".into(),
        );
    }
    if staged.dto().artifact_source != "local" || !matches!(staged, StagedArtifact::CobsIap { .. })
    {
        return Err("manual risk acknowledgement applies only to a local motor IMG".into());
    }
    staged.dto_mut().manual_risk_acknowledged = true;
    Ok(staged.dto().clone())
}

/// Download and stage the stable R2 release for the selected exact target.
///
/// The command accepts no URL or identity input. The complete identity and
/// exact local profile were captured by discovery/selection; those values
/// mechanically choose the sole allowed HTTPS path and metadata schema. The
/// meowpkg and supplier IMG schemas remain disjoint; online IMG additionally
/// requires a build-time pinned HexMeow catalog verification key.
#[tauri::command]
pub async fn stm32_can_dfu_prepare_latest(state: State<'_, CanDfuState>) -> CmdResult<PreparedDto> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let (target, device) = selected_context(&state)?;
    if matches!(&target, SelectedTarget::CobsIap(_)) {
        let (staged, dto) = prepare_latest_motor_img(target, device).await?;
        state.inner.lock().unwrap().staged = Some(staged);
        return Ok(dto);
    }
    let SelectedTarget::Stm32(authorized) = &target else {
        unreachable!("the Motor IMG branch returned above")
    };
    let identity = *authorized.identity();
    let profile_id = authorized.target().profile_id().to_owned();
    let identity_root = format!(
        "{R2_ORIGIN}/dfu/v1/releases/{:08x}/{:08x}",
        identity.vendor_id(),
        identity.product_code()
    );

    let client = r2_client()?;

    let latest_url = format!("{identity_root}/latest.json");
    let latest_bytes =
        fetch_bounded(&client, &latest_url, MAX_LATEST_BYTES, "latest pointer").await?;
    let latest = parse_latest_document(
        &latest_bytes,
        &profile_id,
        identity.vendor_id(),
        identity.product_code(),
    )?;
    revision_warning(
        RevisionEvidence::StandardMeowpkg(latest.firmware_version),
        device.software_revision,
    )?;

    let release_url = format!("{identity_root}/{}", latest.release_path);
    let release_bytes =
        fetch_bounded(&client, &release_url, MAX_RELEASE_BYTES, "release binding").await?;
    verify_bound_bytes(
        "release binding",
        &release_bytes,
        latest.release_bytes,
        &latest.release_sha256,
    )?;
    let release = parse_release_document(
        &release_bytes,
        &latest,
        &profile_id,
        identity.vendor_id(),
        identity.product_code(),
    )?;

    let artifact_url = format!(
        "{identity_root}/{}/{}",
        latest.version, release.artifact_path
    );
    let artifact_bytes =
        fetch_bounded(&client, &artifact_url, MAX_ARTIFACT_BYTES, "DFU package").await?;
    verify_bound_bytes(
        "DFU package",
        &artifact_bytes,
        release.artifact_bytes,
        &release.artifact_sha256,
    )?;

    // This is intentionally the exact same native staging path as a manually
    // selected file. Remote metadata can select bytes, but cannot widen the
    // local identity, hardware, MCU, firmware-ID, encryption, or key policy.
    let (staged, dto) = prepare_artifact_bytes(
        target,
        device,
        artifact_bytes,
        "r2",
        Some(latest.version.clone()),
        Some(latest.firmware_version),
    )?;
    if dto.firmware_version != latest.firmware_version {
        return Err(format!(
            "native package firmware version {} does not match online release {}",
            dto.firmware_version_hex, latest.version
        ));
    }
    if dto.artifact_size != release.artifact_bytes || dto.artifact_sha256 != release.artifact_sha256
    {
        return Err("validated package no longer matches its release binding".into());
    }
    state.inner.lock().unwrap().staged = Some(staged);
    Ok(dto)
}

async fn prepare_latest_motor_img(
    target: SelectedTarget,
    device: DeviceDto,
) -> CmdResult<(StagedArtifact, PreparedDto)> {
    let SelectedTarget::CobsIap(authorized) = &target else {
        return Err("internal online Motor IMG backend mismatch".into());
    };
    let profile_id = authorized.target().profile_id();
    let descriptor = target_by_profile_id(profile_id)
        .ok_or_else(|| "selected Motor IMG profile is absent from the target catalog".to_owned())?;
    if !descriptor.is_enabled()
        || descriptor.backend() != TargetBackend::MotorImg
        || descriptor.vendor_id != device.vendor_id
        || descriptor.product_code != device.product_code
    {
        return Err(
            "selected Motor IMG profile no longer matches the local identity catalog".into(),
        );
    }
    let policy = descriptor
        .motor_img_policy()
        .ok_or_else(|| "selected target has no enabled Motor IMG policy".to_owned())?;
    let (catalog_key_id, catalog_public_key) = policy
        .catalog_key_id
        .zip(policy.catalog_public_key)
        .ok_or_else(|| {
            format!(
                "online IMG releases for {profile_id} are locked until a production catalog verification key is provisioned; use the attended local IMG fallback"
            )
        })?;

    let identity_root = motor_identity_root(device.vendor_id, device.product_code);
    let identity_url = format!("{R2_ORIGIN}/{identity_root}");
    let client = r2_client()?;

    let latest_url = format!("{identity_url}/latest.json");
    let latest_bytes = fetch_bounded(
        &client,
        &latest_url,
        MAX_LATEST_BYTES,
        "Motor IMG latest pointer",
    )
    .await?;
    let latest: MotorLatestDocument = parse_motor_latest_json(&latest_bytes)
        .map_err(|error| format!("invalid Motor IMG latest pointer: {error}"))?;
    verify_motor_latest(
        &latest,
        profile_id,
        device.vendor_id,
        device.product_code,
        catalog_key_id,
        &catalog_public_key,
    )
    .map_err(|error| format!("Motor IMG latest signature/binding failed: {error}"))?;

    let release_bytes_len = bounded_declared_size(
        "Motor IMG release binding",
        latest.release.bytes,
        MAX_RELEASE_BYTES,
    )?;
    let release_url = format!("{identity_url}/{}", latest.release.path);
    let release_bytes = fetch_bounded(
        &client,
        &release_url,
        MAX_RELEASE_BYTES,
        "Motor IMG release binding",
    )
    .await?;
    verify_bound_bytes(
        "Motor IMG release binding",
        &release_bytes,
        release_bytes_len,
        &latest.release.sha256,
    )?;
    let release: MotorReleaseDocument = parse_motor_release_json(&release_bytes)
        .map_err(|error| format!("invalid Motor IMG release binding: {error}"))?;
    verify_motor_release(
        &release,
        profile_id,
        device.vendor_id,
        device.product_code,
        catalog_key_id,
        &catalog_public_key,
    )
    .map_err(|error| format!("Motor IMG release signature/binding failed: {error}"))?;

    if release.sequence != latest.sequence
        || release.release_id != latest.release_id
        || release.native_img.firmware_version != latest.native_firmware_version
    {
        return Err("Motor IMG release changed sequence, release ID or native version after latest validation".into());
    }
    if release.expected_postflash.vendor_id != descriptor.expected_postflash_identity.vendor_id
        || release.expected_postflash.product_code
            != descriptor.expected_postflash_identity.product_code
    {
        return Err(
            "Motor IMG release expected post-flash identity differs from the local profile".into(),
        );
    }
    // Reject as soon as signed revision evidence is available. In particular,
    // do this before fetching the IMG body, and long before opening a CAN bus
    // for mutation. Equal revisions intentionally remain valid reinstalls.
    revision_warning(
        RevisionEvidence::SignedMotorRelease(release.expected_postflash.revision_number),
        device.software_revision,
    )?;

    let artifact_bytes_len = bounded_declared_size(
        "Motor IMG artifact",
        release.artifact.bytes,
        MAX_ARTIFACT_BYTES,
    )?;
    let artifact_url = format!(
        "{identity_url}/{}/{}",
        release.release_id, release.artifact.path
    );
    let artifact_bytes = fetch_bounded(
        &client,
        &artifact_url,
        MAX_ARTIFACT_BYTES,
        "Motor IMG artifact",
    )
    .await?;
    verify_bound_bytes(
        "Motor IMG artifact",
        &artifact_bytes,
        artifact_bytes_len,
        &release.artifact.sha256,
    )?;

    let (staged, dto) = prepare_artifact_bytes(
        target,
        device,
        artifact_bytes,
        "r2",
        Some(release.release_id.clone()),
        Some(release.expected_postflash.revision_number),
    )?;
    let StagedArtifact::CobsIap { prepared, .. } = &staged else {
        return Err("validated online IMG selected the wrong native backend".into());
    };
    let artifact = prepared.artifact();
    let native_matches = artifact.device_id() == release.native_img.device_id
        && artifact.firmware_id() == release.native_img.firmware_id
        && artifact.firmware_version() == release.native_img.firmware_version
        && matches!(
            artifact.encryption(),
            cobs_can_iap::EncryptionMode::Encrypted
        ) == release.native_img.encrypted
        && hex::encode(artifact.hash()) == release.native_img.protected_sha256
        && hex::encode(artifact.signature()) == release.native_img.vendor_signature
        && hex::encode(artifact.iv()) == release.native_img.iv
        && artifact.start_address() == release.native_img.start_address
        && artifact.end_address() == release.native_img.end_address
        && artifact.bin_size() as u64 == release.native_img.bin_size;
    if !native_matches
        || dto.artifact_size != artifact_bytes_len
        || dto.artifact_sha256 != release.artifact.sha256
    {
        return Err(
            "parsed IMG no longer matches its signed release header and content binding".into(),
        );
    }
    Ok((staged, dto))
}

fn r2_client() -> CmdResult<reqwest::Client> {
    // reqwest's no-provider mode avoids pulling in AWS-LC/CMake. The rest of
    // this application already uses rustls with ring, so explicitly install
    // that provider before constructing this client. A concurrently installed
    // process-wide provider is equally valid and wins the one-time race.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        return Err("cannot install the rustls crypto provider".into());
    }

    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(R2_CONNECT_TIMEOUT)
        .timeout(R2_REQUEST_TIMEOUT)
        .user_agent(concat!("hexmeow-gui/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("building the R2 HTTPS client: {error}"))
}

fn selected_context(state: &CanDfuState) -> CmdResult<(SelectedTarget, DeviceDto)> {
    let mut inner = state.inner.lock().unwrap();
    inner.staged = None;
    let target = inner
        .selected
        .clone()
        .ok_or_else(|| "select an authorized CAN target before choosing firmware".to_owned())?;
    let device = inner
        .discovered
        .get(&target.node_id())
        .map(|record| record.dto.clone())
        .ok_or_else(|| "the selected discovery session is stale".to_owned())?;
    Ok((target, device))
}

fn prepare_artifact_bytes(
    target: SelectedTarget,
    device: DeviceDto,
    bytes: Vec<u8>,
    artifact_source: &'static str,
    release_version: Option<String>,
    expected_postflash_revision: Option<u32>,
) -> CmdResult<(StagedArtifact, PreparedDto)> {
    let token = format!(
        "{:016x}",
        getrandom::u64().map_err(|error| format!("cannot create artifact token: {error}"))?
    );
    let source_size = bytes.len();
    let source_sha256 = hex_sha256(&bytes);
    let (staged, dto) = match target {
        SelectedTarget::Stm32(target) => {
            let current_revision = device.software_revision;
            let package = read_package_bytes(
                &bytes,
                PackageLimits {
                    max_archive_bytes: 2 * 1024 * 1024,
                    max_manifest_bytes: 64 * 1024,
                    max_image_bytes: 1024 * 1024,
                    max_entries: 3,
                },
            )
            .map_err(|error| error.to_string())?;
            let prepared =
                PreparedUpgrade::bind(target, package).map_err(|error| error.to_string())?;
            let summary = prepared.package();
            let manifest = summary.manifest();
            let format_version = manifest
                .payload_format
                .as_ref()
                .and_then(|format| format.stm32_header_version)
                .ok_or_else(|| "validated STM32 package has no header version".to_owned())?;
            let version_warning = revision_warning(
                RevisionEvidence::StandardMeowpkg(manifest.firmware_version),
                current_revision,
            )?;
            let dto = PreparedDto {
                token: token.clone(),
                device,
                backend: "stm32_canopen",
                artifact_kind: "meowpkg",
                artifact_sha256: source_sha256,
                artifact_size: source_size,
                mcu: Some(manifest.mcu.clone()),
                format_version: Some(format_version),
                encrypted: matches!(summary.image_mode(), Stm32ImageMode::EncryptedV2),
                img_device_id: None,
                img_device_id_hex: None,
                firmware_id: manifest.firmware_id,
                firmware_id_hex: format!("0x{:08X}", manifest.firmware_id),
                firmware_version: manifest.firmware_version,
                firmware_version_hex: format!("0x{:08X}", manifest.firmware_version),
                plaintext_size: Some(summary.plaintext_size()),
                wire_size: summary.wire_size(),
                img_start_address_hex: None,
                img_end_address_hex: None,
                version_warning,
                artifact_source,
                release_version,
                expected_postflash_revision,
                expected_postflash_revision_hex: expected_postflash_revision
                    .map(|value| format!("0x{value:08X}")),
                manual_risk_required: false,
                manual_risk_acknowledged: true,
                factory_backup_required: false,
            };
            (
                StagedArtifact::Stm32 {
                    token: token.clone(),
                    prepared,
                    dto: dto.clone(),
                },
                dto,
            )
        }
        SelectedTarget::CobsIap(target) => {
            let descriptor =
                target_by_profile_id(target.target().profile_id()).ok_or_else(|| {
                    "selected motor IMG profile is absent from the build-time target catalog"
                        .to_owned()
                })?;
            if descriptor.backend() != TargetBackend::MotorImg || !descriptor.is_enabled() {
                return Err(
                    "selected motor IMG profile is not enabled in the target catalog".into(),
                );
            }
            let factory_backup_required =
                descriptor.preflight == PreflightPolicy::BackupMeowFactory4001;
            let artifact = ImgArtifact::parse(
                &bytes,
                ImgLimits {
                    max_file_bytes: MAX_ARTIFACT_BYTES,
                    max_bin_bytes: MAX_ARTIFACT_BYTES - cobs_can_iap::IMG_TAG_SIZE,
                },
            )
            .map_err(|error| error.to_string())?;
            let prepared =
                CobsPreparedUpgrade::bind(target, artifact).map_err(|error| error.to_string())?;
            let artifact = prepared.artifact();
            let revision_evidence = match (artifact_source, expected_postflash_revision) {
                ("r2", Some(revision)) => RevisionEvidence::SignedMotorRelease(revision),
                ("r2", None) => {
                    return Err(
                        "online Motor IMG is missing its signed expected post-flash revision"
                            .into(),
                    )
                }
                ("local", None) => RevisionEvidence::UnprovenLocalMotorImg,
                ("local", Some(_)) => {
                    return Err(
                        "local Motor IMG must not claim an unproven post-flash revision".into(),
                    )
                }
                _ => return Err("internal firmware artifact source is invalid".into()),
            };
            let version_warning = revision_warning(revision_evidence, device.software_revision)?;
            let dto = PreparedDto {
                token: token.clone(),
                device,
                backend: "cobs_can_iap_v1",
                artifact_kind: "compatible_img",
                artifact_sha256: source_sha256,
                artifact_size: source_size,
                mcu: None,
                format_version: None,
                encrypted: matches!(
                    artifact.encryption(),
                    cobs_can_iap::EncryptionMode::Encrypted
                ),
                img_device_id: Some(artifact.device_id()),
                img_device_id_hex: Some(format!("0x{:08X}", artifact.device_id())),
                firmware_id: artifact.firmware_id(),
                firmware_id_hex: format!("0x{:08X}", artifact.firmware_id()),
                firmware_version: artifact.firmware_version(),
                firmware_version_hex: format!("0x{:08X}", artifact.firmware_version()),
                plaintext_size: None,
                wire_size: artifact.bin_size(),
                img_start_address_hex: Some(format!("0x{:08X}", artifact.start_address())),
                img_end_address_hex: Some(format!("0x{:08X}", artifact.end_address())),
                version_warning,
                artifact_source,
                release_version,
                expected_postflash_revision,
                expected_postflash_revision_hex: expected_postflash_revision
                    .map(|value| format!("0x{value:08X}")),
                manual_risk_required: artifact_source == "local",
                manual_risk_acknowledged: artifact_source != "local",
                // This gate comes only from the build-time identity catalog;
                // neither the IMG nor remote release metadata may set it.
                factory_backup_required,
            };
            (
                StagedArtifact::CobsIap {
                    token: token.clone(),
                    prepared,
                    dto: dto.clone(),
                },
                dto,
            )
        }
    };
    Ok((staged, dto))
}

fn revision_warning(
    evidence: RevisionEvidence,
    installed_revision: u32,
) -> CmdResult<&'static str> {
    let (target_revision, evidence_label) = match evidence {
        RevisionEvidence::StandardMeowpkg(revision) => {
            (revision, ".meowpkg manifest firmware_version")
        }
        RevisionEvidence::SignedMotorRelease(revision) => (
            revision,
            "signed Motor IMG expected_postflash.revision_number",
        ),
        // The native IMG header version is intentionally ignored. Its numeric
        // relationship to CANopen 0x1018:03 has not been established.
        RevisionEvidence::UnprovenLocalMotorImg => return Ok("unknown"),
    };
    match target_revision.cmp(&installed_revision) {
        std::cmp::Ordering::Less => Err(format!(
            "{evidence_label} 0x{target_revision:08X} is older than the selected device's current 0x1018:03 revision 0x{installed_revision:08X}; firmware downgrade is refused"
        )),
        std::cmp::Ordering::Equal => Ok("reinstall"),
        std::cmp::Ordering::Greater => Ok("none"),
    }
}

async fn fetch_bounded(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
    label: &str,
) -> CmdResult<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("downloading {label} from R2: {error}"))?;
    if response.url().as_str() != url {
        return Err(format!("{label} response changed the fixed R2 URL"));
    }
    if !response.status().is_success() {
        return Err(format!(
            "{label} is unavailable at the fixed R2 path (HTTP {})",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!(
            "{label} Content-Length exceeds the {max_bytes}-byte limit"
        ));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("reading {label} from R2: {error}"))?
    {
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| format!("{label} length overflow"))?;
        if next_len > max_bytes {
            return Err(format!("{label} exceeds the {max_bytes}-byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err(format!("{label} is empty"));
    }
    Ok(bytes)
}

fn parse_latest_document(
    bytes: &[u8],
    expected_profile: &str,
    expected_vendor: u32,
    expected_product: u32,
) -> CmdResult<ValidatedLatest> {
    let document: LatestDocument = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid latest pointer JSON: {error}"))?;
    if document.format != LATEST_FORMAT {
        return Err(format!(
            "unsupported latest pointer format {:?}",
            document.format
        ));
    }
    validate_release_identity(
        "latest pointer",
        &document.profile_id,
        &document.vendor_id,
        &document.product_code,
        expected_profile,
        expected_vendor,
        expected_product,
    )?;
    if document.channel != R2_CHANNEL {
        return Err(format!(
            "unsupported release channel {:?}",
            document.channel
        ));
    }
    let firmware_version = parse_release_version(&document.version)?;
    let expected_release_path = format!("{}/release.json", document.version);
    if document.release.path != expected_release_path {
        return Err(format!(
            "latest release path must be {expected_release_path:?}"
        ));
    }
    validate_lower_sha256("latest release SHA-256", &document.release.sha256)?;
    let release_bytes = bounded_declared_size(
        "latest release binding",
        document.release.bytes,
        MAX_RELEASE_BYTES,
    )?;
    Ok(ValidatedLatest {
        version: document.version,
        firmware_version,
        release_path: document.release.path,
        release_sha256: document.release.sha256,
        release_bytes,
    })
}

fn parse_release_document(
    bytes: &[u8],
    latest: &ValidatedLatest,
    expected_profile: &str,
    expected_vendor: u32,
    expected_product: u32,
) -> CmdResult<ValidatedRelease> {
    let document: ReleaseDocument = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid release binding JSON: {error}"))?;
    if document.format != RELEASE_FORMAT {
        return Err(format!(
            "unsupported release binding format {:?}",
            document.format
        ));
    }
    validate_release_identity(
        "release binding",
        &document.profile_id,
        &document.vendor_id,
        &document.product_code,
        expected_profile,
        expected_vendor,
        expected_product,
    )?;
    if document.version != latest.version {
        return Err("release binding version does not match latest pointer".into());
    }
    if parse_release_version(&document.version)? != latest.firmware_version {
        return Err("release binding version encoding changed after pointer validation".into());
    }
    if document.tag != format!("v{}", latest.version) {
        return Err("release binding tag does not match its canonical version".into());
    }
    if !matches!(document.candidate_source_commit.len(), 40 | 64)
        || !document
            .candidate_source_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("release binding contains an invalid source commit".into());
    }
    validate_lower_sha256("release artifact SHA-256", &document.artifact.sha256)?;
    let expected_artifact_path = format!("{}.meowpkg", document.artifact.sha256);
    if document.artifact.path != expected_artifact_path {
        return Err(format!(
            "release artifact path must be the single filename {expected_artifact_path:?}"
        ));
    }
    let artifact_bytes = bounded_declared_size(
        "release artifact",
        document.artifact.bytes,
        MAX_ARTIFACT_BYTES,
    )?;
    Ok(ValidatedRelease {
        artifact_path: document.artifact.path,
        artifact_sha256: document.artifact.sha256,
        artifact_bytes,
    })
}

fn validate_release_identity(
    label: &str,
    profile_id: &str,
    vendor_id: &str,
    product_code: &str,
    expected_profile: &str,
    expected_vendor: u32,
    expected_product: u32,
) -> CmdResult<()> {
    if profile_id != expected_profile {
        return Err(format!(
            "{label} profile does not match the selected local profile"
        ));
    }
    if vendor_id != format!("0x{expected_vendor:08X}")
        || product_code != format!("0x{expected_product:08X}")
    {
        return Err(format!(
            "{label} identity does not match the selected device"
        ));
    }
    Ok(())
}

fn parse_release_version(version: &str) -> CmdResult<u32> {
    if version.contains('-') || version.contains('+') {
        return Err("release version must not contain pre-release or build metadata".into());
    }
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
        || parts[2] != "0"
    {
        return Err("release version must be canonical M.m.0 SemVer".into());
    }
    let major = parts[0]
        .parse::<u16>()
        .map_err(|_| "release major version exceeds u16".to_owned())?;
    let minor = parts[1]
        .parse::<u16>()
        .map_err(|_| "release minor version exceeds u16".to_owned())?;
    Ok((u32::from(major) << 16) | u32::from(minor))
}

fn validate_lower_sha256(label: &str, value: &str) -> CmdResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "{label} must be 64 lowercase hexadecimal characters"
        ));
    }
    Ok(())
}

fn bounded_declared_size(label: &str, value: u64, limit: usize) -> CmdResult<usize> {
    let value = usize::try_from(value).map_err(|_| format!("{label} size exceeds usize"))?;
    if value == 0 || value > limit {
        return Err(format!("{label} size must be within 1..={limit} bytes"));
    }
    Ok(value)
}

fn verify_bound_bytes(
    label: &str,
    bytes: &[u8],
    expected_size: usize,
    expected_sha256: &str,
) -> CmdResult<()> {
    if bytes.len() != expected_size {
        return Err(format!(
            "{label} size mismatch: expected {expected_size}, got {}",
            bytes.len()
        ));
    }
    if hex_sha256(bytes) != expected_sha256 {
        return Err(format!("{label} SHA-256 does not match its binding"));
    }
    Ok(())
}

struct ActiveReset<'a> {
    active: &'a AtomicBool,
    cancellable: &'a AtomicBool,
    cancellation: &'a Mutex<Option<ActiveCancellation>>,
}

impl Drop for ActiveReset<'_> {
    fn drop(&mut self) {
        *self.cancellation.lock().unwrap() = None;
        self.cancellable.store(false, Ordering::SeqCst);
        self.active.store(false, Ordering::SeqCst);
    }
}

/// Mutation entry point for all locally registered CAN update profiles.
///
/// The selected 0x1018 row fixes the backend before a file is parsed. A token
/// is single-use, and the complete identity is reread on the exact update bus
/// before either backend can transmit its first protocol-specific mutation.
#[tauri::command]
pub async fn stm32_can_dfu_start(
    app: tauri::AppHandle,
    state: State<'_, CanDfuState>,
    mutation_gate: State<'_, DfuMutationGate>,
    can_gate: State<'_, CanTransportGate>,
    token: String,
    on_event: Channel<ProgressDto>,
) -> CmdResult<OutcomeDto> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let (spec, staged) = {
        let inner = state.inner.lock().unwrap();
        let staged = inner
            .staged
            .as_ref()
            .ok_or_else(|| "no validated CAN artifact is staged".to_owned())?;
        if staged.token() != token {
            return Err("artifact token is stale; select and validate the file again".into());
        }
        if staged.dto().manual_risk_required && !staged.dto().manual_risk_acknowledged {
            return Err(
                "acknowledge the manually selected IMG risk after reviewing its identity and SHA-256"
                    .into(),
            );
        }
        let spec = inner
            .spec
            .clone()
            .ok_or_else(|| "the CAN discovery session is stale".to_owned())?;
        (spec, staged.clone())
    };

    // Defense in depth at the command boundary: no mutation permit, adapter,
    // backup, Reset, or download stage is reached with an older proven target.
    // The subsequent backend revalidation still rereads the complete 0x1018
    // identity and proves it is unchanged from discovery.
    let staged_evidence = match &staged {
        StagedArtifact::Stm32 { dto, .. } => {
            RevisionEvidence::StandardMeowpkg(dto.firmware_version)
        }
        StagedArtifact::CobsIap { dto, .. } if dto.artifact_source == "r2" => {
            RevisionEvidence::SignedMotorRelease(dto.expected_postflash_revision.ok_or_else(
                || "online Motor IMG is missing its signed expected post-flash revision".to_owned(),
            )?)
        }
        StagedArtifact::CobsIap { .. } => RevisionEvidence::UnprovenLocalMotorImg,
    };
    revision_warning(staged_evidence, staged.dto().device.software_revision)?;

    let _mutation_permit = mutation_gate
        .try_acquire(staged.backend())
        .map_err(str::to_owned)?;
    let can_lease = can_gate.try_acquire(CanOwner::DfuUpdate)?;
    let cancellation = match &staged {
        StagedArtifact::Stm32 { .. } => ActiveCancellation::Stm32(CancellationToken::new()),
        StagedArtifact::CobsIap { .. } => ActiveCancellation::CobsIap(CobsCancellationToken::new()),
    };
    *state.cancellation.lock().unwrap() = Some(cancellation.clone());
    if state
        .active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        *state.cancellation.lock().unwrap() = None;
        return Err("an upgrade is already running".to_owned());
    }
    state.cancellable.store(true, Ordering::SeqCst);
    let _active_reset = ActiveReset {
        active: &state.active,
        cancellable: &state.cancellable,
        cancellation: &state.cancellation,
    };

    // A validated artifact token is single-use even when opening the adapter
    // or final identity revalidation fails.
    state.inner.lock().unwrap().staged = None;

    send_progress(
        &on_event,
        ProgressDto {
            stage: "revalidating",
            completed: 0,
            total: 1,
            cancellable: true,
        },
    );
    let bus = crate::backend::open_classic_1m_bus(&spec, can_lease)
        .await
        .map_err(|error| error.to_string())?;
    let bus = with_exact_sdo_filter(bus);
    match (staged, cancellation) {
        (StagedArtifact::Stm32 { prepared, .. }, ActiveCancellation::Stm32(cancellation)) => {
            run_stm32_update(bus.as_ref(), prepared, cancellation, &on_event, &state).await
        }
        (
            StagedArtifact::CobsIap { prepared, dto, .. },
            ActiveCancellation::CobsIap(cancellation),
        ) => {
            let backup_root = app
                .path()
                .app_data_dir()
                .map_err(|error| format!("resolving application data directory: {error}"))?
                .join("backups")
                .join("firmware-update");
            run_cobs_iap_update(
                bus.as_ref(),
                prepared,
                dto,
                &spec,
                &backup_root,
                cancellation,
                &on_event,
                &state,
            )
            .await
        }
        _ => Err("internal CAN update backend/cancellation mismatch".to_owned()),
    }
}

async fn run_stm32_update(
    bus: &dyn CanBus,
    prepared: PreparedUpgrade,
    cancellation: CancellationToken,
    on_event: &Channel<ProgressDto>,
    state: &CanDfuState,
) -> CmdResult<OutcomeDto> {
    let registry = target_registry().map_err(|error| error.to_string())?;
    let sdo = CanBusSdo::new(bus);
    let ready = revalidate_prepared(&sdo, &prepared, &registry, IDENTITY_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    let wire_total = ready.package().wire_size();
    let result = flash(
        &sdo,
        ready,
        &FlashOptions::default(),
        &cancellation,
        |event| send_flash_progress(on_event, event, wire_total, &state.cancellable),
    )
    .await;

    match result {
        Ok(_) => Ok(OutcomeDto {
            status: "application_verified",
            startup_confirmed: true,
            recoverable_bootloader_expected: false,
            factory_backup_path: None,
            factory_backup_sha256: None,
            factory_data_status: None,
        }),
        Err(FlashError::Cancelled {
            stage: FlashStage::Claiming,
        }) => Ok(OutcomeDto {
            status: "cancelled_before_write",
            startup_confirmed: false,
            recoverable_bootloader_expected: false,
            factory_backup_path: None,
            factory_backup_sha256: None,
            factory_data_status: None,
        }),
        Err(FlashError::Cancelled { .. }) => Ok(OutcomeDto {
            status: "cancelled_recoverable",
            startup_confirmed: false,
            recoverable_bootloader_expected: true,
            factory_backup_path: None,
            factory_backup_sha256: None,
            factory_data_status: None,
        }),
        Err(error) => Err(format!(
            "{error}. If update writes had started, keep the device powered. It may be recoverable in Bootloader, or an application may have started but failed host confirmation; inspect its identity and run one complete qualified upgrade again if needed"
        )),
    }
}

async fn run_cobs_iap_update(
    bus: &dyn CanBus,
    prepared: CobsPreparedUpgrade,
    dto: PreparedDto,
    can_interface: &str,
    backup_root: &std::path::Path,
    cancellation: CobsCancellationToken,
    on_event: &Channel<ProgressDto>,
    state: &CanDfuState,
) -> CmdResult<OutcomeDto> {
    let registry = cobs_target_registry().map_err(|error| error.to_string())?;
    let expected = prepared.target().identity();
    let sdo = CanBusSdo::new(bus);
    let mut actual = observe_cobs_identity(&sdo, expected.node_id()).await?;
    ensure_cobs_identity_unchanged(expected, actual)?;

    // Only the two exact Meow Motor identities carry this build-time gate.
    // The backup is complete, opaque and durable before Reset/Enter-IAP; IMG
    // bytes and remote metadata cannot opt in or out of it.
    let persisted_backup = if dto.factory_backup_required {
        if cancellation.is_cancelled() {
            return Ok(cobs_outcome(
                "cancelled_before_write",
                false,
                false,
                None,
                None,
            ));
        }
        // A stable full-record read and its durable commit form one read-only
        // atomic preflight. Do not claim an immediate cancellation point in
        // the middle: cancellation becomes available again before Reset.
        state.cancellable.store(false, Ordering::SeqCst);
        send_progress(
            on_event,
            ProgressDto {
                stage: "backing_up_factory_data",
                completed: 0,
                total: 1,
                cancellable: false,
            },
        );
        let snapshot = read_stable_snapshot(&sdo, expected.node_id()).await?;
        let artifact = prepared.artifact();
        let context = BackupContext {
            can_interface: can_interface.to_owned(),
            node_id: expected.node_id(),
            profile_id: prepared.target().target().profile_id().to_owned(),
            identity: BackupIdentity {
                vendor_id: expected.vendor_id(),
                product_code: expected.product_code(),
                revision_number: expected.revision_number(),
                serial_number: expected.serial_number(),
            },
            artifact: BackupArtifact {
                source: dto.artifact_source,
                release_id: dto.release_version.clone(),
                sha256: dto.artifact_sha256.clone(),
                bytes: dto.artifact_size,
                device_id: artifact.device_id(),
                firmware_id: artifact.firmware_id(),
                firmware_version: artifact.firmware_version(),
                start_address: artifact.start_address(),
                end_address: artifact.end_address(),
                bin_size: artifact.bin_size(),
            },
        };
        let root = backup_root.to_owned();
        let persisted =
            tokio::task::spawn_blocking(move || persist_backup(&root, &context, &snapshot))
                .await
                .map_err(|error| format!("joining 0x4001 backup persistence task: {error}"))??;

        if cancellation.is_cancelled() {
            return Ok(cobs_outcome(
                "cancelled_before_write",
                false,
                false,
                Some(&persisted),
                Some("startup_unconfirmed"),
            ));
        }

        // Prove that the durable snapshot still describes this exact device
        // immediately before minting the short-lived ReadyToFlash capability.
        send_progress(
            on_event,
            ProgressDto {
                stage: "checking_factory_data",
                completed: 0,
                total: 1,
                cancellable: false,
            },
        );
        let final_snapshot = read_stable_snapshot(&sdo, expected.node_id())
            .await
            .map_err(|error| {
                format!(
                    "{error}; no IAP mutation was sent. Retained backup: {}",
                    persisted.path.display()
                )
            })?;
        if final_snapshot != persisted.snapshot {
            return Err(format!(
                "0x4001 changed after its backup was committed; no IAP mutation was sent. Retained backup: {}",
                persisted.path.display()
            ));
        }
        let final_identity = observe_cobs_identity(&sdo, expected.node_id())
            .await
            .map_err(|error| {
                format!(
                    "{error}; final identity revalidation failed before Reset. Retained backup: {}",
                    persisted.path.display()
                )
            })?;
        ensure_cobs_identity_unchanged(expected, final_identity)
            .map_err(|error| format!("{error} Retained backup: {}", persisted.path.display()))?;
        actual = final_identity;
        Some(persisted)
    } else {
        None
    };

    state.cancellable.store(true, Ordering::SeqCst);
    send_progress(
        on_event,
        ProgressDto {
            stage: "revalidating",
            completed: 1,
            total: 1,
            cancellable: true,
        },
    );
    if cancellation.is_cancelled() {
        return Ok(cobs_outcome(
            "cancelled_before_write",
            false,
            false,
            persisted_backup.as_ref(),
            persisted_backup.as_ref().map(|_| "startup_unconfirmed"),
        ));
    }
    let ready = prepared
        .revalidate(actual, &registry)
        .map_err(|error| error.to_string())?;
    let wire_total = ready_artifact_size(&prepared);
    let result = flash_cobs_iap(
        bus,
        ready,
        &CobsFlashOptions::default(),
        &cancellation,
        |event| send_cobs_flash_progress(on_event, event, wire_total, &state.cancellable),
    )
    .await;

    match result {
        Ok(_) => {
            let Some(backup) = persisted_backup.as_ref() else {
                return Ok(cobs_outcome(
                    "verify_acked_startup_unconfirmed",
                    false,
                    false,
                    None,
                    None,
                ));
            };

            send_progress(
                on_event,
                ProgressDto {
                    stage: "checking_factory_data",
                    completed: 1,
                    total: 1,
                    cancellable: false,
                },
            );
            match observe_postflash_factory_data(
                &sdo,
                expected,
                dto.expected_postflash_revision,
                &backup.snapshot,
            )
            .await
            {
                PostflashFactoryCheck::Preserved => Ok(cobs_outcome(
                    "verify_acked_factory_data_preserved",
                    true,
                    false,
                    Some(backup),
                    Some("preserved"),
                )),
                PostflashFactoryCheck::RecoveryRequired => Ok(cobs_outcome(
                    "verify_acked_factory_data_recovery_required",
                    true,
                    false,
                    Some(backup),
                    Some("recovery_required"),
                )),
                PostflashFactoryCheck::StartupUnconfirmed => Ok(cobs_outcome(
                    "verify_acked_startup_unconfirmed",
                    false,
                    false,
                    Some(backup),
                    Some("startup_unconfirmed"),
                )),
            }
        }
        Err(CobsFlashError::Cancelled {
            recovery_required: false,
            ..
        }) => Ok(cobs_outcome(
            "cancelled_before_write",
            false,
            false,
            persisted_backup.as_ref(),
            persisted_backup.as_ref().map(|_| "startup_unconfirmed"),
        )),
        Err(CobsFlashError::Cancelled { .. }) => Ok(cobs_outcome(
            "cancelled_recoverable",
            false,
            true,
            persisted_backup.as_ref(),
            persisted_backup.as_ref().map(|_| "startup_unconfirmed"),
        )),
        Err(error) => {
            let recovery = error.recovery_required();
            let backup = persisted_backup
                .as_ref()
                .map(|backup| format!(" Retained 0x4001 backup: {}.", backup.path.display()))
                .unwrap_or_default();
            Err(format!(
                "{error}.{backup} {}",
                if recovery {
                    "The command outcome may be ambiguous after download began. Keep the device powered and do not retry blindly. This test backend intentionally refuses an unidentified all-0xFF recovery identity until a product-specific recovery path is hardware-qualified"
                } else {
                    "No download write was confirmed. Recheck the exact device identity and IMG policy before retrying"
                }
            ))
        }
    }
}

fn cobs_outcome(
    status: &'static str,
    startup_confirmed: bool,
    recoverable_bootloader_expected: bool,
    backup: Option<&PersistedBackup>,
    factory_data_status: Option<&'static str>,
) -> OutcomeDto {
    OutcomeDto {
        status,
        startup_confirmed,
        recoverable_bootloader_expected,
        factory_backup_path: backup.map(|backup| backup.path.to_string_lossy().into_owned()),
        factory_backup_sha256: backup.map(|backup| backup.file_sha256.clone()),
        factory_data_status,
    }
}

async fn observe_cobs_identity(
    sdo: &(impl hexmeow_stm32_can_dfu::SdoTransport + ?Sized),
    node_id: u8,
) -> CmdResult<CobsCanopenIdentity> {
    let observed = observe_identity(sdo, node_id, IDENTITY_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    CobsCanopenIdentity::new(
        node_id,
        observed.vendor_id(),
        observed.product_code(),
        observed.revision_number(),
        observed.serial_number(),
    )
    .map_err(|error| error.to_string())
}

fn ensure_cobs_identity_unchanged(
    expected: CobsCanopenIdentity,
    actual: CobsCanopenIdentity,
) -> CmdResult<()> {
    if actual != expected {
        return Err(
            "complete 0x1018 identity changed before 0x4001 backup or Reset; no IAP mutation was sent"
                .into(),
        );
    }
    Ok(())
}

async fn observe_postflash_factory_data(
    sdo: &(impl hexmeow_stm32_can_dfu::SdoTransport + ?Sized),
    expected: CobsCanopenIdentity,
    expected_revision: Option<u32>,
    backup: &crate::motor_factory_backup::RawFactorySnapshot,
) -> PostflashFactoryCheck {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let actual = loop {
        match observe_cobs_identity(sdo, expected.node_id()).await {
            Ok(identity) => break identity,
            Err(error) if tokio::time::Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(_) => return PostflashFactoryCheck::StartupUnconfirmed,
        }
    };
    if actual.vendor_id() != expected.vendor_id()
        || actual.product_code() != expected.product_code()
        || actual.serial_number() != expected.serial_number()
        || expected_revision.is_some_and(|revision| actual.revision_number() != revision)
    {
        return PostflashFactoryCheck::RecoveryRequired;
    }
    match read_stable_snapshot(sdo, expected.node_id()).await {
        Ok(after) if after == *backup => PostflashFactoryCheck::Preserved,
        Ok(_) | Err(_) => PostflashFactoryCheck::RecoveryRequired,
    }
}

enum PostflashFactoryCheck {
    Preserved,
    RecoveryRequired,
    StartupUnconfirmed,
}

fn ready_artifact_size(prepared: &CobsPreparedUpgrade) -> usize {
    prepared.artifact().bin_size()
}

#[tauri::command]
pub fn stm32_can_dfu_cancel(state: State<'_, CanDfuState>) -> bool {
    state.request_cancel()
}

#[tauri::command]
pub async fn stm32_can_dfu_leave(state: State<'_, CanDfuState>) -> CmdResult<()> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("wait for the current CAN protocol command before leaving".into());
    }
    *state.cancellation.lock().unwrap() = None;
    *state.inner.lock().unwrap() = CanDfuInner::default();
    Ok(())
}

fn send_progress(channel: &Channel<ProgressDto>, progress: ProgressDto) {
    // Losing the WebView/channel must not interrupt an erase or leave START in
    // an ambiguous state. The backend continues to a protocol terminal state.
    let _ = channel.send(progress);
}

fn send_flash_progress(
    channel: &Channel<ProgressDto>,
    event: FlashEvent,
    total: usize,
    cancellable: &AtomicBool,
) {
    if matches!(
        &event,
        FlashEvent::Stage(
            FlashStage::VerifyingAndStarting
                | FlashStage::WaitingForApplication
                | FlashStage::ApplicationConfirmed
        )
    ) {
        // The engine deliberately has no cancellation point after this stage
        // notification: START may be in flight and only application
        // confirmation can disambiguate the terminal state.
        cancellable.store(false, Ordering::SeqCst);
    }
    let total = total as u64;
    let progress = match event {
        FlashEvent::Stage(stage) => {
            let (stage, completed, cancellable) = match stage {
                FlashStage::Claiming | FlashStage::WaitingForBootloader => {
                    ("entering_bootloader", 0, true)
                }
                FlashStage::BootloaderAuthorized => ("entering_bootloader", 0, true),
                FlashStage::Header => ("writing_header", 0, true),
                FlashStage::Clear => ("clearing", 0, true),
                FlashStage::Streaming => ("writing", 0, true),
                FlashStage::VerifyingAndStarting => ("verifying_and_starting", total, false),
                FlashStage::WaitingForApplication => ("confirming_application", total, false),
                FlashStage::ApplicationConfirmed => ("confirming_application", total, false),
            };
            Some(ProgressDto {
                stage,
                completed,
                total,
                cancellable,
            })
        }
        FlashEvent::Progress { written, total } => Some(ProgressDto {
            stage: "writing",
            completed: written as u64,
            total: total as u64,
            cancellable: true,
        }),
        FlashEvent::Resynchronized {
            authoritative_offset,
            ..
        } => Some(ProgressDto {
            stage: "writing",
            completed: authoritative_offset as u64,
            total,
            cancellable: true,
        }),
        FlashEvent::StartAcknowledgement { .. } => None,
    };
    if let Some(progress) = progress {
        send_progress(channel, progress);
    }
}

fn send_cobs_flash_progress(
    channel: &Channel<ProgressDto>,
    event: CobsFlashEvent,
    total: usize,
    cancellable: &AtomicBool,
) {
    let total = total as u64;
    let progress = match event {
        CobsFlashEvent::Stage(stage) => {
            // Stage notifications are emitted after a complete request is on
            // the wire and while its ACK is pending. Cancellation is accepted
            // again only after an identity/progress event proves that command
            // reached an unambiguous boundary.
            cancellable.store(false, Ordering::SeqCst);
            let (stage, completed) = match stage {
                CobsFlashStage::Resetting => ("resetting", 0),
                CobsFlashStage::EnteringBootloader => ("entering_compatible_bootloader", 0),
                CobsFlashStage::IdentityVerified => {
                    cancellable.store(true, Ordering::SeqCst);
                    ("validating_compatible_identity", 0)
                }
                CobsFlashStage::StartingDownload => ("starting_download", 0),
                CobsFlashStage::Transferring => ("writing", 0),
                CobsFlashStage::Finalizing => ("finalizing", total),
                CobsFlashStage::Verifying => ("verifying", total),
            };
            Some(ProgressDto {
                stage,
                completed,
                total,
                cancellable: cancellable.load(Ordering::SeqCst),
            })
        }
        CobsFlashEvent::IapIdentity(_) => {
            cancellable.store(true, Ordering::SeqCst);
            Some(ProgressDto {
                stage: "validating_compatible_identity",
                completed: 0,
                total,
                cancellable: true,
            })
        }
        CobsFlashEvent::Progress { written, total } => {
            cancellable.store(true, Ordering::SeqCst);
            Some(ProgressDto {
                stage: "writing",
                completed: written as u64,
                total: total as u64,
                cancellable: true,
            })
        }
    };
    if let Some(progress) = progress {
        send_progress(channel, progress);
    }
}

async fn classify_target(
    sdo: &CanBusSdo<'_, dyn CanBus>,
    node_id: u8,
    identity: IdentitySnapshot,
    registry: &TargetRegistry,
    cobs_registry: &CobsTargetRegistry,
) -> DiscoveredTarget {
    match registry.classify(&identity) {
        TargetClassification::Enabled(target) => {
            match authorize(sdo, node_id, registry, IDENTITY_TIMEOUT).await {
                Ok(authorized) => {
                    let profile_id = target.profile_id().to_owned();
                    let dto = device_dto(
                        node_id,
                        *authorized.identity(),
                        Some(authorized.hardware_version()),
                        "enabled",
                        Some("stm32_canopen"),
                        Some(profile_id.clone()),
                        display_name_for_profile(&profile_id),
                        "Exact local product, hardware, MCU and firmware policy matched".into(),
                    );
                    DiscoveredTarget {
                        dto,
                        authorized: Some(SelectedTarget::Stm32(authorized)),
                    }
                }
                Err(error) => DiscoveredTarget {
                    dto: device_dto(
                        node_id,
                        identity,
                        None,
                        "known_disabled",
                        Some("stm32_canopen"),
                        Some(target.profile_id().to_owned()),
                        display_name_for_profile(target.profile_id()),
                        error.to_string(),
                    ),
                    authorized: None,
                },
            }
        }
        TargetClassification::Disabled(target) => {
            let reason = match target.support() {
                SupportPolicy::Disabled { reason } => reason.clone(),
                SupportPolicy::Enabled(_) => unreachable!("classification agrees with policy"),
            };
            DiscoveredTarget {
                dto: device_dto(
                    node_id,
                    identity,
                    None,
                    "known_disabled",
                    Some("stm32_canopen"),
                    Some(target.profile_id().to_owned()),
                    display_name_for_profile(target.profile_id()),
                    reason,
                ),
                authorized: None,
            }
        }
        TargetClassification::Unknown => classify_cobs_target(node_id, identity, cobs_registry),
        TargetClassification::Sentinel { field } => DiscoveredTarget {
            dto: device_dto(
                node_id,
                identity,
                None,
                "unsupported",
                None,
                None,
                None,
                format!("Identity field {field} is unprovisioned (0xFFFFFFFF)"),
            ),
            authorized: None,
        },
    }
}

fn classify_cobs_target(
    node_id: u8,
    identity: IdentitySnapshot,
    registry: &CobsTargetRegistry,
) -> DiscoveredTarget {
    let cobs_identity = match CobsCanopenIdentity::new(
        node_id,
        identity.vendor_id(),
        identity.product_code(),
        identity.revision_number(),
        identity.serial_number(),
    ) {
        Ok(identity) => identity,
        Err(error) => {
            return DiscoveredTarget {
                dto: device_dto(
                    node_id,
                    identity,
                    None,
                    "unsupported",
                    None,
                    None,
                    None,
                    error.to_string(),
                ),
                authorized: None,
            }
        }
    };

    match registry.classify(cobs_identity) {
        CobsTargetClassification::Enabled(target) => {
            let profile_id = target.profile_id().to_owned();
            match registry.authorize(cobs_identity) {
                Ok(authorized) => DiscoveredTarget {
                    dto: device_dto(
                        node_id,
                        identity,
                        None,
                        "enabled",
                        Some("cobs_can_iap_v1"),
                        Some(profile_id.clone()),
                        cobs_display_name_for_profile(&profile_id),
                        "Exact local CANopen identity selected a compatible IAP policy; IMG and Enter-IAP identity remain unverified"
                            .into(),
                    ),
                    authorized: Some(SelectedTarget::CobsIap(authorized)),
                },
                Err(error) => DiscoveredTarget {
                    dto: device_dto(
                        node_id,
                        identity,
                        None,
                        "known_disabled",
                        Some("cobs_can_iap_v1"),
                        Some(profile_id.clone()),
                        cobs_display_name_for_profile(&profile_id),
                        error.to_string(),
                    ),
                    authorized: None,
                },
            }
        }
        CobsTargetClassification::Disabled(target) => {
            let reason = match target.support() {
                CobsSupportPolicy::Disabled { reason } => reason.clone(),
                CobsSupportPolicy::Enabled(_) => unreachable!("classification agrees with policy"),
            };
            let profile_id = target.profile_id().to_owned();
            DiscoveredTarget {
                dto: device_dto(
                    node_id,
                    identity,
                    None,
                    "known_disabled",
                    Some("cobs_can_iap_v1"),
                    Some(profile_id.clone()),
                    cobs_display_name_for_profile(&profile_id),
                    reason,
                ),
                authorized: None,
            }
        }
        CobsTargetClassification::Unknown => DiscoveredTarget {
            dto: device_dto(
                node_id,
                identity,
                None,
                "unsupported",
                None,
                None,
                None,
                format!(
                    "No local update profile matches vendor 0x{:08X}, product 0x{:08X}",
                    identity.vendor_id(),
                    identity.product_code()
                ),
            ),
            authorized: None,
        },
    }
}

fn ensure_disjoint_backend_routes(
    standard: &TargetRegistry,
    compatible: &CobsTargetRegistry,
) -> CmdResult<()> {
    for first in standard.targets() {
        for second in compatible.targets() {
            if first.vendor_id() == second.vendor_id()
                && first.product_code() == second.product_code()
            {
                return Err(format!(
                    "CAN update registries overlap at vendor 0x{:08X}, product 0x{:08X}",
                    first.vendor_id(),
                    first.product_code()
                ));
            }
        }
    }
    Ok(())
}

fn device_dto(
    node_id: u8,
    identity: IdentitySnapshot,
    hardware_version: Option<u32>,
    authorization: &'static str,
    backend: Option<&'static str>,
    profile_id: Option<String>,
    display_name: Option<&'static str>,
    reason: String,
) -> DeviceDto {
    DeviceDto {
        node_id,
        node_id_hex: format!("0x{node_id:02X}"),
        device_name: None,
        vendor_id: identity.vendor_id(),
        vendor_id_hex: format!("0x{:08X}", identity.vendor_id()),
        product_code: identity.product_code(),
        product_code_hex: format!("0x{:08X}", identity.product_code()),
        software_revision: identity.revision_number(),
        software_revision_hex: format!("0x{:08X}", identity.revision_number()),
        serial_number: identity.serial_number(),
        serial_number_hex: format!("0x{:08X}", identity.serial_number()),
        hardware_version,
        hardware_version_hex: hardware_version.map(|value| format!("0x{value:08X}")),
        authorization,
        backend,
        profile_id,
        display_name,
        reason,
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn observe_heartbeat_nodes(bus: &dyn CanBus, window: Duration) -> Result<Vec<u8>> {
    let mut rx = bus
        .subscribe(CanFilter::standard(HEARTBEAT_BASE, HEARTBEAT_MASK))
        .await
        .map_err(|error| anyhow::anyhow!("subscribing to CANopen heartbeats: {error}"))?;
    let deadline = tokio::time::Instant::now() + window;
    let mut nodes = BTreeSet::new();
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout(deadline - now, rx.recv()).await {
            Ok(Ok(frame)) => {
                if let Some(node_id) = heartbeat_node_id(&frame) {
                    nodes.insert(node_id);
                    if nodes.len() > MAX_DISCOVERY_NODES {
                        bail!(
                            "more than {MAX_DISCOVERY_NODES} valid CANopen heartbeat nodes were observed; isolate the update bus"
                        );
                    }
                }
            }
            Ok(Err(error)) => bail!("receiving CANopen heartbeat: {error}"),
            Err(_) => break,
        }
    }
    Ok(nodes.into_iter().collect())
}

fn heartbeat_node_id(frame: &CanFrame) -> Option<u8> {
    if frame.is_fd()
        || frame.is_remote()
        || frame.data().len() != 1
        || !matches!(frame.data()[0], 0 | 4 | 5 | 127)
    {
        return None;
    }
    let CanId::Standard(id) = frame.id() else {
        return None;
    };
    (HEARTBEAT_BASE + 1..=HEARTBEAT_BASE + 0x7F)
        .contains(&id)
        .then_some((id - HEARTBEAT_BASE) as u8)
}

fn exact_tsdo_filter(filter: CanFilter) -> Option<(CanFilter, u16)> {
    if filter.extended || filter.mask != TSDO_FAMILY_MASK {
        return None;
    }
    if !(TSDO_BASE + 1..=TSDO_BASE + 0x7F).contains(&filter.id) {
        return None;
    }
    let expected = filter.id as u16;
    Some((CanFilter::exact_standard(expected), expected))
}

struct ExactSdoBus {
    inner: Arc<dyn CanBus>,
}

#[async_trait]
impl CanBus for ExactSdoBus {
    async fn send(&self, frame: CanFrame) -> std::result::Result<(), CanIoError> {
        self.inner.send(frame).await
    }

    async fn subscribe(
        &self,
        filter: CanFilter,
    ) -> std::result::Result<Box<dyn CanRx>, CanIoError> {
        let Some((exact, expected)) = exact_tsdo_filter(filter) else {
            return self.inner.subscribe(filter).await;
        };
        let inner = self.inner.subscribe(exact).await?;
        Ok(Box::new(ValidatedSdoRx { inner, expected }))
    }

    fn capabilities(&self) -> CanCapabilities {
        self.inner.capabilities()
    }

    async fn bus_state(&self) -> std::result::Result<Option<CanBusState>, CanIoError> {
        self.inner.bus_state().await
    }

    async fn link_config(&self) -> std::result::Result<Option<CanLinkConfig>, CanIoError> {
        self.inner.link_config().await
    }
}

struct ValidatedSdoRx {
    inner: Box<dyn CanRx>,
    expected: u16,
}

#[async_trait]
impl CanRx for ValidatedSdoRx {
    async fn recv(&mut self) -> std::result::Result<CanFrame, CanIoError> {
        loop {
            let frame = self.inner.recv().await?;
            if self.accepts(&frame) {
                return Ok(frame);
            }
            log::warn!(
                "discarding CAN DFU SDO frame from the wrong node: expected 0x{:03X}, got {:?}",
                self.expected,
                frame.id()
            );
        }
    }

    fn try_recv(&mut self) -> std::result::Result<Option<CanFrame>, CanIoError> {
        loop {
            let Some(frame) = self.inner.try_recv()? else {
                return Ok(None);
            };
            if self.accepts(&frame) {
                return Ok(Some(frame));
            }
            log::warn!(
                "discarding CAN DFU SDO frame from the wrong node: expected 0x{:03X}, got {:?}",
                self.expected,
                frame.id()
            );
        }
    }
}

impl ValidatedSdoRx {
    fn accepts(&self, frame: &CanFrame) -> bool {
        frame.id() == CanId::Standard(self.expected)
    }
}

fn with_exact_sdo_filter(bus: Arc<dyn CanBus>) -> Arc<dyn CanBus> {
    Arc::new(ExactSdoBus { inner: bus })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_accepted_only_before_the_start_boundary() {
        let state = CanDfuState::default();
        state.active.store(true, Ordering::SeqCst);
        state.cancellable.store(true, Ordering::SeqCst);
        assert!(!state.request_cancel());

        let token = CancellationToken::new();
        *state.cancellation.lock().unwrap() = Some(ActiveCancellation::Stm32(token.clone()));
        assert!(state.request_cancel());
        assert!(token.is_cancelled());

        state.cancellable.store(false, Ordering::SeqCst);
        assert!(!state.request_cancel());
    }

    #[test]
    fn exact_filter_only_narrows_valid_tsdo_nodes() {
        let broad = CanFilter::standard(0x594, TSDO_FAMILY_MASK as u16);
        let (exact, expected) = exact_tsdo_filter(broad).expect("TSDO filter");
        assert_eq!(expected, 0x594);
        assert_eq!(exact, CanFilter::exact_standard(0x594));
        assert!(exact_tsdo_filter(CanFilter::standard(0x580, 0x780)).is_none());
    }

    #[test]
    fn heartbeat_candidates_require_classic_one_byte_nmt_state() {
        let valid = CanFrame::new_data(0x714u16, &[5]).unwrap();
        assert_eq!(heartbeat_node_id(&valid), Some(0x14));

        assert!(heartbeat_node_id(&CanFrame::new_data(0x714u16, &[]).unwrap()).is_none());
        assert!(heartbeat_node_id(&CanFrame::new_data(0x714u16, &[6]).unwrap()).is_none());
        assert!(heartbeat_node_id(&CanFrame::new_fd(0x714u16, &[5], false).unwrap()).is_none());
        assert!(heartbeat_node_id(&CanFrame::new_data(0x700u16, &[0]).unwrap()).is_none());
    }

    #[test]
    fn enabled_backend_registries_are_disjoint() {
        let standard = target_registry().unwrap();
        let compatible = cobs_target_registry().unwrap();
        ensure_disjoint_backend_routes(&standard, &compatible).unwrap();
    }

    #[test]
    fn changed_identity_is_rejected_before_factory_backup() {
        let expected = CobsCanopenIdentity::new(1, 0x0068_6578, 0x6C64_BC78, 1, 42).unwrap();
        assert!(ensure_cobs_identity_unchanged(expected, expected).is_ok());

        for changed in [
            CobsCanopenIdentity::new(1, 0x4859_444C, 0xAAAA_0001, 1, 42).unwrap(),
            CobsCanopenIdentity::new(1, 0x0068_6578, 0x6C64_BC78, 2, 42).unwrap(),
            CobsCanopenIdentity::new(1, 0x0068_6578, 0x6C64_BC78, 1, 43).unwrap(),
        ] {
            assert!(ensure_cobs_identity_unchanged(expected, changed).is_err());
        }
    }

    #[test]
    fn meowpkg_revision_floor_rejects_older_but_allows_reinstall_and_upgrade() {
        let installed = 0x0001_0002;
        let error = revision_warning(RevisionEvidence::StandardMeowpkg(0x0001_0001), installed)
            .unwrap_err();
        assert!(error.contains(".meowpkg manifest firmware_version"));
        assert!(error.contains("0x1018:03"));
        assert!(error.contains("downgrade is refused"));
        assert_eq!(
            revision_warning(RevisionEvidence::StandardMeowpkg(installed), installed).unwrap(),
            "reinstall"
        );
        assert_eq!(
            revision_warning(RevisionEvidence::StandardMeowpkg(0x0001_0003), installed).unwrap(),
            "none"
        );
    }

    #[test]
    fn signed_motor_release_revision_floor_rejects_only_older_targets() {
        let installed = 0x6578_0002;
        let error = revision_warning(RevisionEvidence::SignedMotorRelease(0x6578_0001), installed)
            .unwrap_err();
        assert!(error.contains("signed Motor IMG expected_postflash.revision_number"));
        assert!(error.contains("0x65780001"));
        assert!(error.contains("0x65780002"));
        assert_eq!(
            revision_warning(RevisionEvidence::SignedMotorRelease(installed), installed).unwrap(),
            "reinstall"
        );
        assert_eq!(
            revision_warning(RevisionEvidence::SignedMotorRelease(0x6578_0003), installed).unwrap(),
            "none"
        );
    }

    #[test]
    fn local_motor_img_does_not_compare_unproven_header_version_to_1018() {
        assert_eq!(
            revision_warning(RevisionEvidence::UnprovenLocalMotorImg, 0xFFFF_FFFE).unwrap(),
            "unknown"
        );
    }

    #[test]
    fn r2_https_client_uses_the_process_crypto_provider() {
        // Building the client catches a missing or ambiguous process-wide
        // provider without making a network-dependent test.
        r2_client().expect("fixed HTTPS client");
    }

    const TEST_PROFILE: &str = "lift-g0b1-v1";
    const TEST_VENDOR: u32 = 0x6865_786D;
    const TEST_PRODUCT: u32 = 0x006C_6674;
    const RELEASE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ARTIFACT_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn latest_bytes(path: &str, version: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "format": LATEST_FORMAT,
            "profile_id": TEST_PROFILE,
            "vendor_id": "0x6865786D",
            "product_code": "0x006C6674",
            "channel": R2_CHANNEL,
            "version": version,
            "release": {
                "path": path,
                "sha256": RELEASE_SHA,
                "bytes": 321
            }
        }))
        .unwrap()
    }

    fn release_bytes(path: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "format": RELEASE_FORMAT,
            "profile_id": TEST_PROFILE,
            "vendor_id": "0x6865786D",
            "product_code": "0x006C6674",
            "version": "1.2.0",
            "tag": "v1.2.0",
            "candidate_source_commit":
                "cccccccccccccccccccccccccccccccccccccccc",
            "artifact": {
                "path": path,
                "sha256": ARTIFACT_SHA,
                "bytes": 1024
            }
        }))
        .unwrap()
    }

    #[test]
    fn online_documents_accept_only_the_literal_identity_and_canonical_paths() {
        let latest = parse_latest_document(
            &latest_bytes("1.2.0/release.json", "1.2.0"),
            TEST_PROFILE,
            TEST_VENDOR,
            TEST_PRODUCT,
        )
        .unwrap();
        assert_eq!(latest.version, "1.2.0");
        assert_eq!(latest.firmware_version, 0x0001_0002);

        let release = parse_release_document(
            &release_bytes(&format!("{ARTIFACT_SHA}.meowpkg")),
            &latest,
            TEST_PROFILE,
            TEST_VENDOR,
            TEST_PRODUCT,
        )
        .unwrap();
        assert_eq!(release.artifact_path, format!("{ARTIFACT_SHA}.meowpkg"));
        assert_eq!(release.artifact_bytes, 1024);
    }

    #[test]
    fn latest_rejects_noncanonical_version_traversal_and_unknown_fields() {
        assert!(parse_latest_document(
            &latest_bytes("01.2.0/release.json", "01.2.0"),
            TEST_PROFILE,
            TEST_VENDOR,
            TEST_PRODUCT,
        )
        .is_err());
        assert!(parse_latest_document(
            &latest_bytes("../1.2.0/release.json", "1.2.0"),
            TEST_PROFILE,
            TEST_VENDOR,
            TEST_PRODUCT,
        )
        .is_err());

        let mut document: serde_json::Value =
            serde_json::from_slice(&latest_bytes("1.2.0/release.json", "1.2.0")).unwrap();
        document["url"] = serde_json::Value::String("https://example.invalid/fw".into());
        assert!(parse_latest_document(
            &serde_json::to_vec(&document).unwrap(),
            TEST_PROFILE,
            TEST_VENDOR,
            TEST_PRODUCT,
        )
        .is_err());
    }

    #[test]
    fn release_rejects_nested_or_sha_unbound_artifact_names() {
        let latest = parse_latest_document(
            &latest_bytes("1.2.0/release.json", "1.2.0"),
            TEST_PROFILE,
            TEST_VENDOR,
            TEST_PRODUCT,
        )
        .unwrap();
        for path in [
            format!("nested/{ARTIFACT_SHA}.meowpkg"),
            format!("{RELEASE_SHA}.meowpkg"),
            format!("{ARTIFACT_SHA}.bin"),
        ] {
            assert!(parse_release_document(
                &release_bytes(&path),
                &latest,
                TEST_PROFILE,
                TEST_VENDOR,
                TEST_PRODUCT,
            )
            .is_err());
        }
    }

    #[test]
    fn bound_download_requires_exact_size_and_sha256() {
        let bytes = b"exact release bytes";
        let sha = hex_sha256(bytes);
        verify_bound_bytes("test", bytes, bytes.len(), &sha).unwrap();
        assert!(verify_bound_bytes("test", bytes, bytes.len() + 1, &sha).is_err());
        assert!(verify_bound_bytes("test", bytes, bytes.len(), RELEASE_SHA).is_err());
    }
}
