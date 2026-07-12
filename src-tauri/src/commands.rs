//! `#[tauri::command]` surface.
//!
//! Each command acquires the manager mutex, clones the `Arc` out, and drops
//! the guard before awaiting any motor I/O so two commands can run
//! concurrently on the same bus (the underlying [`Cia402Manager`] already
//! serialises overlapping ops via its `inflight_ops` set).

use std::sync::Arc;

use hex_motor::cia402::{Cia402Manager, Cia402ManagerOptions};
use hex_motor::types::MotorMode;
use tauri::State;

use crate::backend;
use crate::diag::{EventsSnapshot, LogLine};
use crate::dto::{LiveStateDto, MotorInfoDto, MotorModeDto, MotorTargetDto};
use crate::state::AppState;
use crate::zenoh_base::{BaseInfo, ZenohBaseState, ZenohConn};
use crate::zenoh_arm::{ArmInfo, ArmUrdf, ZenohArmConn, ZenohArmState};
use crate::zenoh_ee::{ConsoleUrdf, EeInfo, MountEdgeDto, RobotNode, SceneRobot, ZenohEeConn, ZenohEeState};
use crate::zenoh_config::{
    ConfigGetDto, ConfigSetResult, ConfigValidateResult, ControllerInfoDto, RestartResult, ZenohConfigConn,
};

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

#[tauri::command]
pub async fn connect(
    state: State<'_, AppState>,
    iface: String,
    our_nid: u8,
    broadcast_heartbeat: bool,
) -> CmdResult<()> {
    let mut guard = state.manager.lock().await;
    if guard.is_some() {
        return Err("already connected; call disconnect() first".into());
    }

    let (bus, _hw_ts) = backend::open_bus(&iface, false).await.map_err(err)?;
    let opts = Cia402ManagerOptions {
        heartbeat_node_id: our_nid,
        broadcast_heartbeat,
        ..Default::default()
    };
    let mgr = Cia402Manager::new(bus, opts).map_err(err)?;
    log::info!("connected to {iface} as nid 0x{our_nid:02X}");
    *guard = Some(Arc::new(mgr));
    Ok(())
}

#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    // Stop any running Robot Application first (disables its motors cleanly).
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
    if was {
        log::info!("disconnected");
    }
    Ok(())
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
    let results = mgr.initialize_all().await;
    Ok(results
        .into_iter()
        .map(|(nid, r)| (nid, r.err().map(|e| e.to_string())))
        .collect())
}

#[tauri::command]
pub async fn set_mode(
    state: State<'_, AppState>,
    nid: u8,
    mode: MotorModeDto,
) -> CmdResult<()> {
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
pub async fn set_max_torque(
    state: State<'_, AppState>,
    nid: u8,
    permille: u16,
) -> CmdResult<()> {
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

/// Change a motor's Node-ID (0x2001:01 + save). Power-cycle to apply.
#[tauri::command]
pub async fn change_node_id(
    state: State<'_, AppState>,
    nid: u8,
    new_id: u8,
) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    mgr.change_node_id(nid, new_id).await.map_err(err)
}

/// Drop offline motor entries from the discovery list (batch ID-change cleanup).
#[tauri::command]
pub async fn forget_offline(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(mgr) = state.manager().await {
        mgr.forget_offline();
    }
    Ok(())
}

/// Set this motor's current rotor position to `pos` (Rev, -0.5..0.5) via the
/// 0x3001 user-position-preset. Motor must be in Switch On Disabled (it is on
/// fresh power-up). See huayi.md §3.6.
#[tauri::command]
pub async fn set_position_preset(
    state: State<'_, AppState>,
    nid: u8,
    pos: f32,
) -> CmdResult<()> {
    let mgr = manager(&state).await?;
    mgr.set_position_preset(nid, pos).await.map_err(err)
}

/// Read 0x6064 (actual position, Rev) once, on demand.
#[tauri::command]
pub async fn read_position(state: State<'_, AppState>, nid: u8) -> CmdResult<f32> {
    let mgr = manager(&state).await?;
    mgr.read_position(nid).await.map_err(err)
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

/// Initialize the three HopeA3 motors and start the 500 Hz PV control loop.
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
pub async fn hopea3_get_state(
    state: State<'_, AppState>,
) -> CmdResult<crate::hopea3::Hopea3State> {
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
    let app = guard.as_ref().ok_or_else(|| "IMU not running".to_string())?;
    app.bias_trim().await.map_err(err)
}

/// Zero the IMU yaw (re-level from gravity).
#[tauri::command]
pub async fn imu_yaw_reset(state: State<'_, AppState>) -> CmdResult<()> {
    let guard = state.imu.lock().await;
    let app = guard.as_ref().ok_or_else(|| "IMU not running".to_string())?;
    app.yaw_reset().await.map_err(err)
}

// ───────────────────────────── CAN Analyzer ─────────────────────────────

/// Open `spec` (e.g. `"can0"`, `"gs_usb"`) as a fresh bus and start capturing
/// all traffic. Independent of the motor `connect()` — the analyzer owns its
/// bus. `hw_ts` requests device hardware timestamps (gs_usb, firmware-gated;
/// silently degrades to host timestamps — see the status `hw_ts` flag).
#[tauri::command]
pub async fn analyzer_start(state: State<'_, AppState>, spec: String, hw_ts: bool) -> CmdResult<()> {
    let mut guard = state.analyzer.lock().await;
    if guard.is_some() {
        return Err("analyzer already running; stop it first".into());
    }
    let app = crate::analyzer::CanAnalyzer::start(&spec, hw_ts)
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
pub async fn zenoh_acquire(state: State<'_, AppState>, prefix: String, model: String) -> CmdResult<()> {
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
    Ok(state.zenoh.lock().await.as_ref().map(|c| c.state()).unwrap_or_default())
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
    Ok(state.zenoh.lock().await.as_ref().map(|c| c.get_events()).unwrap_or_default())
}

#[tauri::command]
pub async fn zenoh_get_logs(state: State<'_, AppState>) -> CmdResult<Vec<LogLine>> {
    Ok(state.zenoh.lock().await.as_ref().map(|c| c.get_logs()).unwrap_or_default())
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
    if g.is_some() { return Err("Arm Zenoh 已连接;先 disconnect".into()); }
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
pub async fn arm_acquire(state: State<'_, AppState>, prefix: String, model: String) -> CmdResult<()> {
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
    Ok(state.zenoh_arm.lock().await.as_ref().map(|c| c.state()).unwrap_or_default())
}

/// 取某臂 URDF 供前端 3D 渲染(选中即拉,与取控解耦)。优先机器人级整机(arm+EE),退到臂-only;无则回 None。
#[tauri::command]
pub async fn arm_get_urdf(state: State<'_, AppState>, prefix: String) -> CmdResult<Option<ArmUrdf>> {
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
    Ok(state.zenoh_arm.lock().await.as_ref().map(|c| c.get_events()).unwrap_or_default())
}

#[tauri::command]
pub async fn arm_get_logs(state: State<'_, AppState>) -> CmdResult<Vec<LogLine>> {
    Ok(state.zenoh_arm.lock().await.as_ref().map(|c| c.get_logs()).unwrap_or_default())
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
    let c = g.as_ref().ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    Ok(c.discover().await)
}

/// 读取某控制器的 launch.yaml(含 sha256 / path / mtime / schema_version / recovery_mode)。
#[tauri::command]
pub async fn config_get(state: State<'_, AppState>, cid: String) -> CmdResult<ConfigGetDto> {
    let g = state.config.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    c.get(&cid).await.map_err(err)
}

/// 干跑校验(errors + 语义红线 critical_changes)。不落盘。
#[tauri::command]
pub async fn config_validate(state: State<'_, AppState>, cid: String, yaml: String) -> CmdResult<ConfigValidateResult> {
    let g = state.config.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Config Zenoh".to_string())?;
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
    let c = g.as_ref().ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    c.set(&cid, &yaml, &expect_sha256, apply, confirm, force).await.map_err(err)
}

/// 单独"应用":重启该控制器全部子进程(confirm 复述后为 true;force 越过会话检查)。
#[tauri::command]
pub async fn config_restart(state: State<'_, AppState>, cid: String, confirm: bool, force: bool) -> CmdResult<RestartResult> {
    let g = state.config.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 Config Zenoh".to_string())?;
    c.restart(&cid, confirm, force).await.map_err(err)
}


// ───────────────────────── EE(Zenoh)─────────────────────────
// 镜像 arm_* 的形状(commands 仅解锁转发,逻辑在 zenoh_ee.rs)。机器人控制台
// 共用本连接的 ee_discover_all 做设备树全量发现。

#[tauri::command]
pub async fn ee_connect(state: State<'_, AppState>, connect: String) -> CmdResult<()> {
    let mut g = state.zenoh_ee.lock().await;
    if g.is_some() { return Err("EE Zenoh 已连接;先 disconnect".into()); }
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

#[tauri::command]
pub async fn ee_acquire(state: State<'_, AppState>, prefix: String, model: String) -> CmdResult<()> {
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
    Ok(state.zenoh_ee.lock().await.as_ref().map(|c| c.state()).unwrap_or_default())
}

#[tauri::command]
pub async fn ee_release(state: State<'_, AppState>) -> CmdResult<()> {
    let g = state.zenoh_ee.lock().await;
    if let Some(c) = g.as_ref() { c.release().await; }
    Ok(())
}


/// 场景快照(M2 常驻 3D,30Hz 轮询):纯读缓存不触网。
#[tauri::command]
pub async fn ee_scene(state: State<'_, AppState>) -> CmdResult<Vec<SceneRobot>> {
    Ok(state.zenoh_ee.lock().await.as_ref().map(|c| c.scene()).unwrap_or_default())
}

/// 通用 URDF 取用(M2):先 <prefix>/urdf(臂=整机拼装),退 <prefix>/<kind>/urdf。
#[tauri::command]
pub async fn console_get_urdf(state: State<'_, AppState>, prefix: String, kind_name: String) -> CmdResult<Option<ConsoleUrdf>> {
    let g = state.zenoh_ee.lock().await;
    let c = g.as_ref().ok_or_else(|| "未连接 EE Zenoh".to_string())?;
    Ok(c.get_urdf(&prefix, &kind_name).await)
}


/// 整机挂载边(M3):cid → MountEdge 列表(随 3s 发现节拍刷新;无 machine 段 = 不含该 cid)。
#[tauri::command]
pub async fn ee_machines(state: State<'_, AppState>) -> CmdResult<std::collections::HashMap<String, Vec<MountEdgeDto>>> {
    Ok(state.zenoh_ee.lock().await.as_ref().map(|c| c.machines()).unwrap_or_default())
}
