//! Tauri adapter for the fail-closed STM32 CANopen DFU core.
//!
//! The GUI exposes read-only heartbeat discovery, exact local target
//! classification, and bounded `.meowpkg` preflight. The common streaming
//! engine is wired behind the core's opaque `ReadyToFlash` gate, but no product
//! is enabled until its hardware→MCU and firmware-id mapping is frozen.
//! Therefore no current discovery session can reach an SDO download.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use can_transport::{
    CanBus, CanBusState, CanCapabilities, CanFilter, CanFrame, CanId, CanIoError, CanRx,
};
use hexmeow_stm32_can_dfu::{
    authorize, flash, observe_identity, read_package_bytes, revalidate_prepared, AuthorizedTarget,
    CanBusSdo, CancellationToken, FlashError, FlashEvent, FlashOptions, FlashStage,
    IdentitySnapshot, PackageLimits, PreparedUpgrade, RegisteredTarget, Stm32ImageMode,
    SupportPolicy, TargetClassification, TargetRegistry,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::State;

use crate::dfu_gate::{DfuBackend, DfuMutationGate};

type CmdResult<T> = std::result::Result<T, String>;

const DISCOVERY_WINDOW: Duration = Duration::from_millis(2_500);
const IDENTITY_TIMEOUT: Duration = Duration::from_millis(750);
const VENDOR_ID: u32 = 0x6865_786D;
const HEARTBEAT_BASE: u16 = 0x700;
const HEARTBEAT_MASK: u16 = 0x780;
const MAX_DISCOVERY_NODES: usize = 32;
const MAX_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;
const TSDO_BASE: u32 = 0x580;
const TSDO_FAMILY_MASK: u32 = 0x780;

#[derive(Default)]
struct CanDfuInner {
    spec: Option<String>,
    discovered: HashMap<u8, DiscoveredTarget>,
    selected: Option<AuthorizedTarget>,
    staged: Option<StagedArtifact>,
}

#[derive(Clone)]
struct DiscoveredTarget {
    dto: DeviceDto,
    authorized: Option<AuthorizedTarget>,
}

struct StagedArtifact {
    token: String,
    prepared: PreparedUpgrade,
}

pub struct CanDfuState {
    inner: Mutex<CanDfuInner>,
    session_lock: tokio::sync::Mutex<()>,
    active: AtomicBool,
    cancellation: Mutex<Option<CancellationToken>>,
}

impl Default for CanDfuState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(CanDfuInner::default()),
            session_lock: tokio::sync::Mutex::new(()),
            active: AtomicBool::new(false),
            cancellation: Mutex::new(None),
        }
    }
}

impl CanDfuState {
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
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
    artifact_sha256: String,
    artifact_size: usize,
    mcu: String,
    format_version: u16,
    encrypted: bool,
    firmware_id: u32,
    firmware_id_hex: String,
    firmware_version: u32,
    firmware_version_hex: String,
    plaintext_size: usize,
    wire_size: usize,
    version_warning: &'static str,
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
}

/// Read-only CAN discovery. The bus is dropped before this command returns.
#[tauri::command]
pub async fn stm32_can_dfu_discover(
    state: State<'_, CanDfuState>,
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
    *state.inner.lock().unwrap() = CanDfuInner::default();

    let bus = open_classic_bus(&spec)
        .await
        .map_err(|error| error.to_string())?;
    let nodes = observe_heartbeat_nodes(bus.as_ref(), DISCOVERY_WINDOW)
        .await
        .map_err(|error| error.to_string())?;
    let registry = target_registry().map_err(|error| error.to_string())?;
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
        let record = classify_target(&sdo, node_id, identity, &registry).await;
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
    let (target, device) = {
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
        (target, device)
    };

    let source_size = bytes.len();
    let source_sha256 = hex_sha256(&bytes);
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
    let prepared = PreparedUpgrade::bind(target, package).map_err(|error| error.to_string())?;
    let summary = prepared.package();
    let manifest = summary.manifest();
    let format_version = manifest
        .payload_format
        .as_ref()
        .and_then(|format| format.stm32_header_version)
        .ok_or_else(|| "validated STM32 package has no header version".to_owned())?;
    let encrypted = matches!(summary.image_mode(), Stm32ImageMode::EncryptedV2);
    // PreparedUpgrade currently admits plaintext v1 only. When secure v2
    // catalog authorization lands, its descriptor must expose the authenticated
    // plaintext length rather than reusing the encrypted wire length here.
    debug_assert!(matches!(summary.image_mode(), Stm32ImageMode::PlaintextV1));
    let plaintext_size = summary.image().len();
    // 0x1018:03 belongs to the currently responding endpoint. Until an
    // enabled profile classifies 0x1008 as APP versus Bootloader, do not label
    // the package as an upgrade/reinstall/downgrade.
    let version_warning = "unknown";
    let token = format!(
        "{:016x}",
        getrandom::u64().map_err(|error| format!("cannot create artifact token: {error}"))?
    );
    let dto = PreparedDto {
        token: token.clone(),
        device,
        artifact_sha256: source_sha256,
        artifact_size: source_size,
        mcu: manifest.mcu.clone(),
        format_version,
        encrypted,
        firmware_id: manifest.firmware_id,
        firmware_id_hex: format!("0x{:08X}", manifest.firmware_id),
        firmware_version: manifest.firmware_version,
        firmware_version_hex: format!("0x{:08X}", manifest.firmware_version),
        plaintext_size,
        wire_size: aligned_wire_size(summary.image().len()),
        version_warning,
    };
    state.inner.lock().unwrap().staged = Some(StagedArtifact { token, prepared });
    Ok(dto)
}

struct ActiveReset<'a> {
    active: &'a AtomicBool,
    cancellation: &'a Mutex<Option<CancellationToken>>,
}

impl Drop for ActiveReset<'_> {
    fn drop(&mut self) {
        *self.cancellation.lock().unwrap() = None;
        self.active.store(false, Ordering::SeqCst);
    }
}

/// Mutation entry point shared by future qualified STM32 products.
///
/// The current registry contains no enabled profile, so neither the GUI nor a
/// direct IPC caller can create `PreparedUpgrade` and reach this command with a
/// valid token. Keeping the complete path wired lets qualification focus on
/// the finite product/hardware/MCU/firmware mapping instead of copying the old
/// CLI state machine. Before enabling a profile, this command must also join
/// the application's global CAN transport lease so Analyzer/general sessions
/// cannot own the same physical adapter concurrently.
#[tauri::command]
pub async fn stm32_can_dfu_start(
    state: State<'_, CanDfuState>,
    mutation_gate: State<'_, DfuMutationGate>,
    token: String,
    on_event: Channel<ProgressDto>,
) -> CmdResult<OutcomeDto> {
    let _session = state.session_lock.lock().await;
    if state.is_active() {
        return Err("an upgrade is already running".into());
    }
    let (spec, prepared) = {
        let inner = state.inner.lock().unwrap();
        let staged = inner
            .staged
            .as_ref()
            .ok_or_else(|| "no validated STM32 CAN artifact is staged".to_owned())?;
        if staged.token != token {
            return Err("artifact token is stale; select and validate the file again".into());
        }
        let spec = inner
            .spec
            .clone()
            .ok_or_else(|| "the CAN discovery session is stale".to_owned())?;
        (spec, staged.prepared.clone())
    };

    let _mutation_permit = mutation_gate
        .try_acquire(DfuBackend::Stm32Can)
        .map_err(str::to_owned)?;
    let cancellation = CancellationToken::new();
    *state.cancellation.lock().unwrap() = Some(cancellation.clone());
    if state
        .active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        *state.cancellation.lock().unwrap() = None;
        return Err("an upgrade is already running".to_owned());
    }
    let _active_reset = ActiveReset {
        active: &state.active,
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
    let bus = open_classic_bus(&spec)
        .await
        .map_err(|error| error.to_string())?;
    let registry = target_registry().map_err(|error| error.to_string())?;
    let sdo = CanBusSdo::new(bus.as_ref());
    let ready = revalidate_prepared(&sdo, &prepared, &registry, IDENTITY_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    let wire_total = aligned_wire_size(ready.package().image().len());

    let result = flash(
        &sdo,
        ready,
        &FlashOptions::default(),
        &cancellation,
        |event| send_flash_progress(&on_event, event, wire_total),
    )
    .await;

    match result {
        Ok(_) => Ok(OutcomeDto {
            status: "application_verified",
            startup_confirmed: true,
            recoverable_bootloader_expected: false,
        }),
        Err(FlashError::Cancelled {
            stage: FlashStage::Claiming,
        }) => Ok(OutcomeDto {
            status: "cancelled_before_write",
            startup_confirmed: false,
            recoverable_bootloader_expected: false,
        }),
        Err(FlashError::Cancelled { .. }) => Ok(OutcomeDto {
            status: "cancelled_recoverable",
            startup_confirmed: false,
            recoverable_bootloader_expected: true,
        }),
        Err(error) => Err(format!(
            "{error}. If STOP/CLEAR/data had started, keep the device powered in Bootloader and run one complete upgrade again"
        )),
    }
}

#[tauri::command]
pub fn stm32_can_dfu_cancel(state: State<'_, CanDfuState>) -> bool {
    let active = state.is_active();
    if active {
        if let Some(cancellation) = state.cancellation.lock().unwrap().as_ref() {
            cancellation.cancel();
        }
    }
    active
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

fn aligned_wire_size(size: usize) -> usize {
    let remainder = size % 8;
    if remainder == 0 {
        size
    } else {
        size + 8 - remainder
    }
}

fn send_progress(channel: &Channel<ProgressDto>, progress: ProgressDto) {
    // Losing the WebView/channel must not interrupt an erase or leave START in
    // an ambiguous state. The backend continues to a protocol terminal state.
    let _ = channel.send(progress);
}

fn send_flash_progress(channel: &Channel<ProgressDto>, event: FlashEvent, total: usize) {
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

async fn classify_target(
    sdo: &CanBusSdo<'_, dyn CanBus>,
    node_id: u8,
    identity: IdentitySnapshot,
    registry: &TargetRegistry,
) -> DiscoveredTarget {
    match registry.classify(&identity) {
        TargetClassification::Enabled(target) => {
            match authorize(sdo, node_id, registry, IDENTITY_TIMEOUT).await {
                Ok(authorized) => {
                    let dto = device_dto(
                        node_id,
                        *authorized.identity(),
                        Some(authorized.hardware_version()),
                        "enabled",
                        Some(target),
                        "Exact local product, hardware, MCU and firmware policy matched".into(),
                    );
                    DiscoveredTarget {
                        dto,
                        authorized: Some(authorized),
                    }
                }
                Err(error) => DiscoveredTarget {
                    dto: device_dto(
                        node_id,
                        identity,
                        None,
                        "known_disabled",
                        Some(target),
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
                    Some(target),
                    reason,
                ),
                authorized: None,
            }
        }
        TargetClassification::Unknown => DiscoveredTarget {
            dto: device_dto(
                node_id,
                identity,
                None,
                "unsupported",
                None,
                format!(
                    "No local profile matches vendor 0x{:08X}, product 0x{:08X}",
                    identity.vendor_id(),
                    identity.product_code()
                ),
            ),
            authorized: None,
        },
        TargetClassification::Sentinel { field } => DiscoveredTarget {
            dto: device_dto(
                node_id,
                identity,
                None,
                "unsupported",
                None,
                format!("Identity field {field} is unprovisioned (0xFFFFFFFF)"),
            ),
            authorized: None,
        },
    }
}

fn device_dto(
    node_id: u8,
    identity: IdentitySnapshot,
    hardware_version: Option<u32>,
    authorization: &'static str,
    target: Option<&RegisteredTarget>,
    reason: String,
) -> DeviceDto {
    let profile_id = target.map(|target| target.profile_id().to_owned());
    let display_name = profile_id.as_deref().and_then(display_name_for_profile);
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
        profile_id,
        display_name,
        reason,
    }
}

fn display_name_for_profile(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        "imu-g4-bench" => Some("IMU bench / demo"),
        "arm-imu" => Some("Arm IMU"),
        "lift-g0b1" => Some("Lift controller"),
        _ => None,
    }
}

fn target_registry() -> Result<TargetRegistry> {
    Ok(TargetRegistry::new(vec![
        RegisteredTarget::disabled(
            "imu-g4-bench",
            VENDOR_ID,
            0x0069_6D75,
            "Provisioned bench product, but hardware_version → G431/G474 and firmware_id mappings are not frozen",
        )?,
        RegisteredTarget::disabled(
            "arm-imu",
            VENDOR_ID,
            0x6169_6D75,
            "Product code is allocated but not yet provisioned or update-qualified",
        )?,
        RegisteredTarget::disabled(
            "lift-g0b1",
            VENDOR_ID,
            0x006C_6674,
            "Product code is allocated but not yet provisioned; production security and release policy are not qualified",
        )?,
    ])?)
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

async fn open_classic_bus(spec: &str) -> Result<Arc<dyn CanBus>> {
    if let Some(channel) = gs_usb_channel(spec) {
        use can_transport::gs_usb::{GsUsbBus, GsUsbConfig};
        let bus = GsUsbBus::open(GsUsbConfig::classic_1m().with_channel(channel))
            .await
            .with_context(|| format!("opening gs_usb / candleLight channel {channel}"))?;
        log::info!(
            "STM32 DFU opened gs_usb ch{channel} in classic 1 Mbit mode: {:?}",
            bus.capabilities()
        );
        return Ok(with_exact_sdo_filter(Arc::new(bus)));
    }

    let (kind, name) = match spec.split_once(':') {
        Some((kind, name)) => (kind, name),
        None => ("socketcan", spec),
    };
    match kind {
        #[cfg(target_os = "linux")]
        "socketcan" => {
            let bus = can_transport::socketcan::SocketCanBus::open(name)
                .with_context(|| format!("opening SocketCAN interface {name:?}"))?;
            ensure_socketcan_up(&bus, name).await?;
            Ok(with_exact_sdo_filter(Arc::new(bus)))
        }
        other => bail!(
            "backend {other:?} is unavailable for STM32 DFU (use socketcan on Linux or gs_usb<channel>)"
        ),
    }
}

#[cfg(target_os = "linux")]
async fn ensure_socketcan_up(bus: &dyn CanBus, name: &str) -> Result<()> {
    use can_transport::CanControllerState;
    match bus.bus_state().await {
        Ok(Some(CanBusState {
            state: Some(CanControllerState::Stopped),
            ..
        })) => bail!(
            "SocketCAN interface {name:?} is down; run `sudo ip link set dev {name} up` first"
        ),
        Ok(_) => Ok(()),
        Err(error) => {
            log::warn!("could not query SocketCAN interface {name:?} state; continuing: {error}");
            Ok(())
        }
    }
}

fn gs_usb_channel(spec: &str) -> Option<u16> {
    let spec = spec.trim().to_ascii_lowercase();
    let rest = spec
        .strip_prefix("gs_usb")
        .or_else(|| spec.strip_prefix("gsusb"))?;
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    if rest.is_empty() {
        Some(0)
    } else {
        rest.parse().ok()
    }
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
                "discarding STM32 DFU SDO frame from the wrong node: expected 0x{:03X}, got {:?}",
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
                "discarding STM32 DFU SDO frame from the wrong node: expected 0x{:03X}, got {:?}",
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
    fn registry_contains_only_exact_disabled_product_rows() {
        let registry = target_registry().expect("valid static registry");
        assert_eq!(registry.targets().len(), 3);
        assert!(registry.targets().iter().all(|target| {
            target.vendor_id() == VENDOR_ID
                && matches!(target.support(), SupportPolicy::Disabled { .. })
        }));
    }

    #[test]
    fn parser_accepts_only_explicit_gs_usb_spellings() {
        assert_eq!(gs_usb_channel("gs_usb"), Some(0));
        assert_eq!(gs_usb_channel("gs_usb1"), Some(1));
        assert_eq!(gs_usb_channel("gsusb:2"), Some(2));
        assert_eq!(gs_usb_channel("can0"), None);
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
}
