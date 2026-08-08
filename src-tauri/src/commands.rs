//! `#[tauri::command]` surface.
//!
//! Each command acquires the manager mutex, clones the `Arc` out, and drops
//! the guard before awaiting any motor I/O so two commands can run
//! concurrently on the same bus (the underlying [`Cia402Manager`] already
//! serialises overlapping ops via its `inflight_ops` set).

use std::sync::Arc;
use std::time::Duration;

use hex_motor::cia402::{Cia402Manager, Cia402ManagerOptions};
use hex_motor::meow_motor::{
    MeowMitTarget, MeowMotorCanSettings, MeowMotorInitializeOptions, MeowMotorManager,
    MeowMotorManagerOptions, MeowMotorTarget, MeowProfileLimits, SignedQ8_24, Tpdo1Rate,
};
use hex_motor::types::MotorMode;
use tauri::State;

use crate::backend;
use crate::can_lease::{CanOwner, CanTransportGate};
use crate::diag::{EventsSnapshot, LogLine};
use crate::dto::{
    ConnectionInfoDto, DeviceSettingsRequestDto, DeviceSettingsResultDto, LiveStateDto,
    MeowCanSettingsRequestDto, MeowMotorSnapshotDto, MeowMotorTargetDto, MeowProfileLimitsDto,
    MotorInfoDto, MotorModeDto, MotorTargetDto,
};
use crate::friction_calibration::{FrictionCalibrationRequest, FrictionCalibrationView};
use crate::state::AppState;
use crate::torque_calibration::{TorqueCalibrationRequest, TorqueCalibrationView};
use crate::zenoh_arm::{ArmInfo, ArmUrdf, ZenohArmConn, ZenohArmState};
use crate::zenoh_base::{BaseInfo, ZenohBaseState, ZenohConn};
use crate::zenoh_config::{
    ConfigGetDto, ConfigSetResult, ConfigValidateResult, ControllerInfoDto, RestartResult,
    ZenohConfigConn,
};
use crate::zenoh_ee::{
    ConsoleUrdf, EeInfo, MountEdgeDto, RobotNode, SceneRobot, ZenohEeConn, ZenohEeState,
};
use crate::zenoh_hw::HardwareSnapshotDto;

/// Anything we hand back to the frontend.
type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

async fn manager(state: &AppState) -> CmdResult<Arc<Cia402Manager>> {
    state
        .manager()
        .await
        .ok_or_else(|| "not connected: call connect() first".to_string())
}

async fn meow_manager(state: &AppState) -> CmdResult<Arc<MeowMotorManager>> {
    state
        .meow_manager()
        .await
        .ok_or_else(|| "not connected: call connect() first".to_string())
}

/// Clone the active lift session without keeping the application-state mutex
/// across CAN I/O. In particular, directed NMT Stop/zero commands must not
/// wait behind a slow SDO diagnostics refresh.
async fn lift_session(state: &AppState) -> CmdResult<Arc<crate::lift::LiftSession>> {
    state
        .lift
        .lock()
        .await
        .clone()
        .ok_or_else(|| "lift is not attached".to_string())
}

/// Keep the handle registered until the device has acknowledged the safe
/// detach sequence. A failed CAN stop must remain visible and retryable.
pub(crate) async fn stop_lift_session(state: &AppState) -> CmdResult<()> {
    let app = match state.lift.lock().await.clone() {
        Some(app) => app,
        None => return Ok(()),
    };
    app.stop().await.map_err(err)?;
    let mut guard = state.lift.lock().await;
    if guard
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &app))
    {
        guard.take();
    }
    Ok(())
}

/// Cancel and await the Rust-owned friction job before dropping its CAN
/// manager. Cleanup failures remain visible in the final view but never trap
/// the application in a connected state.
pub(crate) async fn stop_friction_calibration(state: &AppState) {
    let _ = state.friction_calibration.stop().await;
}

pub(crate) async fn stop_torque_calibration(state: &AppState) {
    let _ = state.torque_calibration.stop().await;
}

#[tauri::command]
pub async fn connect(
    state: State<'_, AppState>,
    can_gate: State<'_, CanTransportGate>,
    iface: String,
    data_bitrate: u32,
    our_nid: u8,
    broadcast_heartbeat: bool,
) -> CmdResult<ConnectionInfoDto> {
    let mut guard = state.manager.lock().await;
    if guard.is_some() {
        return Err("already connected; call disconnect() first".into());
    }

    let can_lease = can_gate.try_acquire(CanOwner::Manager)?;
    let (bus, _hw_ts) = backend::open_bus(&iface, data_bitrate, false, can_lease)
        .await
        .map_err(err)?;
    let backend_name = backend::backend_name(&iface);
    let (link_config, inspection_error) = match bus.link_config().await {
        Ok(config) => (config, None),
        Err(error) => (None, Some(error.to_string())),
    };
    let opts = Cia402ManagerOptions {
        heartbeat_node_id: our_nid,
        broadcast_heartbeat,
        // Match the Analyzer SDO tab's proven busy-bus timeout. Normal SDOs
        // still complete immediately; this only widens the failure deadline.
        sdo_timeout: Duration::from_millis(500),
        ..Default::default()
    };
    let meow_opts = MeowMotorManagerOptions {
        heartbeat_node_id: our_nid,
        broadcast_heartbeat: false,
        auto_identify: false,
        sdo_timeout: Duration::from_millis(500),
        ..Default::default()
    };
    let meow_mgr = MeowMotorManager::new(bus.clone(), meow_opts).map_err(err)?;
    let calibration_bus = bus.clone();
    let mgr = Cia402Manager::new(bus, opts).map_err(err)?;
    log::info!("connected to {iface} as nid 0x{our_nid:02X}");
    *state.meow_manager.lock().await = Some(Arc::new(meow_mgr));
    *state.calibration_bus.lock().await = Some(calibration_bus);
    *state.calibration_host_node_id.lock().await = Some(our_nid);
    *guard = Some(Arc::new(mgr));
    Ok(ConnectionInfoDto::new(
        backend_name,
        link_config,
        inspection_error,
    ))
}

#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    // Persistent settings and position commands take the same gate before
    // touching the manager. An external disconnect therefore waits for the
    // in-flight transaction, while later transactions wait for teardown.
    let _operation = state.device_settings_operation.acquire().await;
    stop_friction_calibration(&state).await;
    state.torque_calibration.reset().await;
    state.authenticity.clear().await;
    // Stop any running Robot Application first (disables its motors cleanly).
    stop_lift_session(&state).await?;
    if let (Some(app), Some(mgr)) = (state.hopea3.lock().await.take(), state.manager().await) {
        app.stop(&mgr).await;
    }
    if let (Some(app), Some(mgr)) = (state.smartknob.lock().await.take(), state.manager().await) {
        app.stop(&mgr).await;
    }
    if let Some(app) = state.imu.lock().await.take() {
        app.stop().await;
    }
    // The analyzer owns its own bus, so stop it unconditionally (it may be the
    // only thing running — the user never called the manager-based connect()).
    if let Some(app) = state.analyzer.lock().await.take() {
        app.stop().await;
    }
    // Stop any running CSV recorders first so their files flush cleanly.
    for handle in state.drain_logs() {
        crate::logging::stop(handle).await;
    }
    let mut guard = state.manager.lock().await;
    let was = guard.take().is_some();
    state.meow_manager.lock().await.take();
    state.calibration_bus.lock().await.take();
    state.calibration_host_node_id.lock().await.take();
    if was {
        log::info!("disconnected");
    }
    Ok(())
}

#[tauri::command]
pub async fn friction_calibration_start(
    state: State<'_, AppState>,
    request: FrictionCalibrationRequest,
) -> CmdResult<FrictionCalibrationView> {
    let _start = state.calibration_start_gate.lock().await;
    let torque = state.torque_calibration.view().await;
    if torque.running || torque.acceptance_active {
        return Err("torque calibration already owns the motor bus".into());
    }
    let manager = meow_manager(&state).await?;
    let bus = state
        .calibration_bus()
        .await
        .ok_or_else(|| "calibration CAN transport is unavailable".to_string())?;
    let host_node_id = state
        .calibration_host_node_id()
        .await
        .ok_or_else(|| "calibration host node ID is unavailable".to_string())?;
    state
        .friction_calibration
        .start(manager, bus, host_node_id, request)
        .await
}

#[tauri::command]
pub async fn friction_calibration_get(
    state: State<'_, AppState>,
) -> CmdResult<FrictionCalibrationView> {
    Ok(state.friction_calibration.view().await)
}

#[tauri::command]
pub async fn friction_calibration_stop(
    state: State<'_, AppState>,
) -> CmdResult<FrictionCalibrationView> {
    Ok(state.friction_calibration.stop().await)
}

#[tauri::command]
pub async fn torque_calibration_start(
    state: State<'_, AppState>,
    request: TorqueCalibrationRequest,
) -> CmdResult<TorqueCalibrationView> {
    let _start = state.calibration_start_gate.lock().await;
    if state.friction_calibration.view().await.running {
        return Err("friction calibration already owns the motor bus".into());
    }
    let manager = meow_manager(&state).await?;
    let bus = state
        .calibration_bus()
        .await
        .ok_or_else(|| "calibration CAN transport is unavailable".to_string())?;
    let host_node_id = state
        .calibration_host_node_id()
        .await
        .ok_or_else(|| "calibration host node ID is unavailable".to_string())?;
    state
        .torque_calibration
        .start_measurement(manager, bus, host_node_id, request)
        .await
}

#[tauri::command]
pub async fn torque_calibration_acceptance_start(
    state: State<'_, AppState>,
) -> CmdResult<TorqueCalibrationView> {
    let _start = state.calibration_start_gate.lock().await;
    if state.friction_calibration.view().await.running {
        return Err("friction calibration already owns the motor bus".into());
    }
    let manager = meow_manager(&state).await?;
    let bus = state
        .calibration_bus()
        .await
        .ok_or_else(|| "calibration CAN transport is unavailable".to_string())?;
    let host_node_id = state
        .calibration_host_node_id()
        .await
        .ok_or_else(|| "calibration host node ID is unavailable".to_string())?;
    state
        .torque_calibration
        .start_acceptance(manager, bus, host_node_id)
        .await
}

#[tauri::command]
pub async fn torque_calibration_get(
    state: State<'_, AppState>,
) -> CmdResult<TorqueCalibrationView> {
    Ok(state.torque_calibration.view().await)
}

#[tauri::command]
pub async fn torque_calibration_stop(
    state: State<'_, AppState>,
) -> CmdResult<TorqueCalibrationView> {
    Ok(state.torque_calibration.stop().await)
}

#[tauri::command]
pub async fn is_connected(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.manager.lock().await.is_some())
}

#[tauri::command]
pub async fn list_devices(state: State<'_, AppState>) -> CmdResult<Vec<MotorInfoDto>> {
    let Some(mgr) = state.manager().await else {
        return Ok(Vec::new());
    };
    Ok(mgr.list().iter().map(MotorInfoDto::from).collect())
}

#[tauri::command]
pub async fn identify(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    mgr.identify(nid).await.map_err(err)
}

#[tauri::command]
pub async fn initialize(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    mgr.initialize(nid).await.map_err(err)
}

#[tauri::command]
pub async fn initialize_all(state: State<'_, AppState>) -> CmdResult<Vec<(u8, Option<String>)>> {
    let mgr = manager(&state).await?;
    let meow_mgr = meow_manager(&state).await?;
    let motor_nodes = mgr
        .list()
        .into_iter()
        .filter_map(|device| {
            let identity = device.identity.as_ref()?;
            motor_initialization_kind(identity.vendor_id, identity.product_code)
                .map(|kind| (device.node_id, kind))
        })
        .collect::<Vec<_>>();
    let mut results = Vec::with_capacity(motor_nodes.len());
    for (nid, kind) in motor_nodes {
        let result = match kind {
            crate::device_registry::DeviceKind::Cia402Motor => mgr.initialize(nid).await,
            crate::device_registry::DeviceKind::MeowMotor => match meow_mgr.identify(nid).await {
                Ok(_) => meow_mgr.initialize(nid, Tpdo1Rate::Hz1000).await,
                Err(error) => Err(error),
            },
            _ => unreachable!("initialize_all filters non-motor device kinds"),
        };
        results.push((nid, result));
    }
    Ok(results
        .into_iter()
        .map(|(nid, r)| (nid, r.err().map(|e| e.to_string())))
        .collect())
}

fn motor_initialization_kind(
    vendor_id: u32,
    product_code: u32,
) -> Option<crate::device_registry::DeviceKind> {
    match crate::device_registry::classify(vendor_id, product_code) {
        kind @ (crate::device_registry::DeviceKind::Cia402Motor
        | crate::device_registry::DeviceKind::MeowMotor) => Some(kind),
        _ => None,
    }
}

#[tauri::command]
pub async fn set_mode(state: State<'_, AppState>, nid: u8, mode: MotorModeDto) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    let mode: MotorMode = mode.into();
    mgr.set_mode(nid, mode).await.map_err(err)
}

#[tauri::command]
pub async fn set_target(
    state: State<'_, AppState>,
    nid: u8,
    target: MotorTargetDto,
) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    mgr.set_target(nid, target.into()).await.map_err(err)
}

#[tauri::command]
pub async fn set_max_torque(state: State<'_, AppState>, nid: u8, permille: u16) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    mgr.set_max_torque(nid, permille).await.map_err(err)
}

#[tauri::command]
pub async fn disable(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    mgr.disable(nid).await.map_err(err)
}

#[tauri::command]
pub async fn clear_error(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    mgr.clear_error(nid).await.map_err(err)
}

fn meow_snapshot_for(manager: &MeowMotorManager, nid: u8) -> CmdResult<MeowMotorSnapshotDto> {
    let info = manager
        .list()
        .into_iter()
        .find(|info| info.node_id == nid)
        .ok_or_else(|| format!("new-protocol motor node 0x{nid:02X} has not appeared"))?;
    let live = manager.status(nid).map_err(err)?;
    Ok(MeowMotorSnapshotDto::new(&info, &live))
}

#[tauri::command]
pub async fn meow_identify(state: State<'_, AppState>, nid: u8) -> CmdResult<MeowMotorSnapshotDto> {
    let manager = meow_manager(&state).await?;
    manager.identify(nid).await.map_err(err)?;
    meow_snapshot_for(&manager, nid)
}

#[tauri::command]
pub async fn meow_get_status(
    state: State<'_, AppState>,
    nid: u8,
) -> CmdResult<MeowMotorSnapshotDto> {
    let manager = meow_manager(&state).await?;
    meow_snapshot_for(&manager, nid)
}

#[tauri::command]
pub async fn meow_initialize(
    state: State<'_, AppState>,
    nid: u8,
    rate_hz: u16,
) -> CmdResult<MeowMotorSnapshotDto> {
    let manager = meow_manager(&state).await?;
    manager.identify(nid).await.map_err(err)?;
    let rate = match rate_hz {
        500 => Tpdo1Rate::Hz500,
        1000 => Tpdo1Rate::Hz1000,
        _ => return Err(format!("TPDO1 rate must be 500 or 1000, got {rate_hz}")),
    };
    let options = MeowMotorInitializeOptions::new(rate);
    manager
        .initialize_with_options(nid, options)
        .await
        .map_err(err)?;
    meow_snapshot_for(&manager, nid)
}

#[tauri::command]
pub async fn meow_activate_target(
    state: State<'_, AppState>,
    nid: u8,
    target: MeowMotorTargetDto,
) -> CmdResult<()> {
    let manager = meow_manager(&state).await?;
    // `set_mode_sdo` is deliberately ordered: every target object is written
    // first, then the 0x4401 mode command, then fresh TPDO2 confirms the mode.
    manager
        .set_mode_sdo(nid, meow_target(target)?)
        .await
        .map_err(err)
}

fn meow_target(target: MeowMotorTargetDto) -> CmdResult<MeowMotorTarget> {
    Ok(match target {
        MeowMotorTargetDto::ProfilePosition { position_rev } => MeowMotorTarget::ProfilePosition {
            position: SignedQ8_24::from_revolutions(position_rev).map_err(err)?,
        },
        MeowMotorTargetDto::ProfileVelocity { velocity_rev_per_s } => {
            MeowMotorTarget::ProfileVelocity { velocity_rev_per_s }
        }
        MeowMotorTargetDto::Torque { torque_permille } => {
            MeowMotorTarget::Torque { torque_permille }
        }
        MeowMotorTargetDto::Mit {
            position_rev,
            velocity_rev_per_s,
            torque_nm,
            kp,
            kd,
            kp_kd_limit_permille,
        } => MeowMotorTarget::Mit(MeowMitTarget {
            position_rev,
            velocity_rev_per_s,
            torque_nm,
            kp,
            kd,
            kp_kd_limit_permille,
        }),
    })
}

#[cfg(test)]
mod meow_command_tests {
    use super::*;
    use crate::device_registry::{
        DeviceKind, MEOW_MOTOR_4310_PRODUCT_CODE, MEOW_MOTOR_4342_PRODUCT_CODE,
        MEOW_MOTOR_VENDOR_ID,
    };

    #[test]
    fn initialize_all_routes_both_motor_protocols() {
        assert_eq!(
            motor_initialization_kind(MEOW_MOTOR_VENDOR_ID, MEOW_MOTOR_4310_PRODUCT_CODE),
            Some(DeviceKind::MeowMotor)
        );
        assert_eq!(
            motor_initialization_kind(MEOW_MOTOR_VENDOR_ID, MEOW_MOTOR_4342_PRODUCT_CODE),
            Some(DeviceKind::MeowMotor)
        );
        assert_eq!(
            motor_initialization_kind(0x4859_444C, 0xAAAA_0001),
            Some(DeviceKind::Cia402Motor)
        );
        assert_eq!(motor_initialization_kind(0xDEAD_BEEF, 1), None);
    }

    #[test]
    fn pp_target_accepts_negative_endpoint_and_rejects_positive_endpoint() {
        let minimum = meow_target(MeowMotorTargetDto::ProfilePosition {
            position_rev: -128.0,
        })
        .expect("-128 rev is the signed Q8.24 minimum");
        match minimum {
            MeowMotorTarget::ProfilePosition { position } => {
                assert_eq!(position, SignedQ8_24::MIN);
            }
            other => panic!("unexpected target: {other:?}"),
        }

        assert!(meow_target(MeowMotorTargetDto::ProfilePosition {
            position_rev: 128.0,
        })
        .is_err());
    }

    #[test]
    fn pp_target_does_not_clamp_or_wrap_invalid_values() {
        for position_rev in [-128.000_001, f64::NAN, f64::INFINITY] {
            assert!(meow_target(MeowMotorTargetDto::ProfilePosition { position_rev }).is_err());
        }
    }
}

#[tauri::command]
pub async fn meow_set_target(
    state: State<'_, AppState>,
    nid: u8,
    target: MeowMotorTargetDto,
) -> CmdResult<()> {
    let manager = meow_manager(&state).await?;
    manager
        .set_target_sdo(nid, meow_target(target)?)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn meow_set_max_torque(
    state: State<'_, AppState>,
    nid: u8,
    permille: u16,
) -> CmdResult<()> {
    let manager = meow_manager(&state).await?;
    manager.set_max_torque(nid, permille).await.map_err(err)
}

#[tauri::command]
pub async fn meow_set_profile_limits(
    state: State<'_, AppState>,
    nid: u8,
    limits: MeowProfileLimitsDto,
) -> CmdResult<()> {
    let manager = meow_manager(&state).await?;
    manager
        .set_profile_limits(
            nid,
            MeowProfileLimits {
                velocity_rev_per_s: limits.velocity_rev_per_s,
                acceleration_rev_per_s2: limits.acceleration_rev_per_s2,
                deceleration_rev_per_s2: limits.deceleration_rev_per_s2,
            },
        )
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn meow_disable(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    let manager = meow_manager(&state).await?;
    manager.disable(nid).await.map_err(err)
}

#[tauri::command]
pub async fn meow_clear_error(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    let manager = meow_manager(&state).await?;
    manager.clear_error(nid).await.map_err(err)
}

#[tauri::command]
pub async fn meow_start_log(state: State<'_, AppState>, nid: u8) -> CmdResult<String> {
    let manager = meow_manager(&state).await?;
    if let Some(existing) = state.take_log(nid) {
        crate::logging::stop(existing).await;
    }
    let handle = crate::logging::start_meow(manager, nid)
        .await
        .map_err(err)?;
    let path = handle.path.clone();
    state.logs.lock().unwrap().insert(nid, handle);
    log::info!("started meow CSV log for nid 0x{nid:02X}: {path}");
    Ok(path)
}

#[tauri::command]
pub async fn meow_apply_can_settings(
    state: State<'_, AppState>,
    nid: u8,
    request: MeowCanSettingsRequestDto,
) -> CmdResult<bool> {
    let _operation = state.device_settings_operation.acquire().await;
    let manager = meow_manager(&state).await?;
    manager.identify(nid).await.map_err(err)?;
    manager
        .apply_can_settings(
            nid,
            MeowMotorCanSettings {
                node_id: request.node_id,
                nominal_bitrate: request.nominal_bitrate,
                data_bitrate: request.data_bitrate,
                transmit_pdo_brs: request.transmit_pdo_brs,
            },
        )
        .await
        .map_err(err)
}

/// Apply one explicitly requested communication-settings transaction.
///
/// The manager force-reads `0x1018` inside the same per-node exclusive
/// operation before choosing an object-dictionary dialect. The GUI registry
/// check here is defense in depth and keeps unknown tuples and motor types
/// without a dedicated settings manager read-only even if a caller bypasses
/// the React controls.
#[tauri::command]
pub async fn apply_device_settings(
    state: State<'_, AppState>,
    request: DeviceSettingsRequestDto,
) -> CmdResult<DeviceSettingsResultDto> {
    let _operation = state.device_settings_operation.acquire().await;
    let kind =
        crate::device_registry::classify(request.expected_vendor_id, request.expected_product_code);
    if !kind.supports_device_settings() {
        let reason = if matches!(kind, crate::device_registry::DeviceKind::MeowMotor) {
            "this motor requires the dedicated meow_motor settings command"
        } else {
            "unsupported device identity"
        };
        return Err(format!(
            "{reason} 0x{:08X}/0x{:08X}",
            request.expected_vendor_id, request.expected_product_code
        ));
    }

    let mgr = manager(&state).await?;
    let result = mgr
        .apply_device_settings(
            request.node_id,
            request.expected_vendor_id,
            request.expected_product_code,
            request.update(),
        )
        .await
        .map_err(err)?;
    Ok(result.into())
}

/// Drop offline device entries from the discovery list (batch setup cleanup).
#[tauri::command]
pub async fn forget_offline(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(mgr) = state.manager().await {
        mgr.forget_offline();
    }
    Ok(())
}

/// Set this exact registered motor's current rotor position to `pos`
/// (Rev, -0.5..0.5) via the 0x3001 user-position-preset. The driver force
/// refreshes identity, requests Disable Voltage and confirms
/// Switch-On-Disabled before committing the preset.
#[tauri::command]
pub async fn set_position_preset(
    state: State<'_, AppState>,
    nid: u8,
    expected_vendor_id: u32,
    expected_product_code: u32,
    pos: f32,
) -> CmdResult<()> {
    let _operation = state.device_settings_operation.acquire().await;
    if !crate::device_registry::classify(expected_vendor_id, expected_product_code)
        .supports_position_preset()
    {
        return Err(format!(
            "position preset is unavailable for identity 0x{expected_vendor_id:08X}/0x{expected_product_code:08X}"
        ));
    }
    let mgr = manager(&state).await?;
    mgr.set_position_preset_for(nid, expected_vendor_id, expected_product_code, pos)
        .await
        .map_err(err)
}

/// Read 0x6064 (actual position, Rev) once, on demand, after exact identity
/// verification.
#[tauri::command]
pub async fn read_position(
    state: State<'_, AppState>,
    nid: u8,
    expected_vendor_id: u32,
    expected_product_code: u32,
) -> CmdResult<f32> {
    let _operation = state.device_settings_operation.acquire().await;
    if !crate::device_registry::classify(expected_vendor_id, expected_product_code)
        .supports_position_preset()
    {
        return Err(format!(
            "position read is unavailable for identity 0x{expected_vendor_id:08X}/0x{expected_product_code:08X}"
        ));
    }
    let mgr = manager(&state).await?;
    mgr.read_position_for(nid, expected_vendor_id, expected_product_code)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn get_status(state: State<'_, AppState>, nid: u8) -> CmdResult<LiveStateDto> {
    let mgr = manager(&state).await?;
    let snap = mgr.status(nid);
    Ok((&snap).into())
}

/// Start recording this motor's full-rate stream to a fresh CSV file. Returns
/// the absolute path. If a recorder is already running for this nid, it is
/// stopped and replaced (so the toggle is idempotent).
#[tauri::command]
pub async fn start_log(state: State<'_, AppState>, nid: u8) -> CmdResult<String> {
    let mgr = manager(&state).await?;
    if let Some(existing) = state.take_log(nid) {
        crate::logging::stop(existing).await;
    }
    let handle = crate::logging::start(mgr, nid).await.map_err(err)?;
    let path = handle.path.clone();
    state.logs.lock().unwrap().insert(nid, handle);
    log::info!("started CSV log for nid 0x{nid:02X}: {path}");
    Ok(path)
}

/// Stop the CSV recorder for this motor (flush + close). No-op if none running.
#[tauri::command]
pub async fn stop_log(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    if let Some(handle) = state.take_log(nid) {
        crate::logging::stop(handle).await;
        log::info!("stopped CSV log for nid 0x{nid:02X}");
    }
    Ok(())
}

// ───────────────────────── HopeA3 Robot Application ─────────────────────────

/// Initialize the three HopeA3 motors and start the 500 Hz uncompressed-MIT velocity loop.
#[tauri::command]
pub async fn hopea3_start(state: State<'_, AppState>) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    let mut guard = state.hopea3.lock().await;
    if guard.is_some() {
        return Err("HopeA3 already running; stop it first".into());
    }
    let app = crate::hopea3::Hopea3::start(mgr, &state.hopea3_init)
        .await
        .map_err(err)?;
    *guard = Some(app);
    log::info!("HopeA3 started");
    Ok(())
}

/// Poll init progress while `hopea3_start` runs (which motor / attempt).
#[tauri::command]
pub async fn hopea3_init_progress(
    state: State<'_, AppState>,
) -> CmdResult<crate::hopea3::InitProgress> {
    Ok(state.hopea3_init.lock().unwrap().clone())
}

/// Stop the control loop and disable all HopeA3 motors. No-op if not running.
#[tauri::command]
pub async fn hopea3_stop(state: State<'_, AppState>) -> CmdResult<()> {
    let app = state.hopea3.lock().await.take();
    if let Some(app) = app {
        let mgr = manager(&state).await?;
        app.stop(&mgr).await;
        log::info!("HopeA3 stopped");
    }
    Ok(())
}

/// Set the commanded body twist (m/s, m/s, rad/s). Clamped to limits, never errored.
#[tauri::command]
pub async fn hopea3_set_cmd(
    state: State<'_, AppState>,
    vx: f64,
    vy: f64,
    wz: f64,
) -> CmdResult<()> {
    if let Some(app) = state.hopea3.lock().await.as_ref() {
        app.set_cmd(vx, vy, wz);
    }
    Ok(())
}

/// Set per-motor max torque (‰ of peak), indexed [motor1, motor2, motor3].
#[tauri::command]
pub async fn hopea3_set_max_torque(
    state: State<'_, AppState>,
    permille: [u16; 3],
) -> CmdResult<()> {
    if let Some(app) = state.hopea3.lock().await.as_ref() {
        app.set_max_torque(permille);
    }
    Ok(())
}

/// Set per-motor MIT velocity gain KD (SI, Nm·s/rad), indexed [motor1,2,3].
#[tauri::command]
pub async fn hopea3_set_kd(state: State<'_, AppState>, kd_si: [f64; 3]) -> CmdResult<()> {
    if let Some(app) = state.hopea3.lock().await.as_ref() {
        app.set_kd(kd_si);
    }
    Ok(())
}

/// Adjust the velocity limits (max linear m/s magnitude, max angular rad/s).
#[tauri::command]
pub async fn hopea3_set_limits(
    state: State<'_, AppState>,
    max_linear: f64,
    max_angular: f64,
) -> CmdResult<()> {
    if let Some(app) = state.hopea3.lock().await.as_ref() {
        app.set_limits(max_linear, max_angular);
    }
    Ok(())
}

/// Re-initialize a single HopeA3 motor (e.g. one that faulted) while the chassis
/// keeps running. The other motors are unaffected.
#[tauri::command]
pub async fn hopea3_reinit_motor(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    let guard = state.hopea3.lock().await;
    match guard.as_ref() {
        Some(app) => app.reinit_motor(nid).await.map_err(err),
        None => Err("HopeA3 is not running".into()),
    }
}

/// Clear CiA402 faults on all three HopeA3 motors (best-effort). Useful before
/// starting if a previous run left them in a heartbeat-lost / fault state.
#[tauri::command]
pub async fn hopea3_clear_errors(state: State<'_, AppState>) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    crate::hopea3::clear_errors(&mgr).await;
    Ok(())
}

/// Set chassis acceleration (slew-rate) limits. `0` = unlimited. Linear is m/s²
/// (bounds the velocity-vector change), angular rad/s².
#[tauri::command]
pub async fn hopea3_set_accel_limits(
    state: State<'_, AppState>,
    max_lin_acc: f64,
    max_ang_acc: f64,
) -> CmdResult<()> {
    if let Some(app) = state.hopea3.lock().await.as_ref() {
        app.set_accel_limits(max_lin_acc, max_ang_acc);
    }
    Ok(())
}

/// Reset the dead-reckoned odometry pose to the origin.
#[tauri::command]
pub async fn hopea3_reset_odom(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(app) = state.hopea3.lock().await.as_ref() {
        app.reset_odom();
    }
    Ok(())
}

/// Poll the current chassis state (pose, twist, per-motor status).
#[tauri::command]
pub async fn hopea3_get_state(state: State<'_, AppState>) -> CmdResult<crate::hopea3::Hopea3State> {
    Ok(match state.hopea3.lock().await.as_ref() {
        Some(app) => app.state(),
        None => crate::hopea3::Hopea3State::default(),
    })
}

// ─────────────────────────────── SmartKnob ──────────────────────────────────

/// The available haptic presets (modes), so the UI can render the mode buttons
/// and dial. Static — does not require a connection.
#[tauri::command]
pub fn smartknob_configs() -> Vec<crate::smartknob::KnobConfig> {
    crate::smartknob::preset_configs()
}

/// Initialize the chosen motor as a haptic knob and start the haptic loop.
#[tauri::command]
pub async fn smartknob_start(
    state: State<'_, AppState>,
    nid: u8,
    config_index: usize,
) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    let mut guard = state.smartknob.lock().await;
    if guard.is_some() {
        return Err("SmartKnob already running; stop it first".into());
    }
    let app = crate::smartknob::SmartKnob::start(mgr, nid, config_index)
        .await
        .map_err(err)?;
    *guard = Some(app);
    log::info!("SmartKnob started on 0x{nid:02X}");
    Ok(())
}

/// Stop the haptic loop and disable the knob motor. No-op if not running.
#[tauri::command]
pub async fn smartknob_stop(state: State<'_, AppState>) -> CmdResult<()> {
    let app = state.smartknob.lock().await.take();
    if let Some(app) = app {
        let mgr = manager(&state).await?;
        app.stop(&mgr).await;
        log::info!("SmartKnob stopped");
    }
    Ok(())
}

/// Switch haptic mode (the front-panel "mode" button standing in for the press
/// sensor). Index into [`smartknob_configs`].
#[tauri::command]
pub async fn smartknob_set_config(state: State<'_, AppState>, index: usize) -> CmdResult<()> {
    if let Some(app) = state.smartknob.lock().await.as_ref() {
        app.set_config(index);
    }
    Ok(())
}

/// Update live haptic tunables: P-gain and D-gain (firmware PID units),
/// overall strength scale (Nm/unit), host torque clamp (Nm), motor-side
/// max-torque safety clamp (‰ of peak), Coulomb friction compensation (Nm)
/// Coulomb friction compensation (Nm) and click torque (Nm) for modes with
/// `click_torque_nm > 0`.
#[tauri::command]
pub async fn smartknob_set_tuning(
    state: State<'_, AppState>,
    p_gain: f64,
    d_gain: f64,
    strength_scale: f64,
    torque_limit_nm: f64,
    max_torque_permille: u16,
    friction_compensation: f64,
    click_torque_nm: f64,
) -> CmdResult<()> {
    if let Some(app) = state.smartknob.lock().await.as_ref() {
        app.set_tuning(
            p_gain,
            d_gain,
            strength_scale,
            torque_limit_nm,
            max_torque_permille,
            friction_compensation,
            click_torque_nm,
        );
    }
    Ok(())
}

/// Clear a CiA402 fault on the knob motor (best-effort recovery).
#[tauri::command]
pub async fn smartknob_clear_error(state: State<'_, AppState>) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    let nid = state.smartknob.lock().await.as_ref().map(|a| a.node_id());
    if let Some(nid) = nid {
        crate::smartknob::clear_error(&mgr, nid).await;
    }
    Ok(())
}

/// Update the custom mode's KnobConfig (index 0).  The haptic loop
/// re-applies it on the next tick without recentering the detent.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn smartknob_set_custom_config(
    state: State<'_, AppState>,
    position: i32,
    min_position: i32,
    max_position: i32,
    position_width_radians: f64,
    detent_strength_unit: f64,
    endstop_strength_unit: f64,
    snap_point: f64,
    snap_point_bias: f64,
    detent_positions: Vec<i32>,
    click_torque_nm: f64,
    friction_compensation: f64,
    strength_scale: f64,
    p_gain: f64,
    d_gain: f64,
    text: String,
    led_hue: i32,
) -> CmdResult<()> {
    let config = crate::smartknob::KnobConfig {
        position,
        min_position,
        max_position,
        position_width_radians,
        detent_strength_unit,
        endstop_strength_unit,
        snap_point,
        snap_point_bias,
        detent_positions,
        click_torque_nm,
        friction_compensation,
        strength_scale,
        p_gain,
        d_gain,
        text,
        led_hue,
        is_custom: true,
    };
    if let Some(app) = state.smartknob.lock().await.as_ref() {
        app.set_custom_config(config);
    }
    Ok(())
}

/// Poll the current knob state (position, sub-position, torque, health).
#[tauri::command]
pub async fn smartknob_get_state(
    state: State<'_, AppState>,
) -> CmdResult<crate::smartknob::SmartKnobState> {
    Ok(match state.smartknob.lock().await.as_ref() {
        Some(app) => app.state(),
        None => crate::smartknob::SmartKnobState::default(),
    })
}

// ───────────────────────────── IMU ──────────────────────────────

/// Start streaming the selected IMU: NMT-Start it Operational and subscribe to
/// its TPDO1 (quaternion + accel + gyro + temp).
#[tauri::command]
pub async fn imu_start(state: State<'_, AppState>, nid: u8) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    let mut guard = state.imu.lock().await;
    if guard.is_some() {
        return Err("IMU already running; stop it first".into());
    }
    let app = crate::imu::ImuManager::start(mgr, nid).await.map_err(err)?;
    *guard = Some(app);
    log::info!("IMU started on 0x{nid:02X}");
    Ok(())
}

/// Stop the IMU stream and return the device to Pre-Operational.
#[tauri::command]
pub async fn imu_stop(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(app) = state.imu.lock().await.take() {
        app.stop().await;
        log::info!("IMU stopped");
    }
    Ok(())
}

/// Poll the latest IMU snapshot (quaternion, accel, gyro, temp, counter).
#[tauri::command]
pub async fn imu_get_state(state: State<'_, AppState>) -> CmdResult<crate::imu::ImuState> {
    Ok(match state.imu.lock().await.as_ref() {
        Some(app) => app.state(),
        None => crate::imu::ImuState::default(),
    })
}

/// Trigger a still gyro-bias calibration (hold the device motionless).
#[tauri::command]
pub async fn imu_bias_trim(state: State<'_, AppState>) -> CmdResult<()> {
    let guard = state.imu.lock().await;
    let app = guard
        .as_ref()
        .ok_or_else(|| "IMU not running".to_string())?;
    app.bias_trim().await.map_err(err)
}

/// Zero the IMU yaw (re-level from gravity).
#[tauri::command]
pub async fn imu_yaw_reset(state: State<'_, AppState>) -> CmdResult<()> {
    let guard = state.imu.lock().await;
    let app = guard
        .as_ref()
        .ok_or_else(|| "IMU not running".to_string())?;
    app.yaw_reset().await.map_err(err)
}

// ───────────────────────────── CAN Analyzer ─────────────────────────────

/// Open `spec` (e.g. `"can0"`, `"gs_usb"`) as a fresh bus and start capturing
/// all traffic. Independent of the motor `connect()` — the analyzer owns its
/// bus. `hw_ts` requests device hardware timestamps (gs_usb, firmware-gated;
/// silently degrades to host timestamps — see the status `hw_ts` flag).
/// `data_bitrate=None` selects Classic CAN for gs_usb; SocketCAN ignores this
/// setting and preserves whatever arbitrary timing the user configured.
#[tauri::command]
pub async fn analyzer_start(
    state: State<'_, AppState>,
    can_gate: State<'_, CanTransportGate>,
    spec: String,
    data_bitrate: Option<u32>,
    hw_ts: bool,
) -> CmdResult<()> {
    let mut guard = state.analyzer.lock().await;
    if guard.is_some() {
        return Err("analyzer already running; stop it first".into());
    }
    let can_lease = can_gate.try_acquire(CanOwner::Analyzer)?;
    let app = crate::analyzer::CanAnalyzer::start(&spec, data_bitrate, hw_ts, can_lease)
        .await
        .map_err(err)?;
    *guard = Some(app);
    log::info!("CAN analyzer started on {spec:?} (hw_ts requested: {hw_ts})");
    Ok(())
}

/// Poll controller health (state + TX/RX error counters) from the backend.
/// Slow-changing — the UI polls this at ~1 Hz, separate from the trace.
#[tauri::command]
pub async fn analyzer_bus_state(
    state: State<'_, AppState>,
) -> CmdResult<crate::analyzer::BusHealthDto> {
    // Clone the bus out and drop the guard: netlink / USB control transfers
    // take milliseconds and must not block the trace polls.
    let bus = {
        let guard = state.analyzer.lock().await;
        match guard.as_ref() {
            Some(app) => app.bus_handle(),
            None => return Ok(crate::analyzer::BusHealthDto::default()),
        }
    };
    let s = bus.bus_state().await.map_err(err)?;
    Ok(crate::analyzer::BusHealthDto::from_state(s))
}

/// Stop capturing and release the analyzer's bus. No-op if not running.
#[tauri::command]
pub async fn analyzer_stop(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(app) = state.analyzer.lock().await.take() {
        app.stop().await;
        log::info!("CAN analyzer stopped");
    }
    Ok(())
}

/// Poll a bounded trace slice: frames after `after_seq` (up to `max`) passing
/// `filter`. Returns a `gap` flag when older frames were evicted.
#[tauri::command]
pub async fn analyzer_get_trace(
    state: State<'_, AppState>,
    after_seq: u64,
    max: u32,
    filter: crate::analyzer::FilterSpec,
) -> CmdResult<crate::analyzer::TraceReplyDto> {
    Ok(match state.analyzer.lock().await.as_ref() {
        Some(app) => app.get_trace(after_seq, max, &filter),
        None => crate::analyzer::TraceReplyDto::idle(),
    })
}

/// Poll the per-ID aggregate table (for the "grouped by ID" view).
#[tauri::command]
pub async fn analyzer_get_aggregates(
    state: State<'_, AppState>,
    filter: crate::analyzer::FilterSpec,
) -> CmdResult<crate::analyzer::AggReplyDto> {
    Ok(match state.analyzer.lock().await.as_ref() {
        Some(app) => app.get_aggregates(&filter),
        None => crate::analyzer::AggReplyDto::idle(),
    })
}

/// Poll analyzer status only (rate/drops/distinct ids/capabilities).
#[tauri::command]
pub async fn analyzer_get_status(
    state: State<'_, AppState>,
) -> CmdResult<crate::analyzer::AnalyzerStatusDto> {
    Ok(match state.analyzer.lock().await.as_ref() {
        Some(app) => app.get_status(),
        None => crate::analyzer::AnalyzerStatusDto::idle(),
    })
}

/// Empty the ring + aggregates + counters. Returns the cursor the frontend should
/// adopt so post-clear frames aren't treated as a gap.
#[tauri::command]
pub async fn analyzer_clear(state: State<'_, AppState>) -> CmdResult<u64> {
    Ok(match state.analyzer.lock().await.as_ref() {
        Some(app) => app.clear(),
        None => 0,
    })
}

/// Manually transmit a frame (and show it locally as a `tx` row).
#[tauri::command]
pub async fn analyzer_send(
    state: State<'_, AppState>,
    spec: crate::analyzer::SendSpec,
) -> CmdResult<()> {
    let guard = state.analyzer.lock().await;
    let app = guard
        .as_ref()
        .ok_or_else(|| "analyzer not running".to_string())?;
    app.send(spec).await.map_err(err)
}

/// Clone the SDO handles out of the analyzer guard so the (possibly
/// seconds-long, retrying) transfer never blocks the trace-poll commands.
async fn sdo_handles(
    state: &AppState,
) -> CmdResult<(
    std::sync::Arc<dyn can_transport::CanBus>,
    std::sync::Arc<tokio::sync::Mutex<()>>,
)> {
    let guard = state.analyzer.lock().await;
    let app = guard
        .as_ref()
        .ok_or_else(|| "analyzer not running".to_string())?;
    Ok(app.sdo_handles())
}

/// SDO read (upload) on the analyzer's bus — the comeow engine. `dtype` is a
/// CiA-309 token (`u16`, `x32`, `vs`, …) or `None` for raw-hex rendering.
#[tauri::command]
pub async fn analyzer_sdo_read(
    state: State<'_, AppState>,
    node: u8,
    index: u16,
    sub: u8,
    dtype: Option<String>,
    timeout_ms: u64,
    retries: u8,
) -> CmdResult<String> {
    let (bus, lock) = sdo_handles(&state).await?;
    let _serialized = lock.lock().await; // one SDO transfer at a time
    crate::sdo_client::read(
        &bus,
        node,
        index,
        sub,
        dtype.as_deref(),
        std::time::Duration::from_millis(timeout_ms.max(10)),
        // canopen-sdo's parameter is *total attempts* (clamped ≥1); the UI
        // exposes "retries", so N retries = N+1 attempts.
        retries.saturating_add(1),
    )
    .await
}

/// SDO write (download) on the analyzer's bus. Value is encoded per `dtype`.
#[tauri::command]
pub async fn analyzer_sdo_write(
    state: State<'_, AppState>,
    node: u8,
    index: u16,
    sub: u8,
    dtype: String,
    value: String,
    timeout_ms: u64,
    retries: u8,
) -> CmdResult<String> {
    let (bus, lock) = sdo_handles(&state).await?;
    let _serialized = lock.lock().await;
    crate::sdo_client::write(
        &bus,
        node,
        index,
        sub,
        &dtype,
        &value,
        std::time::Duration::from_millis(timeout_ms.max(10)),
        // Total attempts = UI retries + 1 (see analyzer_sdo_read).
        retries.saturating_add(1),
    )
    .await
}

// ───────────────────────── Base(Zenoh) ─────────────────────────

/// 连接到控制器网络。`connect` 如 `tcp/127.0.0.1:7447`(空=仅多播发现)。
#[tauri::command]
pub async fn zenoh_connect(state: State<'_, AppState>, connect: String) -> CmdResult<()> {
    let mut g = state.zenoh.lock().await;
    if g.is_some() {
        return Err("Zenoh 已连接;先 disconnect".into());
    }
    *g = Some(ZenohConn::open(&connect).await.map_err(err)?);
    log::info!("Zenoh 已连接: {connect}");
    Ok(())
}

#[tauri::command]
pub async fn zenoh_disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(c) = state.zenoh.lock().await.take() {
        c.release().await;
    }
    Ok(())
}

/// 发现网络里的底盘(kind==BASE)。
#[tauri::command]
pub async fn zenoh_discover(state: State<'_, AppState>) -> CmdResult<Vec<BaseInfo>> {
    let g = state.zenoh.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Zenoh".to_string())?;
    Ok(c.discover().await)
}

/// 取得某底盘的控制权。
#[tauri::command]
pub async fn zenoh_acquire(
    state: State<'_, AppState>,
    prefix: String,
    model: String,
) -> CmdResult<()> {
    let g = state.zenoh.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Zenoh".to_string())?;
    c.acquire(&prefix, &model).await.map_err(err)
}

/// 置 ACTIVE / DISABLED。
#[tauri::command]
pub async fn zenoh_set_active(state: State<'_, AppState>, on: bool) -> CmdResult<()> {
    let g = state.zenoh.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Zenoh".to_string())?;
    c.set_active(on).await.map_err(err)
}

/// 设置车体速度(由常驻 20Hz 流发出去喂看门狗)。
#[tauri::command]
pub async fn zenoh_set_cmd(state: State<'_, AppState>, vx: f64, vy: f64, wz: f64) -> CmdResult<()> {
    if let Some(c) = state.zenoh.lock().await.as_ref() {
        c.set_cmd(vx, vy, wz);
    }
    Ok(())
}

#[tauri::command]
pub async fn zenoh_get_state(state: State<'_, AppState>) -> CmdResult<ZenohBaseState> {
    Ok(state
        .zenoh
        .lock()
        .await
        .as_ref()
        .map(|c| c.state())
        .unwrap_or_default())
}

#[tauri::command]
pub async fn zenoh_release(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(c) = state.zenoh.lock().await.as_ref() {
        c.release().await;
    }
    Ok(())
}

/// 诊断聚焦(选中底盘时调):订阅其 events/logs 并播种历史。与取控解耦,只读也生效。
#[tauri::command]
pub async fn zenoh_set_diag_focus(state: State<'_, AppState>, prefix: String) -> CmdResult<()> {
    let g = state.zenoh.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Zenoh".to_string())?;
    c.set_diag_focus(&prefix).await;
    Ok(())
}

/// 手动"刷新历史":重新拉取 events/recent + log/recent 替换本地缓冲。
#[tauri::command]
pub async fn zenoh_refresh_diag(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Zenoh".to_string())?;
    c.refresh_diag().await;
    Ok(())
}

#[tauri::command]
pub async fn zenoh_get_events(state: State<'_, AppState>) -> CmdResult<EventsSnapshot> {
    Ok(state
        .zenoh
        .lock()
        .await
        .as_ref()
        .map(|c| c.get_events())
        .unwrap_or_default())
}

#[tauri::command]
pub async fn zenoh_get_logs(state: State<'_, AppState>) -> CmdResult<Vec<LogLine>> {
    Ok(state
        .zenoh
        .lock()
        .await
        .as_ref()
        .map(|c| c.get_logs())
        .unwrap_or_default())
}

/// P1-3 clear_fault:清除底盘锁存的 FATAL(需先取控)。
#[tauri::command]
pub async fn zenoh_clear_fault(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Zenoh".to_string())?;
    c.clear_fault().await.map_err(err)
}

// ───────────────────────── Arm(Zenoh)─────────────────────────

#[tauri::command]
pub async fn arm_connect(state: State<'_, AppState>, connect: String) -> CmdResult<()> {
    let mut g = state.zenoh_arm.lock().await;
    if g.is_some() {
        return Err("Arm Zenoh 已连接;先 disconnect".into());
    }
    *g = Some(ZenohArmConn::open(&connect).await.map_err(err)?);
    log::info!("Arm Zenoh 已连接: {connect}");
    Ok(())
}

#[tauri::command]
pub async fn arm_disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(c) = state.zenoh_arm.lock().await.take() {
        c.release().await;
    }
    Ok(())
}

/// 发现网络里的机械臂(kind==ARM)。
#[tauri::command]
pub async fn arm_discover(state: State<'_, AppState>) -> CmdResult<Vec<ArmInfo>> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    Ok(c.discover().await)
}

#[tauri::command]
pub async fn arm_acquire(
    state: State<'_, AppState>,
    prefix: String,
    model: String,
) -> CmdResult<()> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    c.acquire(&prefix, &model).await.map_err(err)
}

/// 设 OperatingMode(2=ACTIVE,3=PASSIVE,4=GRAVITY_COMP,1=DISABLED)。
#[tauri::command]
pub async fn arm_set_mode(state: State<'_, AppState>, mode: i32) -> CmdResult<()> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    c.set_mode(mode).await.map_err(err)
}

/// 设 base 系重力向量(m/s²)。
#[tauri::command]
pub async fn arm_set_gravity(state: State<'_, AppState>, gravity: [f32; 3]) -> CmdResult<()> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    c.set_gravity(gravity).await.map_err(err)
}

/// 移动到预设位姿(进 ACTIVE + 流目标)。kp/kd 由前端给。
#[tauri::command]
pub async fn arm_goto(state: State<'_, AppState>, q: Vec<f32>, kp: f32, kd: f32) -> CmdResult<()> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    c.goto(q, kp, kd).await.map_err(err)
}

#[tauri::command]
pub async fn arm_get_state(state: State<'_, AppState>) -> CmdResult<ZenohArmState> {
    Ok(state
        .zenoh_arm
        .lock()
        .await
        .as_ref()
        .map(|c| c.state())
        .unwrap_or_default())
}

/// 取某臂 URDF 供前端 3D 渲染(选中即拉,与取控解耦)。优先机器人级整机(arm+EE),退到臂-only;无则回 None。
#[tauri::command]
pub async fn arm_get_urdf(
    state: State<'_, AppState>,
    prefix: String,
) -> CmdResult<Option<ArmUrdf>> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    Ok(c.get_urdf(&prefix).await)
}

#[tauri::command]
pub async fn arm_release(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(c) = state.zenoh_arm.lock().await.as_ref() {
        c.release().await;
    }
    Ok(())
}

/// 诊断聚焦(选中机械臂时调):订阅其 events/logs 并播种历史。与取控解耦,只读也生效。
#[tauri::command]
pub async fn arm_set_diag_focus(state: State<'_, AppState>, prefix: String) -> CmdResult<()> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    c.set_diag_focus(&prefix).await;
    Ok(())
}

/// 手动"刷新历史":重新拉取 events/recent + log/recent 替换本地缓冲。
#[tauri::command]
pub async fn arm_refresh_diag(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    c.refresh_diag().await;
    Ok(())
}

#[tauri::command]
pub async fn arm_get_events(state: State<'_, AppState>) -> CmdResult<EventsSnapshot> {
    Ok(state
        .zenoh_arm
        .lock()
        .await
        .as_ref()
        .map(|c| c.get_events())
        .unwrap_or_default())
}

#[tauri::command]
pub async fn arm_get_logs(state: State<'_, AppState>) -> CmdResult<Vec<LogLine>> {
    Ok(state
        .zenoh_arm
        .lock()
        .await
        .as_ref()
        .map(|c| c.get_logs())
        .unwrap_or_default())
}

/// P1-3 clear_fault:清除机械臂锁存的 FATAL(需先取控)。
#[tauri::command]
pub async fn arm_clear_fault(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh_arm.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Arm Zenoh".to_string())?;
    c.clear_fault().await.map_err(err)
}

// ───────────────────────── Controller Config(Zenoh)─────────────────────────

/// 连接到控制器网络(config 面板专用 Session)。`connect` 空=仅多播发现。
#[tauri::command]
pub async fn config_connect(state: State<'_, AppState>, connect: String) -> CmdResult<()> {
    let mut g = state.config.lock().await;
    if g.is_some() {
        return Err("Config Zenoh 已连接;先 disconnect".into());
    }
    *g = Some(ZenohConfigConn::open(&connect).await.map_err(err)?);
    log::info!("Config Zenoh 已连接: {connect}");
    Ok(())
}

#[tauri::command]
pub async fn config_disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    state.config.lock().await.take();
    Ok(())
}

/// 发现网络里的控制器(走 `<cid>/info`;恢复模式下零 robot 也可发现)。
#[tauri::command]
pub async fn config_discover(state: State<'_, AppState>) -> CmdResult<Vec<ControllerInfoDto>> {
    let g = state.config.lock().await;
    let c = g
        .as_ref()
        .ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    Ok(c.discover().await)
}

/// 读取某控制器的 launch.yaml(含 sha256 / path / mtime / schema_version / recovery_mode)。
#[tauri::command]
pub async fn config_get(state: State<'_, AppState>, cid: String) -> CmdResult<ConfigGetDto> {
    let g = state.config.lock().await;
    let c = g
        .as_ref()
        .ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    c.get(&cid).await.map_err(err)
}

/// 干跑校验(errors + 语义红线 critical_changes)。不落盘。
#[tauri::command]
pub async fn config_validate(
    state: State<'_, AppState>,
    cid: String,
    yaml: String,
) -> CmdResult<ConfigValidateResult> {
    let g = state.config.lock().await;
    let c = g
        .as_ref()
        .ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    c.validate(&cid, &yaml).await.map_err(err)
}

/// 写入配置(乐观锁 expectSha256;apply=true 立即生效;有红线时 confirm 必须 true)。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn config_set(
    state: State<'_, AppState>,
    cid: String,
    yaml: String,
    expect_sha256: String,
    apply: bool,
    confirm: bool,
    force: bool,
) -> CmdResult<ConfigSetResult> {
    let g = state.config.lock().await;
    let c = g
        .as_ref()
        .ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    c.set(&cid, &yaml, &expect_sha256, apply, confirm, force)
        .await
        .map_err(err)
}

/// 单独"应用":重启该控制器全部子进程(confirm 复述后为 true;force 越过会话检查)。
#[tauri::command]
pub async fn config_restart(
    state: State<'_, AppState>,
    cid: String,
    confirm: bool,
    force: bool,
) -> CmdResult<RestartResult> {
    let g = state.config.lock().await;
    let c = g
        .as_ref()
        .ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    c.restart(&cid, confirm, force).await.map_err(err)
}

// ───────────────────────── EE(Zenoh)─────────────────────────
// 镜像 arm_* 的形状(commands 仅解锁转发,逻辑在 zenoh_ee.rs)。机器人控制台
// 共用本连接的 ee_discover_all 做设备树全量发现。

#[tauri::command]
pub async fn ee_connect(state: State<'_, AppState>, connect: String) -> CmdResult<()> {
    let mut g = state.zenoh_ee.lock().await;
    if g.is_some() {
        return Err("EE Zenoh 已连接;先 disconnect".into());
    }
    *g = Some(ZenohEeConn::open(&connect).await.map_err(err)?);
    log::info!("EE Zenoh 已连接: {connect}");
    Ok(())
}

#[tauri::command]
pub async fn ee_disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(c) = state.zenoh_ee.lock().await.take() {
        c.release().await;
    }
    Ok(())
}

/// 发现网络里的 EE(kind==EE),含 ee/description 细节(限位/OpeningMap)。
#[tauri::command]
pub async fn ee_discover(state: State<'_, AppState>) -> CmdResult<Vec<EeInfo>> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    Ok(c.discover().await)
}

/// 全量发现(机器人控制台设备树):所有 kind 的 robot,按 cid 分组由前端完成。
#[tauri::command]
pub async fn ee_discover_all(state: State<'_, AppState>) -> CmdResult<Vec<RobotNode>> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    Ok(c.discover_all().await)
}

/// Robot Console controller-HAL 只读快照：hw/info + liveliness + 最新 hw/<id> 样本。
#[tauri::command]
pub async fn hardware_snapshot(state: State<'_, AppState>) -> CmdResult<HardwareSnapshotDto> {
    let (session, monitor) = {
        let g = state.zenoh_ee.lock().await;
        g.as_ref()
            .map(ZenohEeConn::hardware_client)
            .ok_or_else(|| "未连接 Robot Console Zenoh".to_string())?
    };
    Ok(monitor.snapshot(&session).await)
}

#[tauri::command]
pub async fn ee_acquire(
    state: State<'_, AppState>,
    prefix: String,
    model: String,
) -> CmdResult<()> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    c.acquire(&prefix, &model).await.map_err(err)
}

/// 观察聚焦(只读,与取控解耦):设备树选中即观察。
#[tauri::command]
pub async fn ee_set_focus(state: State<'_, AppState>, prefix: String) -> CmdResult<()> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    c.set_focus(&prefix).await;
    Ok(())
}

/// 开合到 q(进 ACTIVE + 50Hz 流)。kp 省略 → 控制器默认增益;小 kp = 柔顺/限力抓取。
#[tauri::command]
pub async fn ee_goto(state: State<'_, AppState>, q: f32, kp: Option<f32>) -> CmdResult<()> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    c.goto(q, kp).await.map_err(err)
}

/// 设 OperatingMode(2=ACTIVE,1=DISABLED;EE v1 只支持这两个)。
#[tauri::command]
pub async fn ee_set_mode(state: State<'_, AppState>, mode: i32) -> CmdResult<()> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    c.set_mode(mode).await.map_err(err)
}

/// estop 期间姿态(1=保位 2=松开 3=抗拒张开;11 §10)。
#[tauri::command]
pub async fn ee_set_estop_behavior(state: State<'_, AppState>, behavior: i32) -> CmdResult<()> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    c.set_estop_behavior(behavior).await.map_err(err)
}

#[tauri::command]
pub async fn ee_clear_fault(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    c.clear_fault().await.map_err(err)
}

#[tauri::command]
pub async fn ee_get_state(state: State<'_, AppState>) -> CmdResult<ZenohEeState> {
    Ok(state
        .zenoh_ee
        .lock()
        .await
        .as_ref()
        .map(|c| c.state())
        .unwrap_or_default())
}

#[tauri::command]
pub async fn ee_release(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh_ee.lock().await;
    if let Some(c) = g.as_ref() {
        c.release().await;
    }
    Ok(())
}

/// 场景快照(M2 常驻 3D,30Hz 轮询):纯读缓存不触网。
#[tauri::command]
pub async fn ee_scene(state: State<'_, AppState>) -> CmdResult<Vec<SceneRobot>> {
    Ok(state
        .zenoh_ee
        .lock()
        .await
        .as_ref()
        .map(|c| c.scene())
        .unwrap_or_default())
}

/// 通用 URDF 取用(M2):先 <prefix>/urdf(臂=整机拼装),退 <prefix>/<kind>/urdf。
#[tauri::command]
pub async fn console_get_urdf(
    state: State<'_, AppState>,
    prefix: String,
    kind_name: String,
) -> CmdResult<Option<ConsoleUrdf>> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    Ok(c.get_urdf(&prefix, &kind_name).await)
}

/// 整机挂载边(M3):cid → MountEdge 列表(随 3s 发现节拍刷新;无 machine 段 = 不含该 cid)。
#[tauri::command]
pub async fn ee_machines(
    state: State<'_, AppState>,
) -> CmdResult<std::collections::HashMap<String, Vec<MountEdgeDto>>> {
    Ok(state
        .zenoh_ee
        .lock()
        .await
        .as_ref()
        .map(|c| c.machines())
        .unwrap_or_default())
}

// ───────────────────────── Controller Config Wi-Fi(Zenoh) ──────────────────

async fn config_zenoh_session(state: &AppState) -> CmdResult<zenoh::Session> {
    state
        .config
        .lock()
        .await
        .as_ref()
        .map(ZenohConfigConn::session)
        .ok_or_else(|| "未连接 Controller Config Zenoh".to_string())
}

#[tauri::command]
pub async fn wifi_discover(
    state: State<'_, AppState>,
) -> CmdResult<Vec<crate::zenoh_wifi::WifiControllerDto>> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::discover(&session).await.map_err(err)
}

#[tauri::command]
pub async fn wifi_status(
    state: State<'_, AppState>,
    cid: String,
) -> CmdResult<crate::zenoh_wifi::WifiStatusDto> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::status(&session, &cid).await.map_err(err)
}

#[tauri::command]
pub async fn wifi_scan(
    state: State<'_, AppState>,
    cid: String,
) -> CmdResult<Vec<crate::zenoh_wifi::WifiScanEntryDto>> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::scan(&session, &cid).await.map_err(err)
}

#[tauri::command]
pub async fn wifi_networks(
    state: State<'_, AppState>,
    cid: String,
) -> CmdResult<Vec<crate::zenoh_wifi::WifiSavedNetworkDto>> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::networks(&session, &cid)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn wifi_validate(
    state: State<'_, AppState>,
    cid: String,
    ssid: String,
    passphrase: String,
    hidden: bool,
    country: Option<String>,
) -> CmdResult<()> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::validate(&session, &cid, &ssid, passphrase, hidden, country)
        .await
        .map_err(err)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn wifi_set(
    state: State<'_, AppState>,
    cid: String,
    ssid: String,
    passphrase: String,
    hidden: bool,
    country: Option<String>,
    expected_revision: Option<u64>,
) -> CmdResult<crate::zenoh_wifi::WifiJobDto> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::set(
        &session,
        &cid,
        &ssid,
        passphrase,
        hidden,
        country,
        expected_revision,
    )
    .await
    .map_err(err)
}

#[tauri::command]
pub async fn wifi_forget(
    state: State<'_, AppState>,
    cid: String,
    ssid_hex: String,
    expected_revision: Option<u64>,
) -> CmdResult<crate::zenoh_wifi::WifiJobDto> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::forget(&session, &cid, &ssid_hex, expected_revision)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn wifi_forget_all(
    state: State<'_, AppState>,
    cid: String,
    expected_revision: Option<u64>,
) -> CmdResult<crate::zenoh_wifi::WifiJobDto> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::forget_all(&session, &cid, expected_revision)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn wifi_job(
    state: State<'_, AppState>,
    cid: String,
    job_id: String,
) -> CmdResult<crate::zenoh_wifi::WifiJobDto> {
    let session = config_zenoh_session(&state).await?;
    crate::zenoh_wifi::job(&session, &cid, &job_id)
        .await
        .map_err(err)
}

// ───────────────────────── Lift direct-CAN application ──────────────────────

/// Attach to one lift node and read its identity/nameplate/configuration.
/// This deliberately does not change NMT state or arm motion.
#[tauri::command]
pub async fn lift_start(state: State<'_, AppState>, nid: u8) -> CmdResult<crate::lift::LiftState> {
    let mgr = manager(&state).await?;
    let mut guard = state.lift.lock().await;
    if guard.is_some() {
        return Err("a lift session is already attached; detach it first".into());
    }
    let app = crate::lift::LiftSession::start(mgr, nid)
        .await
        .map_err(err)?;
    let snapshot = app.state();
    *guard = Some(Arc::new(app));
    Ok(snapshot)
}

/// Safe detach: directed NMT Stop, then confirmed Pre-operational + Disabled.
#[tauri::command]
pub async fn lift_stop(state: State<'_, AppState>) -> CmdResult<()> {
    stop_lift_session(&state).await
}

#[tauri::command]
pub async fn lift_get_state(state: State<'_, AppState>) -> CmdResult<crate::lift::LiftState> {
    let app = state.lift.lock().await.clone();
    Ok(app.map(|session| session.state()).unwrap_or_default())
}

/// Refresh non-PDO diagnostics over serialized SDO transactions.
#[tauri::command]
pub async fn lift_refresh(state: State<'_, AppState>) -> CmdResult<crate::lift::LiftState> {
    let app = lift_session(&state).await?;
    app.refresh().await.map_err(err)
}

#[tauri::command]
pub async fn lift_set_nmt(state: State<'_, AppState>, command: String) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.set_nmt(&command).await.map_err(err)
}

/// Immediate safe action. This always sends directed NMT Stop before the SDO
/// Disabled request, so it remains useful if the SDO path is unhealthy.
#[tauri::command]
pub async fn lift_disable(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.disable().await.map_err(err)
}

#[tauri::command]
pub async fn lift_home(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.home().await.map_err(err)
}

#[tauri::command]
pub async fn lift_clear_fault(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.clear_fault().await.map_err(err)
}

#[tauri::command]
pub async fn lift_set_velocity(state: State<'_, AppState>, velocity_mps: f32) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.set_velocity(velocity_mps).await.map_err(err)
}

#[tauri::command]
pub async fn lift_renew_velocity(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.renew_velocity_lease().map_err(err)
}

#[tauri::command]
pub async fn lift_set_position(state: State<'_, AppState>, position_m: f32) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.set_position(position_m).await.map_err(err)
}

#[tauri::command]
pub async fn lift_factory_calibration_arm(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.factory_calibration_arm().await.map_err(err)
}

#[tauri::command]
pub async fn lift_factory_calibration_seek_lower(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.factory_calibration_seek_lower().await.map_err(err)
}

#[tauri::command]
pub async fn lift_factory_calibration_seek_upper(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.factory_calibration_seek_upper().await.map_err(err)
}

#[tauri::command]
pub async fn lift_factory_calibration_abort(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.factory_calibration_abort().await.map_err(err)
}

#[tauri::command]
pub async fn lift_factory_calibration_clear_fault(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.factory_calibration_clear_fault().await.map_err(err)
}

#[tauri::command]
pub async fn lift_factory_calibration_commit(
    state: State<'_, AppState>,
    lower_reading_m: f32,
    upper_reading_m: f32,
    manufacture_date: String,
    calibration_date: String,
    station_id: u32,
) -> CmdResult<crate::lift::FactoryCalibrationResult> {
    let app = lift_session(&state).await?;
    app.factory_calibration_commit(
        lower_reading_m,
        upper_reading_m,
        &manufacture_date,
        &calibration_date,
        station_id,
    )
    .await
    .map_err(err)
}

#[tauri::command]
pub async fn lift_commission_arm(state: State<'_, AppState>) -> CmdResult<u32> {
    let app = lift_session(&state).await?;
    app.commission_arm().await.map_err(err)
}

#[tauri::command]
pub async fn lift_commission_hold(
    state: State<'_, AppState>,
    duty_permille: i16,
) -> CmdResult<u16> {
    let app = lift_session(&state).await?;
    app.commission_hold(duty_permille).await.map_err(err)
}

#[tauri::command]
pub async fn lift_commission_renew(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.renew_commission_lease().map_err(err)
}

#[tauri::command]
pub async fn lift_commission_release(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.commission_release().await.map_err(err)
}

#[tauri::command]
pub async fn lift_commission_disarm(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.commission_disarm().await.map_err(err)
}

#[tauri::command]
pub async fn lift_commission_clear_fault(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.commission_clear_fault().await.map_err(err)
}

#[tauri::command]
pub async fn lift_commission_epoch_service(
    state: State<'_, AppState>,
    motor_disconnected: bool,
) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.commission_epoch_service(motor_disconnected)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn lift_commission_estop(state: State<'_, AppState>) -> CmdResult<()> {
    let app = lift_session(&state).await?;
    app.commission_estop().await.map_err(err)
}

#[tauri::command]
pub async fn lift_commission_csv(state: State<'_, AppState>) -> CmdResult<String> {
    let app = lift_session(&state).await?;
    app.commission_csv().map_err(err)
}

// ───────────────────────── Lift (Zenoh robot API) ─────────────────────────
//
// 独立于 catRawCanApp 里的直连 CAN `lift` 调试工具:那个直接说 CANopen,这个只说
// 12-lift-api 的公共 robot API,因此对"托管在底盘进程里"还是"独占总线的
// lift_controller"完全无感 —— 键空间一样。

/// 连上 Zenoh 并开始被动观察(不取控、不动设备)。
#[tauri::command]
pub async fn zlift_connect(state: State<'_, AppState>, connect: String) -> CmdResult<()> {
    let mut g = state.zenoh_lift.lock().await;
    if g.is_some() {
        return Err("Lift Zenoh 已连接;先 disconnect".into());
    }
    *g = Some(crate::zenoh_lift::ZenohLiftConn::open(&connect).await.map_err(err)?);
    log::info!("Lift Zenoh 已连接: {connect}");
    Ok(())
}

#[tauri::command]
pub async fn zlift_disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(c) = state.zenoh_lift.lock().await.take() {
        c.release().await;
    }
    Ok(())
}

/// 发现网络里的升降(kind==LIFT),含 lift/description(设备派生的软限位与能力声明)。
#[tauri::command]
pub async fn zlift_discover(
    state: State<'_, AppState>,
) -> CmdResult<Vec<crate::zenoh_lift::LiftInfo>> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    Ok(c.discover().await)
}

/// 观察聚焦(只读,与取控解耦):列表选中即观察。
#[tauri::command]
pub async fn zlift_set_focus(state: State<'_, AppState>, prefix: String) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.set_focus(&prefix).await;
    Ok(())
}

#[tauri::command]
pub async fn zlift_acquire(
    state: State<'_, AppState>,
    prefix: String,
    model: String,
) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.acquire(&prefix, &model).await.map_err(err)
}

/// 回零。立即返回 started,完成与否看 `homed` 徽标(要几秒~几十秒)。
#[tauri::command]
pub async fn zlift_home(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.home().await.map_err(err)
}

/// 去指定高度(自主 goal,发完即撒手)。
#[tauri::command]
pub async fn zlift_goto(state: State<'_, AppState>, height: f32) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.goto(height).await.map_err(err)
}

/// 点动:`Some(dq)` 起 50Hz 速度流,`None` 停车。
#[tauri::command]
pub async fn zlift_jog(state: State<'_, AppState>, dq: Option<f32>) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.jog(dq).await.map_err(err)
}

/// v1 只支持 DISABLED(1)/ACTIVE(2);未 homing 时 ACTIVE 会被控制器如实拒绝。
#[tauri::command]
pub async fn zlift_set_mode(state: State<'_, AppState>, mode: i32) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.set_mode(mode).await.map_err(err)
}

/// 收紧软限位/速度上限(只收紧;越界值由控制器夹回设备能力)。
#[tauri::command]
pub async fn zlift_set_limits(
    state: State<'_, AppState>,
    pos_min: Option<f32>,
    pos_max: Option<f32>,
    vel_max: Option<f32>,
) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.set_limits(pos_min, pos_max, vel_max).await.map_err(err)
}

#[tauri::command]
pub async fn zlift_clear_fault(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.clear_fault().await.map_err(err)
}

#[tauri::command]
pub async fn zlift_get_state(
    state: State<'_, AppState>,
) -> CmdResult<crate::zenoh_lift::ZenohLiftState> {
    Ok(state
        .zenoh_lift
        .lock()
        .await
        .as_ref()
        .map(|c| c.state())
        .unwrap_or_default())
}

#[tauri::command]
pub async fn zlift_release(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh_lift.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Lift Zenoh".to_string())?;
    c.release().await;
    Ok(())
}
