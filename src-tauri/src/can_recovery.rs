//! Attended CAN recovery. A native model selection precedes any active probe.
use crate::can_lease::{CanOwner, CanTransportGate};
use crate::dfu_gate::{DfuBackend, DfuMutationGate};
use can_transport::CanBus;
use hexmeow_dfu_targets::{PreflightPolicy, TargetSupport};
use hexmeow_motor_iap::{
    AssertedTarget, CancellationToken, FlashEvent, FlashStage, IapIdentity, ImgArtifact, ImgLimits,
};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tauri::{
    ipc::{Channel, InvokeBody, Request},
    State,
};
type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Serialize)]
pub struct Profile {
    pub id: String,
    pub family: &'static str,
    pub name: String,
    pub description: String,
    pub image: &'static str,
    pub available: bool,
    pub reason: Option<String>,
    pub vendor: u32,
    pub product: u32,
}
fn profiles() -> Vec<Profile> {
    let mut out = vec![Profile {
        id: crate::hpm_can_profiles::EVK_PROFILE.into(),
        family: "hpm",
        name: "HPM5300EVK / HPM V3".into(),
        description: "HPM5300EVK；签名 .hpmota/3；无地址时可显式 LSS 分配。IMU 产品身份尚未登记。"
            .into(),
        image: "/recovery/hpm-evk.png",
        available: true,
        reason: None,
        vendor: crate::hpm_can_profiles::VENDOR,
        product: crate::hpm_can_profiles::EVK_PRODUCT,
    }];
    for t in hexmeow_dfu_targets::TARGETS {
        let (family, image) = match t.backend() {
            hexmeow_dfu_targets::TargetBackend::StandardMeowpkg => {
                ("stm32", "/recovery/stm32-board.svg")
            }
            _ => ("motor_iap", "/recovery/hex-4310.jpg"),
        };
        let available = t.is_enabled() && t.preflight == PreflightPolicy::None;
        let reason = if t.preflight != PreflightPolicy::None {
            Some("This Meow Motor requires a verified factory-calibration backup; generic identity-less recovery is unavailable.".into())
        } else {
            match t.support {
                TargetSupport::Disabled { reason, .. } => Some(reason.into()),
                _ => None,
            }
        };
        out.push(Profile {
            id: t.profile_id.into(),
            family,
            name: t.display_name.into(),
            description: format!(
                "Vendor 0x{:08X} / Product 0x{:08X}",
                t.vendor_id, t.product_code
            ),
            image,
            available,
            reason,
            vendor: t.vendor_id,
            product: t.product_code,
        });
    }
    out
}
#[derive(Default)]
struct Inner {
    profile: Option<Profile>,
    token: String,
    bus: Option<Arc<dyn CanBus>>,
    hits: Vec<Hit>,
    selected: Option<u8>,
    staged: Option<(String, AssertedTarget, Prepared)>,
}
pub struct RecoveryState {
    inner: Mutex<Inner>,
    operation: tokio::sync::Mutex<()>,
    active: AtomicBool,
    cancel: AtomicBool,
    flash_cancel: Mutex<CancellationToken>,
}
impl Default for RecoveryState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            operation: tokio::sync::Mutex::new(()),
            active: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            flash_cancel: Mutex::new(CancellationToken::new()),
        }
    }
}
impl RecoveryState {
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}
struct Active<'a>(&'a AtomicBool);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
#[derive(Clone, Serialize)]
pub struct Hit {
    pub node_id: u8,
    pub device_id: Option<u32>,
    pub firmware_id: Option<u32>,
    pub version: Option<u32>,
    pub selectable: bool,
    pub reason: String,
}
#[derive(Clone, Serialize)]
pub struct Prepared {
    pub token: String,
    pub node_id: u8,
    pub model: String,
    pub sha256: String,
    pub bytes: usize,
    pub wire_bytes: usize,
    pub device_id: u32,
    pub firmware_id: u32,
    pub firmware_version: u32,
}
#[derive(Clone, Serialize)]
pub struct Progress {
    pub stage: &'static str,
    pub completed: usize,
    pub total: usize,
    pub cancellable: bool,
}
fn selected_profile(state: &RecoveryState) -> Result<Profile> {
    let i = state.inner.lock().unwrap();
    i.profile
        .clone()
        .ok_or_else(|| "explicitly choose and confirm a recovery model first".into())
}
fn motor_target(profile: &Profile) -> Result<hexmeow_motor_iap::RegisteredTarget> {
    if profile.family != "motor_iap" || !profile.available {
        return Err("the chosen model does not permit Motor-IAP recovery".into());
    }
    let registry = crate::cobs_can_iap_profiles::target_registry().map_err(|e| e.to_string())?;
    registry
        .targets()
        .iter()
        .find(|t| t.profile_id() == profile.id)
        .cloned()
        .ok_or_else(|| "recovery model is not registered".into())
}
pub(crate) fn require_hpm(
    state: &RecoveryState,
    token: Option<&str>,
    vendor: u32,
    product: u32,
) -> Result<()> {
    let i = state.inner.lock().unwrap();
    let p = i
        .profile
        .as_ref()
        .ok_or("confirm the recovery device before LSS/capture")?;
    if p.family != "hpm"
        || !p.available
        || token != Some(i.token.as_str())
        || vendor != p.vendor
        || product != p.product
    {
        return Err("recovery model/token does not authorize this HPM target".into());
    }
    Ok(())
}
#[tauri::command]
pub fn can_recovery_profiles() -> Vec<Profile> {
    profiles()
}
#[derive(Serialize)]
pub struct Selection {
    pub profile: Profile,
    pub token: String,
}
#[tauri::command]
pub async fn can_recovery_begin(
    state: State<'_, RecoveryState>,
    gate: State<'_, DfuMutationGate>,
    profile_id: String,
) -> Result<Selection> {
    let _op = state.operation.lock().await;
    if state.is_active() || gate.is_active() {
        return Err("wait for the active update/capture first".into());
    }
    let p = profiles()
        .into_iter()
        .find(|p| p.id == profile_id && p.available)
        .ok_or("unknown or unavailable recovery model")?;
    let token = format!("{:016x}", getrandom::u64().map_err(|e| e.to_string())?);
    *state.inner.lock().unwrap() = Inner {
        profile: Some(p.clone()),
        token: token.clone(),
        ..Default::default()
    };
    Ok(Selection { profile: p, token })
}
#[tauri::command]
pub async fn can_recovery_scan(
    state: State<'_, RecoveryState>,
    gate: State<'_, DfuMutationGate>,
    can_gate: State<'_, CanTransportGate>,
    spec: String,
) -> Result<Vec<Hit>> {
    let _op = state.operation.lock().await;
    let profile = selected_profile(&state)?;
    let target = motor_target(&profile)?;
    let _permit = gate
        .try_acquire(DfuBackend::CobsCanIap)
        .map_err(str::to_owned)?;
    state.cancel.store(false, Ordering::SeqCst);
    state.active.store(true, Ordering::SeqCst);
    let _active = Active(&state.active);
    {
        let mut i = state.inner.lock().unwrap();
        i.bus = None;
        i.hits.clear();
        i.selected = None;
        i.staged = None;
    }
    let lease = can_gate.try_acquire(CanOwner::DfuUpdate)?;
    let bus = crate::backend::open_classic_1m_bus(spec.trim(), lease)
        .await
        .map_err(|e| e.to_string())?;
    let mut heartbeats = bus
        .subscribe(can_transport::CanFilter::standard(0x700, 0x780))
        .await
        .map_err(|e| e.to_string())?;
    let end = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        while let Some(frame) = heartbeats.try_recv().map_err(|e| e.to_string())? {
            if frame.kind() == can_transport::FrameKind::Data && frame.data().len() == 1 {
                if let can_transport::CanId::Standard(id) = frame.id() {
                    let node = (id & 127) as u8;
                    if crate::can_node_policy::RESERVED.contains(&node) {
                        return Err(crate::can_node_policy::check(node).unwrap_err());
                    }
                }
            }
        }
        if state.cancel.load(Ordering::SeqCst) {
            return Err("recovery capture cancelled".into());
        }
        // 100/101 would send on LSS's 0x7E4/0x7E5. Never probe them.
        let report = hexmeow_motor_iap::scan_for_bootloaders(
            bus.as_ref(),
            (1..=127).filter(|id| crate::can_node_policy::check(*id).is_ok()),
            Duration::from_millis(150),
        )
        .await
        .map_err(|e| e.to_string())?;
        if !report.hits().is_empty() {
            let mut hits = Vec::new();
            for h in report.hits() {
                let identity = h.identity();
                let matched = identity.is_some_and(|id| iap_matches(&target, id));
                let allowed = crate::can_node_policy::check(h.node_id()).is_ok();
                let reason = if !allowed {
                    crate::can_node_policy::check(h.node_id()).unwrap_err()
                } else if identity.is_none() {
                    "All-FF/malformed IAP identity: this device cannot be safely matched to the selected model".into()
                } else if !matched {
                    "IAP identity does not match the explicitly selected model".into()
                } else {
                    "Model matches; original CANopen serial identity is unavailable in recovery"
                        .into()
                };
                hits.push(Hit {
                    node_id: h.node_id(),
                    device_id: identity.map(|i| i.device_id()),
                    firmware_id: identity.map(|i| i.firmware_id()),
                    version: identity.map(|i| i.current_version()),
                    selectable: matched && allowed,
                    reason,
                });
            }
            let mut i = state.inner.lock().unwrap();
            i.bus = Some(bus);
            i.hits = hits.clone();
            return Ok(hits);
        }
        if tokio::time::Instant::now() >= end {
            return Err("no matching IAP response; power-cycle the isolated target after starting recovery capture".into());
        }
    }
}
fn iap_matches(target: &hexmeow_motor_iap::RegisteredTarget, id: IapIdentity) -> bool {
    let hexmeow_motor_iap::SupportPolicy::Enabled(p) = target.support() else {
        return false;
    };
    id.device_id() == p.device_id() && p.firmware_ids().contains(&id.firmware_id())
}
#[tauri::command]
pub async fn can_recovery_select(state: State<'_, RecoveryState>, node_id: u8) -> Result<()> {
    crate::can_node_policy::check(node_id)?;
    let _op = state.operation.lock().await;
    let mut i = state.inner.lock().unwrap();
    if !i.hits.iter().any(|h| h.node_id == node_id && h.selectable) {
        return Err("select a matching device from this recovery scan".into());
    }
    i.selected = Some(node_id);
    i.staged = None;
    Ok(())
}
#[tauri::command]
pub async fn can_recovery_prepare(
    request: Request<'_>,
    state: State<'_, RecoveryState>,
) -> Result<Prepared> {
    let _op = state.operation.lock().await;
    let p = selected_profile(&state)?;
    let target = motor_target(&p)?;
    let bytes = match request.body() {
        InvokeBody::Raw(b) if b.len() <= 2 * 1024 * 1024 => b,
        _ => return Err("expected a raw IMG file, at most 2 MiB".into()),
    };
    let mut i = state.inner.lock().unwrap();
    i.staged = None;
    let node = i.selected.ok_or("select a captured IAP device first")?;
    let artifact = ImgArtifact::parse(
        bytes,
        ImgLimits {
            max_file_bytes: 2 * 1024 * 1024,
            max_bin_bytes: 2 * 1024 * 1024 - hexmeow_motor_iap::IMG_TAG_SIZE,
        },
    )
    .map_err(|e| e.to_string())?;
    let policy = AssertedTarget::assert(&target, artifact).map_err(|e| e.to_string())?;
    let a = policy.artifact();
    let token = format!("{:016x}", getrandom::u64().map_err(|e| e.to_string())?);
    let dto = Prepared {
        token: token.clone(),
        node_id: node,
        model: p.name,
        sha256: crate::dfu_release::sha256_hex(bytes),
        bytes: bytes.len(),
        wire_bytes: a.bin_size(),
        device_id: a.device_id(),
        firmware_id: a.firmware_id(),
        firmware_version: a.firmware_version(),
    };
    i.staged = Some((token, policy, dto.clone()));
    Ok(dto)
}
#[tauri::command]
pub async fn can_recovery_start(
    state: State<'_, RecoveryState>,
    gate: State<'_, DfuMutationGate>,
    token: String,
    on_event: Channel<Progress>,
) -> Result<&'static str> {
    let _op = state.operation.lock().await;
    let p = selected_profile(&state)?;
    motor_target(&p)?;
    let _permit = gate
        .try_acquire(DfuBackend::CobsCanIap)
        .map_err(str::to_owned)?;
    let (bus, policy, node) = {
        let mut i = state.inner.lock().unwrap();
        if !i.staged.as_ref().is_some_and(|s| s.0 == token) {
            return Err("recovery artifact token is stale".into());
        }
        let node = i.selected.ok_or("no recovery target")?;
        crate::can_node_policy::check(node)?;
        (
            i.bus.clone().ok_or("recovery transport ended")?,
            i.staged.take().unwrap().1,
            node,
        )
    };
    let cancel = CancellationToken::new();
    *state.flash_cancel.lock().unwrap() = cancel.clone();
    state.active.store(true, Ordering::SeqCst);
    let _active = Active(&state.active);
    let total = policy.artifact().bin_size();
    let result = hexmeow_motor_iap::recover(
        bus.as_ref(),
        node,
        &policy,
        &Default::default(),
        &cancel,
        |event| {
            let p = match event {
                FlashEvent::Progress { written, total } => Progress {
                    stage: "writing",
                    completed: written,
                    total,
                    cancellable: true,
                },
                FlashEvent::Stage(stage) => Progress {
                    stage: match stage {
                        FlashStage::Resetting => "resetting",
                        FlashStage::EnteringBootloader => "entering_bootloader",
                        FlashStage::IdentityVerified => "revalidating",
                        FlashStage::StartingDownload => "preparing",
                        FlashStage::Transferring => "writing",
                        FlashStage::Finalizing => "finalizing",
                        FlashStage::Verifying => "verifying",
                    },
                    completed: 0,
                    total,
                    cancellable: stage != FlashStage::Verifying,
                },
                _ => return,
            };
            let _ = on_event.send(p);
        },
    )
    .await;
    {
        let mut i = state.inner.lock().unwrap();
        i.bus = None;
        i.selected = None;
    }
    result
        .map(|_| "verify_acked_startup_unconfirmed")
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub fn can_recovery_cancel(state: State<'_, RecoveryState>) -> bool {
    let active = state.is_active();
    if active {
        state.cancel.store(true, Ordering::SeqCst);
        state.flash_cancel.lock().unwrap().cancel();
    }
    active
}
#[tauri::command]
pub async fn can_recovery_leave(
    state: State<'_, RecoveryState>,
    token: Option<String>,
) -> Result<()> {
    if state.is_active() {
        return Err("cancel and wait for the recovery operation first".into());
    }
    let _op = state.operation.lock().await;
    let mut inner = state.inner.lock().unwrap();
    if token.as_ref().is_some_and(|t| t != &inner.token) {
        return Ok(());
    }
    *inner = Default::default();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectable_models_never_skip_factory_backup_or_disabled_profiles() {
        for p in profiles()
            .into_iter()
            .filter(|p| p.family == "motor_iap" && p.available)
        {
            let t = hexmeow_dfu_targets::target_by_profile_id(&p.id).unwrap();
            assert_eq!(t.preflight, PreflightPolicy::None);
            assert!(t.is_enabled());
        }
    }
    #[test]
    fn no_recovery_authority_without_explicit_native_model_selection() {
        let state = RecoveryState::default();
        assert!(require_hpm(
            &state,
            None,
            crate::hpm_can_profiles::VENDOR,
            crate::hpm_can_profiles::EVK_PRODUCT
        )
        .is_err());
        assert!(selected_profile(&state).is_err());
    }
}
