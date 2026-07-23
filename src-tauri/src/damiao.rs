//! Direct CAN 2.0 control for the DAMIAO DM-J4310-2EC V1.1 geared motor.
//!
//! This motor does not speak CiA 402.  The session only borrows the physical
//! bus owned by `Cia402Manager`. A bus-level discovery monitor finds motors,
//! while one independent session per motor implements the DAMIAO MIT /
//! position-velocity / velocity frame formats.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use can_transport::{CanBus, CanFilter, CanFrame, CanId, CanIoError, FrameKind};
use hex_motor::cia402::Cia402Manager;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

const STATUS_REQUEST_ID: u16 = 0x7FF;
const STATUS_REQUEST_COMMAND: u8 = 0xCC;
const ENABLE_COMMAND: u8 = 0xFC;
const DISABLE_COMMAND: u8 = 0xFD;
const SET_ZERO_COMMAND: u8 = 0xFE;
const CLEAR_FAULT_COMMAND: u8 = 0xFB;
const WRITE_PARAMETER_COMMAND: u8 = 0x55;
const CONTROL_MODE_REGISTER: u8 = 0x0A;

const FEEDBACK_FRESH_FOR: Duration = Duration::from_millis(750);
const DISCOVERY_FRESH_FOR: Duration = Duration::from_millis(2500);
const DISCOVERY_STEP: Duration = Duration::from_millis(75);
const DISCOVERY_RESPONSE_WINDOW: Duration = Duration::from_millis(70);
const LEGACY_DISCOVERY_EVERY_SWEEPS: u32 = 5;
const AUTO_DISCOVERY_MAX_ID: u16 = 0x0F;
const STATUS_REQUEST_PERIOD: Duration = Duration::from_millis(100);
const TARGET_STREAM_PERIOD: Duration = Duration::from_millis(20);
const MODE_SWITCH_SETTLE: Duration = Duration::from_millis(100);
const FEEDBACK_RATE_WINDOW: Duration = Duration::from_millis(500);
const DM_J4310_PEAK_TORQUE_NM: f32 = 7.0;
const DM_J4310_MAX_SPEED_RAD_S: f32 = 200.0 * std::f32::consts::TAU / 60.0;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DamiaoMode {
    Mit,
    PositionVelocity,
    Velocity,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DamiaoConfig {
    pub motor_id: u16,
    pub master_id: u16,
    pub mode: DamiaoMode,
    pub p_max: f32,
    pub v_max: f32,
    pub t_max: f32,
}

/// One motor seen by the bus-level automatic discovery monitor.
#[derive(Debug, Clone, Serialize)]
pub struct DamiaoDiscoveredDevice {
    pub motor_id: u16,
    pub feedback_can_id: Option<u16>,
    pub online: bool,
    pub attached: bool,
    pub status_code: u8,
    pub status: String,
    pub feedback_age_ms: Option<u64>,
    pub rx_count: u64,
}

impl DamiaoConfig {
    fn validate(self) -> Result<Self> {
        // The feedback payload only carries the low nibble of the motor ID.
        // DAMIAO recommends IDs below 16; allowing one byte keeps compatibility
        // while still rejecting values the V1.1 feedback format cannot name.
        if self.motor_id > 0xFF {
            bail!("DAMIAO motor CAN ID must be in 0x000..=0x0FF");
        }
        if self.master_id > 0x7FF {
            bail!("DAMIAO Master ID must be an 11-bit CAN ID");
        }
        let control_id = control_id(self.motor_id, self.mode)?;
        if control_id > 0x7FF {
            bail!("DAMIAO control CAN ID exceeds the 11-bit range");
        }
        for (name, value) in [
            ("PMAX", self.p_max),
            ("VMAX", self.v_max),
            ("TMAX", self.t_max),
        ] {
            if !value.is_finite() || value <= 0.0 {
                bail!("{name} must be a finite positive number");
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DamiaoTarget {
    Mit {
        position_rad: f32,
        velocity_rad_s: f32,
        torque_nm: f32,
        kp: f32,
        kd: f32,
    },
    PositionVelocity {
        position_rad: f32,
        velocity_rad_s: f32,
    },
    Velocity {
        velocity_rad_s: f32,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DamiaoState {
    pub attached: bool,
    pub motor_id: u16,
    pub master_id: u16,
    /// Actual standard CAN ID observed on the most recent valid feedback.
    /// It may differ from `master_id` when the drive was configured elsewhere.
    pub feedback_can_id: Option<u16>,
    pub mode: DamiaoMode,
    pub p_max: f32,
    pub v_max: f32,
    pub t_max: f32,
    pub online: bool,
    pub enabled: bool,
    pub streaming: bool,
    pub status_code: u8,
    pub status: String,
    pub position_rad: Option<f32>,
    pub velocity_rad_s: Option<f32>,
    pub torque_nm: Option<f32>,
    pub mos_temp_c: Option<u8>,
    pub rotor_temp_c: Option<u8>,
    pub feedback_age_ms: Option<u64>,
    /// Rate of valid feedback frames measured in the receive task. This is
    /// based on every CAN frame, not on the slower frontend polling cadence.
    pub feedback_rate_hz: Option<f32>,
    pub rx_count: u64,
    pub last_error: Option<String>,
}

impl DamiaoState {
    fn attached(config: DamiaoConfig) -> Self {
        Self {
            attached: true,
            motor_id: config.motor_id,
            master_id: config.master_id,
            feedback_can_id: None,
            mode: config.mode,
            p_max: config.p_max,
            v_max: config.v_max,
            t_max: config.t_max,
            online: false,
            enabled: false,
            streaming: false,
            status_code: 0,
            status: "waiting for feedback".into(),
            position_rad: None,
            velocity_rad_s: None,
            torque_nm: None,
            mos_temp_c: None,
            rotor_temp_c: None,
            feedback_age_ms: None,
            feedback_rate_hz: None,
            rx_count: 0,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone)]
struct DiscoveredEntry {
    feedback_can_id: u16,
    status_code: u8,
    last_feedback: Instant,
    rx_count: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingProbe {
    motor_id: u16,
    sent_at: Instant,
}

#[derive(Default)]
struct DiscoveryState {
    devices: BTreeMap<u16, DiscoveredEntry>,
    attached: HashSet<u16>,
    pending: Option<PendingProbe>,
}

/// Bus-level automatic discovery for the public DAMIAO direct-CAN protocol.
///
/// The common protocol provides no heartbeat or broadcast inventory command.
/// We therefore scan the feedback frame's unambiguous ID range with the
/// documented 0x7FF/0xCC status request. The DM-J4310 V1.1 firmware family is
/// not guaranteed to implement that later request, so unknown, unattached IDs
/// also receive the motion-safe Disable command. Attached IDs are never
/// legacy-probed, which prevents discovery from interrupting active control.
pub struct DamiaoDiscovery {
    state: Arc<StdMutex<DiscoveryState>>,
    running: Arc<AtomicBool>,
    request_safe_sweep: Arc<AtomicBool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl DamiaoDiscovery {
    pub async fn start(mgr: Arc<Cia402Manager>) -> Result<Arc<Self>> {
        let bus = mgr.bus();
        let rx = bus
            .subscribe(CanFilter::pass_all_standard())
            .await
            .map_err(|error| anyhow!("subscribe DAMIAO discovery feedback: {error}"))?;
        let state = Arc::new(StdMutex::new(DiscoveryState::default()));
        let running = Arc::new(AtomicBool::new(true));
        let request_safe_sweep = Arc::new(AtomicBool::new(false));
        let discovery = Arc::new(Self {
            state: state.clone(),
            running: running.clone(),
            request_safe_sweep: request_safe_sweep.clone(),
            tasks: StdMutex::new(Vec::new()),
        });

        let receive_task = tokio::spawn(discovery_receive_loop(rx, state.clone(), running.clone()));
        let scan_task = tokio::spawn(discovery_scan_loop(bus, state, running, request_safe_sweep));
        discovery
            .tasks
            .lock()
            .unwrap()
            .extend([receive_task, scan_task]);
        Ok(discovery)
    }

    pub fn snapshot(&self) -> Vec<DamiaoDiscoveredDevice> {
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        state
            .devices
            .iter()
            .map(|(&motor_id, entry)| {
                let age = now.saturating_duration_since(entry.last_feedback);
                DamiaoDiscoveredDevice {
                    motor_id,
                    feedback_can_id: Some(entry.feedback_can_id),
                    online: age <= DISCOVERY_FRESH_FOR,
                    attached: state.attached.contains(&motor_id),
                    status_code: entry.status_code,
                    status: status_name(entry.status_code).into(),
                    feedback_age_ms: Some(age.as_millis() as u64),
                    rx_count: entry.rx_count,
                }
            })
            .collect()
    }

    pub fn set_attached(&self, motor_id: u16, attached: bool) {
        let mut state = self.state.lock().unwrap();
        if attached {
            state.attached.insert(motor_id);
        } else {
            state.attached.remove(&motor_id);
        }
    }

    pub fn request_safe_sweep(&self) {
        self.request_safe_sweep.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

async fn discovery_receive_loop(
    mut rx: Box<dyn can_transport::CanRx>,
    state: Arc<StdMutex<DiscoveryState>>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::Acquire) {
        match rx.recv().await {
            Ok(frame) => {
                if frame.kind() != FrameKind::Data
                    || frame.data().len() != 8
                    || frame.id() == CanId::Standard(STATUS_REQUEST_ID)
                {
                    continue;
                }
                let Some((motor_id, status_code)) = feedback_identity(frame.data()) else {
                    continue;
                };
                if is_parameter_reply_for(motor_id, frame.data()) {
                    continue;
                }
                let CanId::Standard(feedback_can_id) = frame.id() else {
                    continue;
                };
                let now = Instant::now();
                let mut shared = state.lock().unwrap();
                let expected = shared.pending.is_some_and(|probe| {
                    probe.motor_id == motor_id
                        && now.saturating_duration_since(probe.sent_at) <= DISCOVERY_RESPONSE_WINDOW
                });
                if !expected && !shared.devices.contains_key(&motor_id) {
                    continue;
                }
                let entry = shared.devices.entry(motor_id).or_insert(DiscoveredEntry {
                    feedback_can_id,
                    status_code,
                    last_feedback: now,
                    rx_count: 0,
                });
                entry.feedback_can_id = feedback_can_id;
                entry.status_code = status_code;
                entry.last_feedback = now;
                entry.rx_count = entry.rx_count.saturating_add(1);
            }
            Err(CanIoError::Lagged { .. }) => {}
            Err(error) => {
                log::warn!("DAMIAO discovery receive stopped: {error}");
                break;
            }
        }
    }
}

async fn discovery_scan_loop(
    bus: Arc<dyn CanBus>,
    state: Arc<StdMutex<DiscoveryState>>,
    running: Arc<AtomicBool>,
    request_safe_sweep: Arc<AtomicBool>,
) {
    let mut ticker = tokio::time::interval(DISCOVERY_STEP);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut motor_id = 0u16;
    let mut sweep = 0u32;
    let mut force_safe_sweep = true;
    while running.load(Ordering::Acquire) {
        ticker.tick().await;
        if motor_id == 0 && request_safe_sweep.swap(false, Ordering::AcqRel) {
            force_safe_sweep = true;
        }
        let use_legacy_disable = {
            let shared = state.lock().unwrap();
            let stale_or_unknown = shared.devices.get(&motor_id).map_or(true, |entry| {
                Instant::now().saturating_duration_since(entry.last_feedback) > DISCOVERY_FRESH_FOR
            });
            !shared.attached.contains(&motor_id)
                && stale_or_unknown
                && (force_safe_sweep || sweep % LEGACY_DISCOVERY_EVERY_SWEEPS == 0)
        };
        let frame = if use_legacy_disable {
            CanFrame::new_data(motor_id, &special_payload(DISABLE_COMMAND))
        } else {
            CanFrame::new_data(STATUS_REQUEST_ID, &status_request_payload(motor_id))
        };
        let Ok(frame) = frame else {
            continue;
        };
        state.lock().unwrap().pending = Some(PendingProbe {
            motor_id,
            sent_at: Instant::now(),
        });
        if let Err(error) = bus.send(frame).await {
            log::warn!("DAMIAO discovery probe for ID 0x{motor_id:X} failed: {error}");
        }

        if motor_id == AUTO_DISCOVERY_MAX_ID {
            motor_id = 0;
            sweep = sweep.wrapping_add(1);
            force_safe_sweep = false;
        } else {
            motor_id += 1;
        }
    }
}

fn feedback_identity(data: &[u8]) -> Option<(u16, u8)> {
    if data.len() != 8 {
        return None;
    }
    let status_code = data[0] >> 4;
    if !matches!(status_code, 0x0 | 0x1 | 0x8..=0xE) {
        return None;
    }
    Some((u16::from(data[0] & 0x0F), status_code))
}

fn is_parameter_reply_for(motor_id: u16, data: &[u8]) -> bool {
    data.len() == 8
        && data[0] == motor_id as u8
        && data[1] == (motor_id >> 8) as u8
        && matches!(data[2], 0x33 | WRITE_PARAMETER_COMMAND)
}

struct SharedState {
    view: DamiaoState,
    last_feedback: Option<Instant>,
    feedback_rate_window_start: Option<Instant>,
    feedback_rate_intervals: u32,
    feedback_rate_hz: Option<f32>,
}

/// One attached DM-J4310-2EC V1.1 motor.
pub struct DamiaoSession {
    bus: Arc<dyn CanBus>,
    config: Arc<StdMutex<DamiaoConfig>>,
    state: Arc<StdMutex<SharedState>>,
    stream_target: Arc<StdMutex<Option<DamiaoTarget>>>,
    /// Serializes motion sends and state-changing control transactions. The
    /// stream loop acquires this before reading its target, so a mode switch
    /// cannot leak one last target encoded with the previous mode.
    control_gate: Arc<AsyncMutex<()>>,
    running: Arc<AtomicBool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl DamiaoSession {
    pub async fn start(mgr: Arc<Cia402Manager>, config: DamiaoConfig) -> Result<Arc<Self>> {
        let config = config.validate()?;
        let bus = mgr.bus();
        // Listen broadly so a stale/unknown Master ID cannot hide otherwise
        // valid feedback. The payload motor ID and status nibble are validated
        // in `decode_feedback`; the observed CAN ID is reported to the UI.
        let rx = bus
            .subscribe(CanFilter::pass_all_standard())
            .await
            .map_err(|error| anyhow!("subscribe DAMIAO standard feedback: {error}"))?;

        let state = Arc::new(StdMutex::new(SharedState {
            view: DamiaoState::attached(config),
            last_feedback: None,
            feedback_rate_window_start: None,
            feedback_rate_intervals: 0,
            feedback_rate_hz: None,
        }));
        let config = Arc::new(StdMutex::new(config));
        let stream_target = Arc::new(StdMutex::new(None));
        let control_gate = Arc::new(AsyncMutex::new(()));
        let running = Arc::new(AtomicBool::new(true));

        let session = Arc::new(Self {
            bus: bus.clone(),
            config: config.clone(),
            state: state.clone(),
            stream_target: stream_target.clone(),
            control_gate: control_gate.clone(),
            running: running.clone(),
            tasks: StdMutex::new(Vec::new()),
        });

        let receive_task = tokio::spawn(receive_loop(
            rx,
            config.clone(),
            state.clone(),
            running.clone(),
        ));
        let refresh_task = tokio::spawn(refresh_loop(
            bus.clone(),
            config.clone(),
            state.clone(),
            running.clone(),
        ));
        let stream_task = tokio::spawn(stream_loop(
            bus,
            config,
            state,
            stream_target.clone(),
            control_gate,
            running,
        ));
        session
            .tasks
            .lock()
            .unwrap()
            .extend([receive_task, refresh_task, stream_task]);

        // V1.1 firmware is not guaranteed to implement the later 0x7FF/0xCC
        // status query. A disable command is motion-safe and, on legacy
        // firmware, solicits the normal feedback frame immediately.
        if let Err(error) = session.send_special(DISABLE_COMMAND).await {
            session.force_stop();
            return Err(anyhow!("probe DAMIAO feedback with safe disable: {error}"));
        }
        Ok(session)
    }

    pub fn snapshot(&self) -> DamiaoState {
        let now = Instant::now();
        let mut shared = self.state.lock().unwrap();
        let age = shared
            .last_feedback
            .map(|last| now.saturating_duration_since(last));
        let online = age.is_some_and(|value| value <= FEEDBACK_FRESH_FOR);
        let feedback_rate_hz = shared.feedback_rate_hz;
        shared.view.feedback_age_ms = age.map(|value| value.as_millis() as u64);
        shared.view.online = online;
        shared.view.feedback_rate_hz = if online { feedback_rate_hz } else { None };
        shared.view.streaming = self.stream_target.lock().unwrap().is_some();
        shared.view.clone()
    }

    pub async fn enable(&self) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        self.enable_locked().await
    }

    async fn enable_locked(&self) -> Result<()> {
        // The tested DM-J4310 firmware requires the runtime control-mode
        // register to be written before enabling. This is intentionally not
        // saved to flash: every enable transaction establishes the selected
        // mode safely and deterministically.
        let config = self.current_config();
        self.stream_target.lock().unwrap().take();
        self.send_special_to(config.motor_id, DISABLE_COMMAND)
            .await?;
        tokio::time::sleep(MODE_SWITCH_SETTLE).await;
        self.write_control_mode(config.motor_id, config.mode)
            .await?;
        tokio::time::sleep(MODE_SWITCH_SETTLE).await;
        self.send_special_to(config.motor_id, ENABLE_COMMAND).await
    }

    pub async fn disable(&self) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        self.stream_target.lock().unwrap().take();
        self.send_special(DISABLE_COMMAND).await
    }

    pub async fn clear_fault(&self) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        self.send_special(CLEAR_FAULT_COMMAND).await
    }

    pub async fn set_zero(&self) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        // DAMIAO requires zero saving while disabled.  Make the safe state an
        // enforced part of the transaction instead of trusting UI ordering.
        self.stream_target.lock().unwrap().take();
        self.send_special(DISABLE_COMMAND).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.send_special(SET_ZERO_COMMAND).await
    }

    /// Change the volatile 0x0A control-mode register without detaching the
    /// motor. Any periodic target is stopped first. If the last feedback said
    /// the motor was enabled, the transaction restores enable after the new
    /// mode has been written; otherwise the motor remains disabled.
    pub async fn switch_mode(&self, mode: DamiaoMode) -> Result<DamiaoState> {
        let _guard = self.control_gate.lock().await;
        let current = self.current_config();
        if current.mode == mode {
            return Ok(self.snapshot());
        }

        let next = DamiaoConfig { mode, ..current }.validate()?;
        let was_enabled = self.state.lock().unwrap().view.enabled;
        self.stream_target.lock().unwrap().take();

        self.send_special_to(current.motor_id, DISABLE_COMMAND)
            .await?;
        tokio::time::sleep(MODE_SWITCH_SETTLE).await;
        self.write_control_mode(current.motor_id, mode).await?;

        // The mode-write frame has been accepted by the CAN backend. Commit
        // the matching encoder/ID state before any subsequent target can pass
        // through `control_gate`.
        *self.config.lock().unwrap() = next;
        self.state.lock().unwrap().view.mode = mode;

        tokio::time::sleep(MODE_SWITCH_SETTLE).await;
        if was_enabled {
            self.send_special_to(current.motor_id, ENABLE_COMMAND)
                .await?;
        }
        Ok(self.snapshot())
    }

    pub async fn send_target(&self, target: DamiaoTarget, repeat: bool) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        let config = self.current_config();
        let frame = build_target_frame(config, target)?;
        self.bus.send(frame).await.map_err(|error| anyhow!(error))?;
        // Position-velocity mode starts an internal trapezoidal trajectory.
        // Re-sending the same goal at 50 Hz can restart that trajectory on
        // this firmware, so those goals are deliberately one-shot.
        if repeat && config.mode != DamiaoMode::PositionVelocity {
            *self.stream_target.lock().unwrap() = Some(target);
        } else {
            self.stream_target.lock().unwrap().take();
        }
        Ok(())
    }

    pub fn stop_stream(&self) {
        self.stream_target.lock().unwrap().take();
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        self.stream_target.lock().unwrap().take();
        self.send_special(DISABLE_COMMAND).await?;
        self.force_stop();
        Ok(())
    }

    /// Last-resort local teardown for process exit after the disable frame
    /// failed. Manual detach never uses this because it must remain retryable.
    pub fn force_stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        let mut shared = self.state.lock().unwrap();
        shared.view.attached = false;
        shared.view.streaming = false;
    }

    async fn send_special(&self, command: u8) -> Result<()> {
        // Public commands use the configured motor ID. Position/velocity mode
        // offsets apply to motion targets, not to disable/zero/fault commands.
        self.send_special_to(self.current_config().motor_id, command)
            .await
    }

    fn current_config(&self) -> DamiaoConfig {
        *self.config.lock().unwrap()
    }

    async fn write_control_mode(&self, motor_id: u16, mode: DamiaoMode) -> Result<()> {
        let payload = control_mode_payload(motor_id, mode);
        let frame =
            CanFrame::new_data(STATUS_REQUEST_ID, &payload).map_err(|error| anyhow!(error))?;
        self.bus.send(frame).await.map_err(|error| anyhow!(error))
    }

    async fn send_special_to(&self, can_id: u16, command: u8) -> Result<()> {
        let payload = special_payload(command);
        let frame = CanFrame::new_data(can_id, &payload).map_err(|error| anyhow!(error))?;
        self.bus.send(frame).await.map_err(|error| anyhow!(error))
    }
}

async fn receive_loop(
    mut rx: Box<dyn can_transport::CanRx>,
    config: Arc<StdMutex<DamiaoConfig>>,
    state: Arc<StdMutex<SharedState>>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::Acquire) {
        match rx.recv().await {
            Ok(frame) => {
                if frame.kind() != FrameKind::Data || frame.data().len() != 8 {
                    continue;
                }
                // Never mistake a locally echoed 0x7FF status request for a
                // disabled feedback packet while listening in auto-detect mode.
                if frame.id() == CanId::Standard(STATUS_REQUEST_ID) {
                    continue;
                }
                let config = *config.lock().unwrap();
                // A runtime mode-write acknowledgement uses the configured
                // Master ID but is not a normal 8-byte motor feedback packet.
                if is_control_mode_parameter_reply(config, frame.data()) {
                    continue;
                }
                match decode_feedback(config, frame.data()) {
                    Ok(feedback) => {
                        let mut shared = state.lock().unwrap();
                        if let CanId::Standard(id) = frame.id() {
                            shared.view.feedback_can_id = Some(id);
                        }
                        shared.view.enabled = feedback.status_code == 1;
                        shared.view.status_code = feedback.status_code;
                        shared.view.status = status_name(feedback.status_code).into();
                        shared.view.position_rad = Some(feedback.position_rad);
                        shared.view.velocity_rad_s = Some(feedback.velocity_rad_s);
                        shared.view.torque_nm = Some(feedback.torque_nm);
                        shared.view.mos_temp_c = Some(feedback.mos_temp_c);
                        shared.view.rotor_temp_c = Some(feedback.rotor_temp_c);
                        shared.view.rx_count = shared.view.rx_count.saturating_add(1);
                        shared.view.last_error = None;
                        record_feedback_timing(&mut shared, Instant::now());
                    }
                    Err(error) if error.to_string().contains("different motor") => {}
                    Err(error) => set_last_error(&state, error),
                }
            }
            Err(CanIoError::Lagged { dropped }) => {
                set_last_error(
                    &state,
                    anyhow!("DAMIAO feedback dropped {dropped} CAN frames"),
                );
            }
            Err(error) => {
                set_last_error(&state, anyhow!("DAMIAO feedback receive failed: {error}"));
                break;
            }
        }
    }
}

async fn refresh_loop(
    bus: Arc<dyn CanBus>,
    config: Arc<StdMutex<DamiaoConfig>>,
    state: Arc<StdMutex<SharedState>>,
    running: Arc<AtomicBool>,
) {
    let mut ticker = tokio::time::interval(STATUS_REQUEST_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while running.load(Ordering::Acquire) {
        ticker.tick().await;
        let config = *config.lock().unwrap();
        let payload = status_request_payload(config.motor_id);
        let Ok(frame) = CanFrame::new_data(STATUS_REQUEST_ID, &payload) else {
            continue;
        };
        if let Err(error) = bus.send(frame).await {
            set_last_error(&state, anyhow!("request DAMIAO status: {error}"));
            break;
        }
    }
}

async fn stream_loop(
    bus: Arc<dyn CanBus>,
    config: Arc<StdMutex<DamiaoConfig>>,
    state: Arc<StdMutex<SharedState>>,
    target: Arc<StdMutex<Option<DamiaoTarget>>>,
    control_gate: Arc<AsyncMutex<()>>,
    running: Arc<AtomicBool>,
) {
    let mut ticker = tokio::time::interval(TARGET_STREAM_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while running.load(Ordering::Acquire) {
        ticker.tick().await;
        let _guard = control_gate.lock().await;
        let current = *target.lock().unwrap();
        let Some(current) = current else {
            continue;
        };
        let config = *config.lock().unwrap();
        let result = build_target_frame(config, current);
        let result = match result {
            Ok(frame) => bus.send(frame).await.map_err(|error| anyhow!(error)),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            target.lock().unwrap().take();
            set_last_error(&state, anyhow!("stream DAMIAO target: {error}"));
        }
    }
}

fn set_last_error(state: &Arc<StdMutex<SharedState>>, error: anyhow::Error) {
    state.lock().unwrap().view.last_error = Some(error.to_string());
}

fn record_feedback_timing(shared: &mut SharedState, now: Instant) {
    if shared
        .last_feedback
        .is_some_and(|last| now.saturating_duration_since(last) > FEEDBACK_FRESH_FOR)
    {
        shared.feedback_rate_window_start = None;
        shared.feedback_rate_intervals = 0;
        shared.feedback_rate_hz = None;
    }

    match shared.feedback_rate_window_start {
        None => shared.feedback_rate_window_start = Some(now),
        Some(started) => {
            shared.feedback_rate_intervals = shared.feedback_rate_intervals.saturating_add(1);
            let elapsed = now.saturating_duration_since(started);
            if elapsed >= FEEDBACK_RATE_WINDOW {
                shared.feedback_rate_hz =
                    Some(shared.feedback_rate_intervals as f32 / elapsed.as_secs_f32());
                shared.feedback_rate_window_start = Some(now);
                shared.feedback_rate_intervals = 0;
            }
        }
    }
    shared.last_feedback = Some(now);
}

fn control_id(motor_id: u16, mode: DamiaoMode) -> Result<u16> {
    let offset = match mode {
        DamiaoMode::Mit => 0x000,
        DamiaoMode::PositionVelocity => 0x100,
        DamiaoMode::Velocity => 0x200,
    };
    motor_id
        .checked_add(offset)
        .filter(|value| *value <= 0x7FF)
        .ok_or_else(|| anyhow!("DAMIAO control CAN ID exceeds 0x7FF"))
}

fn target_dlc(mode: DamiaoMode) -> usize {
    match mode {
        DamiaoMode::Mit | DamiaoMode::PositionVelocity => 8,
        DamiaoMode::Velocity => 4,
    }
}

fn build_target_frame(config: DamiaoConfig, target: DamiaoTarget) -> Result<CanFrame> {
    let payload = encode_target(config, target)?;
    CanFrame::new_data(
        control_id(config.motor_id, config.mode)?,
        &payload[..target_dlc(config.mode)],
    )
    .map_err(|error| anyhow!(error))
}

fn special_payload(command: u8) -> [u8; 8] {
    [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, command]
}

fn status_request_payload(motor_id: u16) -> [u8; 8] {
    [
        motor_id as u8,
        (motor_id >> 8) as u8,
        STATUS_REQUEST_COMMAND,
        0,
        0,
        0,
        0,
        0,
    ]
}

fn control_mode_payload(motor_id: u16, mode: DamiaoMode) -> [u8; 8] {
    let mode_value: u32 = match mode {
        DamiaoMode::Mit => 1,
        DamiaoMode::PositionVelocity => 2,
        DamiaoMode::Velocity => 3,
    };
    let value = mode_value.to_le_bytes();
    [
        motor_id as u8,
        (motor_id >> 8) as u8,
        WRITE_PARAMETER_COMMAND,
        CONTROL_MODE_REGISTER,
        value[0],
        value[1],
        value[2],
        value[3],
    ]
}

fn is_control_mode_parameter_reply(config: DamiaoConfig, data: &[u8]) -> bool {
    data.len() == 8
        && data[0] == config.motor_id as u8
        && data[1] == (config.motor_id >> 8) as u8
        && matches!(data[2], 0x33 | WRITE_PARAMETER_COMMAND)
        && data[3] == CONTROL_MODE_REGISTER
}

fn encode_target(config: DamiaoConfig, target: DamiaoTarget) -> Result<[u8; 8]> {
    match (config.mode, target) {
        (
            DamiaoMode::Mit,
            DamiaoTarget::Mit {
                position_rad,
                velocity_rad_s,
                torque_nm,
                kp,
                kd,
            },
        ) => {
            let command_v_max = config.v_max.min(DM_J4310_MAX_SPEED_RAD_S);
            let command_t_max = config.t_max.min(DM_J4310_PEAK_TORQUE_NM);
            if !(-command_v_max..=command_v_max).contains(&velocity_rad_s) {
                bail!("velocity exceeds the DM-J4310-2EC V1.1 200 rpm limit");
            }
            if !(-command_t_max..=command_t_max).contains(&torque_nm) {
                bail!("torque exceeds the DM-J4310-2EC V1.1 7 Nm peak limit");
            }
            let position =
                float_to_uint(position_rad, -config.p_max, config.p_max, 16, "position")?;
            let velocity =
                float_to_uint(velocity_rad_s, -config.v_max, config.v_max, 12, "velocity")?;
            let torque = float_to_uint(torque_nm, -config.t_max, config.t_max, 12, "torque")?;
            let kp = float_to_uint(kp, 0.0, 500.0, 12, "Kp")?;
            let kd = float_to_uint(kd, 0.0, 5.0, 12, "Kd")?;
            Ok([
                (position >> 8) as u8,
                position as u8,
                (velocity >> 4) as u8,
                (((velocity & 0xF) << 4) | (kp >> 8)) as u8,
                kp as u8,
                (kd >> 4) as u8,
                (((kd & 0xF) << 4) | (torque >> 8)) as u8,
                torque as u8,
            ])
        }
        (
            DamiaoMode::PositionVelocity,
            DamiaoTarget::PositionVelocity {
                position_rad,
                velocity_rad_s,
            },
        ) => {
            ensure_finite("position", position_rad)?;
            ensure_finite("velocity", velocity_rad_s)?;
            let command_v_max = config.v_max.min(DM_J4310_MAX_SPEED_RAD_S);
            if !(0.0..=command_v_max).contains(&velocity_rad_s) {
                bail!(
                    "position-velocity speed must be in 0..={} rad/s",
                    command_v_max
                );
            }
            let mut payload = [0u8; 8];
            payload[..4].copy_from_slice(&position_rad.to_le_bytes());
            payload[4..].copy_from_slice(&velocity_rad_s.to_le_bytes());
            Ok(payload)
        }
        (DamiaoMode::Velocity, DamiaoTarget::Velocity { velocity_rad_s }) => {
            ensure_finite("velocity", velocity_rad_s)?;
            let command_v_max = config.v_max.min(DM_J4310_MAX_SPEED_RAD_S);
            if !(-command_v_max..=command_v_max).contains(&velocity_rad_s) {
                bail!(
                    "velocity must be in -{}..={} rad/s",
                    command_v_max,
                    command_v_max
                );
            }
            let mut payload = [0u8; 8];
            payload[..4].copy_from_slice(&velocity_rad_s.to_le_bytes());
            Ok(payload)
        }
        (mode, _) => bail!("target kind does not match attached DAMIAO mode {mode:?}"),
    }
}

fn ensure_finite(name: &str, value: f32) -> Result<()> {
    if value.is_finite() {
        Ok(())
    } else {
        bail!("{name} must be finite")
    }
}

fn float_to_uint(value: f32, min: f32, max: f32, bits: u8, name: &str) -> Result<u16> {
    ensure_finite(name, value)?;
    if !(min..=max).contains(&value) {
        bail!("{name} must be in {min}..={max}");
    }
    let scale = ((1u32 << bits) - 1) as f32;
    Ok((((value - min) * scale) / (max - min)) as u16)
}

fn uint_to_float(value: u16, min: f32, max: f32, bits: u8) -> f32 {
    let scale = ((1u32 << bits) - 1) as f32;
    f32::from(value) * (max - min) / scale + min
}

#[derive(Debug, Clone, Copy)]
struct Feedback {
    status_code: u8,
    position_rad: f32,
    velocity_rad_s: f32,
    torque_nm: f32,
    mos_temp_c: u8,
    rotor_temp_c: u8,
}

fn decode_feedback(config: DamiaoConfig, data: &[u8]) -> Result<Feedback> {
    if data.len() != 8 {
        bail!("DAMIAO feedback must contain 8 bytes");
    }
    if data[0] & 0x0F != config.motor_id as u8 & 0x0F {
        bail!("DAMIAO feedback belongs to a different motor");
    }
    let status_code = data[0] >> 4;
    if !matches!(status_code, 0x0 | 0x1 | 0x8..=0xE) {
        bail!("DAMIAO feedback has an invalid status code");
    }
    let position = u16::from_be_bytes([data[1], data[2]]);
    let velocity = (u16::from(data[3]) << 4) | u16::from(data[4] >> 4);
    let torque = (u16::from(data[4] & 0x0F) << 8) | u16::from(data[5]);
    Ok(Feedback {
        status_code,
        position_rad: uint_to_float(position, -config.p_max, config.p_max, 16),
        velocity_rad_s: uint_to_float(velocity, -config.v_max, config.v_max, 12),
        torque_nm: uint_to_float(torque, -config.t_max, config.t_max, 12),
        mos_temp_c: data[6],
        rotor_temp_c: data[7],
    })
}

fn status_name(code: u8) -> &'static str {
    match code {
        0x0 => "Disabled",
        0x1 => "Enabled",
        0x8 => "Over-voltage",
        0x9 => "Under-voltage",
        0xA => "Over-current",
        0xB => "MOS over-temperature",
        0xC => "Motor over-temperature",
        0xD => "Communication lost",
        0xE => "Overload",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use can_transport::CanCapabilities;

    use super::*;

    #[derive(Default)]
    struct RecordingBus {
        frames: StdMutex<Vec<(CanId, Vec<u8>)>>,
    }

    #[async_trait::async_trait]
    impl CanBus for RecordingBus {
        async fn send(&self, frame: CanFrame) -> std::result::Result<(), CanIoError> {
            self.frames
                .lock()
                .unwrap()
                .push((frame.id(), frame.data().to_vec()));
            Ok(())
        }

        async fn subscribe(
            &self,
            _filter: CanFilter,
        ) -> std::result::Result<Box<dyn can_transport::CanRx>, CanIoError> {
            Err(CanIoError::Disconnected)
        }

        fn capabilities(&self) -> CanCapabilities {
            CanCapabilities {
                fd: false,
                max_dlen: 8,
            }
        }
    }

    fn config(mode: DamiaoMode) -> DamiaoConfig {
        DamiaoConfig {
            motor_id: 1,
            master_id: 0,
            mode,
            p_max: 12.5,
            v_max: 30.0,
            t_max: 10.0,
        }
    }

    fn test_session(bus: Arc<dyn CanBus>, mode: DamiaoMode, enabled: bool) -> DamiaoSession {
        let config = config(mode);
        let mut view = DamiaoState::attached(config);
        view.enabled = enabled;
        DamiaoSession {
            bus,
            config: Arc::new(StdMutex::new(config)),
            state: Arc::new(StdMutex::new(SharedState {
                view,
                last_feedback: None,
                feedback_rate_window_start: None,
                feedback_rate_intervals: 0,
                feedback_rate_hz: None,
            })),
            stream_target: Arc::new(StdMutex::new(Some(DamiaoTarget::Mit {
                position_rad: 0.0,
                velocity_rad_s: 0.0,
                torque_nm: 0.0,
                kp: 0.0,
                kd: 0.0,
            }))),
            control_gate: Arc::new(AsyncMutex::new(())),
            running: Arc::new(AtomicBool::new(false)),
            tasks: StdMutex::new(Vec::new()),
        }
    }

    #[test]
    fn measures_real_feedback_rate_and_resets_after_an_outage() {
        let recording = Arc::new(RecordingBus::default());
        let session = test_session(recording, DamiaoMode::Mit, false);
        let started = Instant::now();
        let mut shared = session.state.lock().unwrap();

        for step in 0..=50 {
            record_feedback_timing(&mut shared, started + Duration::from_millis(step * 10));
        }
        let rate = shared.feedback_rate_hz.unwrap();
        assert!((rate - 100.0).abs() < 0.01);

        record_feedback_timing(&mut shared, started + Duration::from_secs(2));
        assert_eq!(shared.feedback_rate_hz, None);
    }

    #[tokio::test]
    async fn switches_mode_at_runtime_and_uses_the_new_target_id_and_dlc() {
        let recording = Arc::new(RecordingBus::default());
        let session = test_session(recording.clone(), DamiaoMode::Mit, true);

        let state = session.switch_mode(DamiaoMode::Velocity).await.unwrap();
        assert_eq!(state.mode, DamiaoMode::Velocity);
        assert!(!state.streaming, "a mode switch must stop the old stream");

        let frames = recording.frames.lock().unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0],
            (
                CanId::Standard(1),
                special_payload(DISABLE_COMMAND).to_vec()
            )
        );
        assert_eq!(
            frames[1],
            (
                CanId::Standard(STATUS_REQUEST_ID),
                control_mode_payload(1, DamiaoMode::Velocity).to_vec()
            )
        );
        assert_eq!(
            frames[2],
            (CanId::Standard(1), special_payload(ENABLE_COMMAND).to_vec())
        );
        drop(frames);

        session
            .send_target(
                DamiaoTarget::Velocity {
                    velocity_rad_s: 5.0,
                },
                true,
            )
            .await
            .unwrap();
        let frames = recording.frames.lock().unwrap();
        assert_eq!(frames[3].0, CanId::Standard(0x201));
        assert_eq!(frames[3].1, 5.0f32.to_le_bytes());
        assert!(session.snapshot().streaming);
    }

    #[test]
    fn uses_documented_target_ids_and_runtime_mode_payloads() {
        assert_eq!(control_id(1, DamiaoMode::Mit).unwrap(), 0x001);
        assert_eq!(control_id(1, DamiaoMode::PositionVelocity).unwrap(), 0x101);
        assert_eq!(control_id(1, DamiaoMode::Velocity).unwrap(), 0x201);
        assert_eq!(target_dlc(DamiaoMode::Mit), 8);
        assert_eq!(target_dlc(DamiaoMode::PositionVelocity), 8);
        assert_eq!(target_dlc(DamiaoMode::Velocity), 4);
        assert_eq!(
            control_mode_payload(4, DamiaoMode::Velocity),
            [0x04, 0x00, 0x55, 0x0A, 0x03, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            control_mode_payload(4, DamiaoMode::PositionVelocity),
            [0x04, 0x00, 0x55, 0x0A, 0x02, 0x00, 0x00, 0x00]
        );
        assert!(is_control_mode_parameter_reply(
            DamiaoConfig {
                motor_id: 4,
                ..config(DamiaoMode::Velocity)
            },
            &[0x04, 0x00, 0x55, 0x0A, 0x03, 0x00, 0x00, 0x00]
        ));
        assert_eq!(
            special_payload(ENABLE_COMMAND),
            [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFC]
        );
        assert_eq!(
            status_request_payload(0x0B),
            [0x0B, 0x00, 0xCC, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn identifies_only_valid_v1_1_feedback_headers() {
        assert_eq!(
            feedback_identity(&[0x1B, 0, 0, 0, 0, 0, 25, 26]),
            Some((0x0B, 0x01))
        );
        assert_eq!(
            feedback_identity(&[0xEC, 0, 0, 0, 0, 0, 25, 26]),
            Some((0x0C, 0x0E))
        );
        assert_eq!(feedback_identity(&[0x2B, 0, 0, 0, 0, 0, 25, 26]), None);
        assert_eq!(feedback_identity(&[0x1B, 0, 0]), None);
        assert!(is_parameter_reply_for(
            4,
            &[0x04, 0x00, 0x55, 0x0A, 0x03, 0x00, 0x00, 0x00]
        ));
    }

    #[test]
    fn encodes_mit_neutral_at_mapping_midpoints() {
        let bytes = encode_target(
            config(DamiaoMode::Mit),
            DamiaoTarget::Mit {
                position_rad: 0.0,
                velocity_rad_s: 0.0,
                torque_nm: 0.0,
                kp: 0.0,
                kd: 0.0,
            },
        )
        .unwrap();
        assert_eq!(bytes, [0x7F, 0xFF, 0x7F, 0xF0, 0x00, 0x00, 0x07, 0xFF]);
    }

    #[test]
    fn position_velocity_and_velocity_are_little_endian_floats() {
        let pv = encode_target(
            config(DamiaoMode::PositionVelocity),
            DamiaoTarget::PositionVelocity {
                position_rad: 1.25,
                velocity_rad_s: 2.5,
            },
        )
        .unwrap();
        assert_eq!(&pv[..4], &1.25f32.to_le_bytes());
        assert_eq!(&pv[4..], &2.5f32.to_le_bytes());

        let pv_frame = build_target_frame(
            DamiaoConfig {
                motor_id: 4,
                ..config(DamiaoMode::PositionVelocity)
            },
            DamiaoTarget::PositionVelocity {
                position_rad: 1.25,
                velocity_rad_s: 2.5,
            },
        )
        .unwrap();
        assert_eq!(pv_frame.id(), CanId::Standard(0x104));
        assert_eq!(pv_frame.data().len(), 8);
        assert_eq!(
            pv_frame.data(),
            &[0x00, 0x00, 0xA0, 0x3F, 0x00, 0x00, 0x20, 0x40]
        );

        let velocity = encode_target(
            config(DamiaoMode::Velocity),
            DamiaoTarget::Velocity {
                velocity_rad_s: 5.0,
            },
        )
        .unwrap();
        assert_eq!(&velocity[..4], &[0x00, 0x00, 0xA0, 0x40]);
        assert_eq!(target_dlc(DamiaoMode::Velocity), 4);
    }

    #[test]
    fn decodes_feedback_status_and_mapping() {
        let feedback = decode_feedback(
            config(DamiaoMode::Mit),
            &[0x11, 0x7F, 0xFF, 0x7F, 0xF7, 0xFF, 42, 39],
        )
        .unwrap();
        assert_eq!(feedback.status_code, 1);
        assert!(feedback.position_rad.abs() < 0.001);
        assert!(feedback.velocity_rad_s.abs() < 0.02);
        assert!(feedback.torque_nm.abs() < 0.01);
        assert_eq!(feedback.mos_temp_c, 42);
        assert_eq!(feedback.rotor_temp_c, 39);
    }

    #[test]
    fn rejects_mismatched_mode_and_out_of_range_mit_values() {
        assert!(encode_target(
            config(DamiaoMode::Velocity),
            DamiaoTarget::Mit {
                position_rad: 0.0,
                velocity_rad_s: 0.0,
                torque_nm: 0.0,
                kp: 0.0,
                kd: 0.0,
            }
        )
        .is_err());
        assert!(encode_target(
            config(DamiaoMode::Mit),
            DamiaoTarget::Mit {
                position_rad: 13.0,
                velocity_rad_s: 0.0,
                torque_nm: 0.0,
                kp: 0.0,
                kd: 0.0,
            }
        )
        .is_err());
    }
}
