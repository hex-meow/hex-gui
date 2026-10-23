//! Direct CAN control for the DAMIAO DM-J4310-2EC geared motor.
//!
//! This motor does not speak CiA 402.  The session only borrows the physical
//! bus opened by the v1.4 connection. A bus-level discovery monitor finds motors,
//! while one independent session per motor implements the DAMIAO MIT /
//! position-velocity / velocity / force-position frame formats.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use can_transport::{CanBus, CanFilter, CanFrame, CanId, CanIoError, FrameKind};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

#[path = "damiao_registers.rs"]
mod registers;
pub use registers::{
    register_definition, DamiaoRegisterDefinition, DamiaoRegisterReading, REGISTERS,
};

const STATUS_REQUEST_ID: u16 = 0x7FF;
const STATUS_REQUEST_COMMAND: u8 = 0xCC;
const ENABLE_COMMAND: u8 = 0xFC;
const DISABLE_COMMAND: u8 = 0xFD;
const SET_ZERO_COMMAND: u8 = 0xFE;
const CLEAR_FAULT_COMMAND: u8 = 0xFB;
const READ_PARAMETER_COMMAND: u8 = 0x33;
const WRITE_PARAMETER_COMMAND: u8 = 0x55;
const SAVE_PARAMETERS_COMMAND: u8 = 0xAA;
const MOTOR_ID_REGISTER: u8 = 0x08;
const CONTROL_MODE_REGISTER: u8 = 0x0A;
const CAN_BITRATE_REGISTER: u8 = 0x23;
const POSITION_MAPPING_REGISTER: u8 = 0x15;
const VELOCITY_MAPPING_REGISTER: u8 = 0x16;
const TORQUE_MAPPING_REGISTER: u8 = 0x17;
const PRECISE_POSITION_REGISTER: u8 = 0x50;
const OUTPUT_POSITION_REGISTER: u8 = 0x51;
const POSITION_READ_PERIOD: Duration = Duration::from_millis(200);
const POSITION_READ_TIMEOUT: Duration = Duration::from_millis(100);
const PARAMETER_RESPONSE_TIMEOUT: Duration = Duration::from_millis(800);
const PARAMETER_SAVE_SETTLE: Duration = Duration::from_millis(30);
const DOCUMENTED_BITRATES: [u32; 10] = [
    125_000, 200_000, 250_000, 500_000, 1_000_000, 2_000_000, 2_500_000, 3_200_000, 4_000_000,
    5_000_000,
];

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
#[cfg(test)]
const DM_J4310_MAPPING_P_MAX_RAD: f32 = 12.5;
#[cfg(test)]
const DM_J4310_MAPPING_V_MAX_RAD_S: f32 = 30.0;
#[cfg(test)]
const DM_J4310_MAPPING_T_MAX_NM: f32 = 10.0;
// Allow a few feedback quantization counts (16-bit position, 12-bit speed)
// when checking a stationary shaft and the drive's newly saved zero point.
const ZERO_POSITION_TOLERANCE_RAD: f32 = 0.002;
const ZERO_VELOCITY_TOLERANCE_RAD_S: f32 = 0.05;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DamiaoMode {
    /// Register value 0 has no documented motion protocol.
    Unspecified,
    Mit,
    PositionVelocity,
    Velocity,
    ForcePosition,
}

impl DamiaoMode {
    fn from_register(value: u32) -> Result<Self> {
        match value {
            0 => Ok(Self::Unspecified),
            1 => Ok(Self::Mit),
            2 => Ok(Self::PositionVelocity),
            3 => Ok(Self::Velocity),
            4 => Ok(Self::ForcePosition),
            _ => bail!("Unsupported DAMIAO control mode {value}"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DamiaoRegisterWriteResult {
    pub reading: Option<DamiaoRegisterReading>,
    pub state: DamiaoState,
    pub id_change: Option<DamiaoIdChangeResult>,
    pub bitrate: Option<DamiaoBitrateState>,
}

/// A value of the volatile CAN bitrate register; `acknowledged` distinguishes
/// a confirmed value from an uncertain write. Rates above
/// 1 Mbps select CAN FD feedback; the host interface must be configured and
/// reopened separately before further motor commands can be sent.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DamiaoBitrateState {
    pub motor_id: u16,
    pub bitrate: u32,
    pub register_code: u32,
    pub can_fd: bool,
    pub acknowledged: bool,
    pub requires_reconnect: bool,
}

impl DamiaoBitrateState {
    fn confirmed(motor_id: u16, register_code: u32, requires_reconnect: bool) -> Result<Self> {
        let bitrate = *DOCUMENTED_BITRATES
            .get(register_code as usize)
            .ok_or_else(|| {
                anyhow!("DAMIAO reported unsupported CAN bitrate code {register_code}")
            })?;
        Ok(Self {
            motor_id,
            bitrate,
            register_code,
            can_fd: bitrate > 1_000_000,
            acknowledged: true,
            requires_reconnect,
        })
    }
}

/// A verified address change can survive a later Flash-save failure. Return
/// both facts so the caller always keeps controlling the confirmed address.
#[derive(Debug, Clone, Serialize)]
pub struct DamiaoIdChangeResult {
    pub previous_motor_id: u16,
    pub requested_motor_id: u16,
    pub verified: bool,
    pub saved: bool,
    pub error: Option<String>,
    pub state: DamiaoState,
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
        // ESC_ID is uint32 with an 11-bit documented range. Control frames
        // match its low 8 bits; feedback carries only the low 4 ID bits.
        if self.motor_id > 0x7FF {
            bail!("DAMIAO motor CAN ID must be in 0x000..=0x7FF");
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
        // PMAX / VMAX / TMAX are configurable protocol scales, not the
        // drive's mechanical limits. The actual scales are read at attach.
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
    ForcePosition {
        position_rad: f32,
        velocity_rad_s: f32,
        current_limit_ratio: f32,
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
    pub requires_reconnect: bool,
    pub status_code: u8,
    pub status: String,
    /// Authoritative control coordinate from float p_m (0x50). This can
    /// exceed PMAX, unlike the cyclic 16-bit feedback representation.
    pub position_rad: Option<f32>,
    /// Decoded 16-bit feedback position, retained only for diagnostics.
    pub feedback_position_rad: Option<f32>,
    /// Float p_m (0x50): rotor position referred to the output shaft.
    pub precise_position_rad: Option<f32>,
    /// Float xout (0x51): measured by the output-shaft encoder itself.
    pub output_position_rad: Option<f32>,
    pub position_read_age_ms: Option<u64>,
    pub position_read_count: u64,
    pub position_read_error: Option<String>,
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
            requires_reconnect: false,
            status_code: 0,
            status: "waiting for feedback".into(),
            position_rad: None,
            feedback_position_rad: None,
            precise_position_rad: None,
            output_position_rad: None,
            position_read_age_ms: None,
            position_read_count: 0,
            position_read_error: None,
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
    scan_gate: Arc<AsyncMutex<()>>,
    running: Arc<AtomicBool>,
    request_safe_sweep: Arc<AtomicBool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl DamiaoDiscovery {
    pub async fn start(bus: Arc<dyn CanBus>) -> Result<Arc<Self>> {
        let rx = bus
            .subscribe(CanFilter::pass_all_standard())
            .await
            .map_err(|error| anyhow!("subscribe DAMIAO discovery feedback: {error}"))?;
        let state = Arc::new(StdMutex::new(DiscoveryState::default()));
        let running = Arc::new(AtomicBool::new(true));
        let request_safe_sweep = Arc::new(AtomicBool::new(false));
        let scan_gate = Arc::new(AsyncMutex::new(()));
        let discovery = Arc::new(Self {
            state: state.clone(),
            scan_gate: scan_gate.clone(),
            running: running.clone(),
            request_safe_sweep: request_safe_sweep.clone(),
            tasks: StdMutex::new(Vec::new()),
        });

        let receive_task = tokio::spawn(discovery_receive_loop(rx, state.clone(), running.clone()));
        let scan_task = tokio::spawn(discovery_scan_loop(
            bus,
            state,
            running,
            request_safe_sweep,
            scan_gate,
        ));
        discovery
            .tasks
            .lock()
            .unwrap()
            .extend([receive_task, scan_task]);
        Ok(discovery)
    }

    pub fn snapshot(&self) -> Vec<DamiaoDiscoveredDevice> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        prune_discovery_devices(&mut state, now);
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

    /// Wait for any in-flight probe and hold discovery sends during an ID
    /// transaction, including the migration of the attached-session map.
    pub async fn suspend_scanning(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.scan_gate.clone().lock_owned().await
    }

    pub fn rename_attached(&self, previous_motor_id: u16, snapshot: &DamiaoState) {
        let mut shared = self.state.lock().unwrap();
        shared.attached.remove(&previous_motor_id);
        shared.devices.remove(&previous_motor_id);
        shared.pending = None;
        shared.attached.insert(snapshot.motor_id);
        if snapshot.online {
            if let Some(feedback_can_id) = snapshot.feedback_can_id {
                shared.devices.insert(
                    snapshot.motor_id,
                    DiscoveredEntry {
                        feedback_can_id,
                        status_code: snapshot.status_code,
                        last_feedback: Instant::now(),
                        rx_count: snapshot.rx_count,
                    },
                );
            }
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

    /// Await cancellation before a bus timing transaction: an in-progress
    /// scan must not enqueue another old-rate frame after the register write.
    pub async fn stop_and_wait(&self) {
        self.running.store(false, Ordering::SeqCst);
        let tasks: Vec<_> = self.tasks.lock().unwrap().drain(..).collect();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
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
                if !is_data_frame(&frame)
                    || frame.data().len() != 8
                    || frame.id() == CanId::Standard(STATUS_REQUEST_ID)
                {
                    continue;
                }
                let Some((motor_id, status_code)) = feedback_identity(frame.data()) else {
                    continue;
                };
                let parameter_motor_id = u16::from_le_bytes([frame.data()[0], frame.data()[1]]);
                if parameter_motor_id <= 0x7FF
                    && is_parameter_reply_for(parameter_motor_id, frame.data())
                {
                    continue;
                }
                let CanId::Standard(feedback_can_id) = frame.id() else {
                    continue;
                };
                let now = Instant::now();
                let mut shared = state.lock().unwrap();
                record_discovery_feedback(&mut shared, motor_id, feedback_can_id, status_code, now);
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
    scan_gate: Arc<AsyncMutex<()>>,
) {
    let mut ticker = tokio::time::interval(DISCOVERY_STEP);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut motor_id = 0u16;
    let mut sweep = 0u32;
    let mut force_safe_sweep = true;
    while running.load(Ordering::Acquire) {
        ticker.tick().await;
        let _scan_guard = scan_gate.lock().await;
        if motor_id == 0 && request_safe_sweep.swap(false, Ordering::AcqRel) {
            force_safe_sweep = true;
        }
        let use_legacy_disable = {
            let mut shared = state.lock().unwrap();
            prune_discovery_devices(&mut shared, Instant::now());
            let stale_or_unknown = shared.devices.get(&motor_id).map_or(true, |entry| {
                Instant::now().saturating_duration_since(entry.last_feedback) > DISCOVERY_FRESH_FOR
            });
            !shared.attached.iter().any(|id| id & 0x0F == motor_id)
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
            let mut shared = state.lock().unwrap();
            if shared
                .pending
                .is_some_and(|probe| probe.motor_id == motor_id)
            {
                shared.pending = None;
            }
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

fn record_discovery_feedback(
    shared: &mut DiscoveryState,
    motor_id: u16,
    feedback_can_id: u16,
    status_code: u8,
    now: Instant,
) -> bool {
    // Attached exceptional IDs still report only their low nibble. Keep
    // their telemetry under the full configured ID after an address change.
    let motor_id = shared
        .attached
        .iter()
        .copied()
        .find(|id| id & 0x0F == motor_id)
        .unwrap_or(motor_id);
    let expected = shared.pending.is_some_and(|probe| {
        probe.motor_id == motor_id
            && now.saturating_duration_since(probe.sent_at) <= DISCOVERY_RESPONSE_WINDOW
    });
    if expected {
        shared.pending = None;
    }

    let known_and_fresh = shared.devices.get(&motor_id).is_some_and(|entry| {
        now.saturating_duration_since(entry.last_feedback) <= DISCOVERY_FRESH_FOR
    });
    if !expected && !known_and_fresh && !shared.attached.contains(&motor_id) {
        return false;
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
    true
}

fn prune_discovery_devices(shared: &mut DiscoveryState, now: Instant) {
    let attached = shared.attached.clone();
    shared.devices.retain(|motor_id, entry| {
        attached.contains(motor_id)
            || now.saturating_duration_since(entry.last_feedback) <= DISCOVERY_FRESH_FOR
    });
}

fn feedback_identity(data: &[u8]) -> Option<(u16, u8)> {
    if data.len() != 8 {
        return None;
    }
    let status_code = data[0] >> 4;
    if !valid_status_code(status_code) {
        return None;
    }
    Some((u16::from(data[0] & 0x0F), status_code))
}

fn is_parameter_reply_for(motor_id: u16, data: &[u8]) -> bool {
    data.len() >= 4
        && data[0] == motor_id as u8
        && data[1] == (motor_id >> 8) as u8
        && matches!(
            data[2],
            READ_PARAMETER_COMMAND | WRITE_PARAMETER_COMMAND | SAVE_PARAMETERS_COMMAND
        )
}

fn is_data_frame(frame: &CanFrame) -> bool {
    matches!(frame.kind(), FrameKind::Data | FrameKind::Fd { .. })
}

fn is_request_echo(data: &[u8]) -> bool {
    data.len() >= 4
        && data[1] <= 7
        && matches!(
            data[2],
            READ_PARAMETER_COMMAND
                | WRITE_PARAMETER_COMMAND
                | SAVE_PARAMETERS_COMMAND
                | STATUS_REQUEST_COMMAND
        )
}

fn valid_status_code(code: u8) -> bool {
    matches!(code, 0x0 | 0x1 | 0x3..=0x5 | 0x8..=0xE)
}

struct SharedState {
    view: DamiaoState,
    last_feedback: Option<Instant>,
    last_position_read: Option<Instant>,
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
    /// Block traffic when a bitrate or address write leaves communication
    /// uncertain. Reopening the bus and attaching again clears this.
    communication_requires_reconnect: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl DamiaoSession {
    pub async fn start(bus: Arc<dyn CanBus>, config: DamiaoConfig) -> Result<Arc<Self>> {
        let config = config.validate()?;
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
            last_position_read: None,
            feedback_rate_window_start: None,
            feedback_rate_intervals: 0,
            feedback_rate_hz: None,
        }));
        let config = Arc::new(StdMutex::new(config));
        let stream_target = Arc::new(StdMutex::new(None));
        let control_gate = Arc::new(AsyncMutex::new(()));
        let communication_requires_reconnect = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));

        let session = Arc::new(Self {
            bus: bus.clone(),
            config: config.clone(),
            state: state.clone(),
            stream_target: stream_target.clone(),
            control_gate: control_gate.clone(),
            communication_requires_reconnect: communication_requires_reconnect.clone(),
            running: running.clone(),
            tasks: StdMutex::new(Vec::new()),
        });

        // Read the drive's scales before any telemetry is decoded or targets
        // can be sent. UI defaults must never decide a live motor's units.
        session.sync_mapping_locked().await.map_err(|error| {
            anyhow!("read DAMIAO position/velocity/torque mapping before attach: {error}")
        })?;

        let receive_task = tokio::spawn(receive_loop(
            rx,
            config.clone(),
            state.clone(),
            running.clone(),
            communication_requires_reconnect.clone(),
        ));
        let refresh_task = tokio::spawn(refresh_loop(
            bus.clone(),
            config.clone(),
            state.clone(),
            running.clone(),
            control_gate.clone(),
            communication_requires_reconnect.clone(),
        ));
        let stream_task = tokio::spawn(stream_loop(
            bus,
            config,
            state,
            stream_target.clone(),
            control_gate,
            running,
        ));
        session.tasks.lock().unwrap().extend([
            receive_task,
            refresh_task,
            stream_task,
            tokio::spawn(position_loop(Arc::downgrade(&session))),
        ]);

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
        let online =
            !self.requires_reconnect() && age.is_some_and(|value| value <= FEEDBACK_FRESH_FOR);
        let feedback_rate_hz = shared.feedback_rate_hz;
        shared.view.feedback_age_ms = age.map(|value| value.as_millis() as u64);
        shared.view.online = online;
        shared.view.feedback_rate_hz = if online { feedback_rate_hz } else { None };
        shared.view.streaming = self.stream_target.lock().unwrap().is_some();
        shared.view.requires_reconnect = self.requires_reconnect();
        let position_age = shared
            .last_position_read
            .map(|last| now.saturating_duration_since(last));
        shared.view.position_read_age_ms = position_age.map(|value| value.as_millis() as u64);
        if self.requires_reconnect()
            || !position_age.is_some_and(|value| value <= FEEDBACK_FRESH_FOR)
        {
            shared.view.position_rad = None;
            shared.view.precise_position_rad = None;
            shared.view.output_position_rad = None;
        }
        shared.view.clone()
    }

    pub async fn enable(&self) -> Result<DamiaoState> {
        let _guard = self.control_gate.lock().await;
        let result = async {
            self.prepare_mode_locked(self.current_config().mode).await?;
            self.confirm_special_locked(ENABLE_COMMAND, 1).await?;
            self.state.lock().unwrap().view.last_error = None;
            Ok(self.snapshot())
        }
        .await;
        self.finish_mode_operation(result).await
    }

    /// Establish a known mode and neutral target before enabling. The manual
    /// recommends reading p_m (0x50) before entering a position mode because
    /// writing CTRL_MODE clears the drive's previous target to zero.
    async fn prepare_mode_locked(&self, mode: DamiaoMode) -> Result<()> {
        self.ensure_communication_ready()?;
        if mode == DamiaoMode::Unspecified {
            bail!("CTRL_MODE 0 has no documented motion behavior; select a mode from 1 to 4 before enabling");
        }
        self.stream_target.lock().unwrap().take();
        let disabled = self.confirm_disabled_locked().await?;
        if disabled.velocity_rad_s.abs() > ZERO_VELOCITY_TOLERANCE_RAD_S {
            bail!("DAMIAO shaft is still moving ({:.3} rad/s); wait for it to stop before enabling or switching mode", disabled.velocity_rad_s);
        }
        let position = if mode != DamiaoMode::Velocity {
            self.read_positions_locked().await?.position_rad
        } else {
            None
        };
        let next = DamiaoConfig {
            mode,
            ..self.current_config()
        }
        .validate()?;
        self.write_control_mode(next.motor_id, mode).await?;
        *self.config.lock().unwrap() = next;
        self.state.lock().unwrap().view.mode = mode;
        tokio::time::sleep(MODE_SWITCH_SETTLE).await;
        let neutral = match mode {
            DamiaoMode::Unspecified => unreachable!(),
            DamiaoMode::Mit => DamiaoTarget::Mit {
                position_rad: position.unwrap_or(0.0).clamp(-next.p_max, next.p_max),
                velocity_rad_s: 0.0,
                torque_nm: 0.0,
                kp: 0.0,
                kd: 0.0,
            },
            DamiaoMode::PositionVelocity => DamiaoTarget::PositionVelocity {
                position_rad: position
                    .ok_or_else(|| anyhow!("DAMIAO precise position is unavailable"))?,
                velocity_rad_s: 0.0,
            },
            DamiaoMode::Velocity => DamiaoTarget::Velocity {
                velocity_rad_s: 0.0,
            },
            DamiaoMode::ForcePosition => DamiaoTarget::ForcePosition {
                position_rad: position
                    .ok_or_else(|| anyhow!("DAMIAO precise position is unavailable"))?,
                velocity_rad_s: 0.0,
                current_limit_ratio: 0.0,
            },
        };
        self.bus.send(build_target_frame(next, neutral)?).await?;
        Ok(())
    }

    async fn finish_mode_operation(&self, result: Result<DamiaoState>) -> Result<DamiaoState> {
        if let Err(error) = result {
            self.stream_target.lock().unwrap().take();
            // An enable may have reached the drive even if its reply was lost.
            // Attempt a confirmed disable, and report failure of that recovery.
            let recovery = if self.requires_reconnect() {
                Ok(())
            } else {
                self.confirm_disabled_locked().await.map(|_| ())
            };
            let error = match recovery {
                Ok(()) => error,
                Err(stop_error) => anyhow!("{error}; disable recovery also failed: {stop_error}"),
            };
            self.state.lock().unwrap().view.last_error = Some(error.to_string());
            return Err(error);
        }
        result
    }

    pub async fn disable(&self) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        self.stream_target.lock().unwrap().take();
        self.ensure_communication_ready()?;
        self.confirm_disabled_locked().await.map(|_| ())
    }

    pub async fn clear_fault(&self) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        self.stream_target.lock().unwrap().take();
        self.send_special(CLEAR_FAULT_COMMAND).await
    }

    /// Save the current output-shaft position as the drive's zero point.
    /// The documented 0xFE command saves the zero itself; it must not be
    /// followed by a save-all-parameters command or an automatic retry.
    pub async fn set_zero(&self) -> Result<DamiaoState> {
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        self.stream_target.lock().unwrap().take();
        // Confirm a fresh response to disable instead of assuming that a
        // successful host transmission or a fixed sleep stopped the motor.
        let disabled = self.confirm_disabled_locked().await?;
        if disabled.velocity_rad_s.abs() > ZERO_VELOCITY_TOLERANCE_RAD_S {
            bail!("DAMIAO output shaft is still moving ({:.3} rad/s); wait until it stops before saving zero. The zero-save command was not sent", disabled.velocity_rad_s);
        }

        let config = self.current_config();
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        // Discard telemetry already queued before sending 0xFE. Only a new
        // response can verify the motor's position after the zero command.
        while rx.try_recv()?.is_some() {}
        // A zero change invalidates both position-register readings. They
        // will be refreshed under the new reference after this transaction.
        {
            let mut shared = self.state.lock().unwrap();
            shared.last_position_read = None;
            shared.view.position_rad = None;
            shared.view.precise_position_rad = None;
            shared.view.output_position_rad = None;
            shared.view.position_read_age_ms = None;
        }
        self.send_special_to(config.motor_id, SET_ZERO_COMMAND)
            .await
            .map_err(|error| {
                anyhow!("DAMIAO zero-save transmit failed; the saved zero is uncertain: {error}")
            })?;
        let result = tokio::time::timeout(PARAMETER_RESPONSE_TIMEOUT, async {
            let mut status_requests = tokio::time::interval(STATUS_REQUEST_PERIOD);
            status_requests.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Give the zero command's own feedback time to arrive before
            // polling. Refresh is otherwise paused by this control gate.
            status_requests.tick().await;
            loop {
                let received = tokio::select! {
                    received = rx.recv() => received,
                    _ = status_requests.tick() => {
                        self.bus.send(CanFrame::new_data(STATUS_REQUEST_ID, &status_request_payload(config.motor_id))?).await?;
                        continue;
                    }
                };
                let frame = match received {
                    Ok(frame) => frame,
                    Err(CanIoError::Lagged { .. }) => continue,
                    Err(error) => bail!("receive DAMIAO zero confirmation: {error}"),
                };
                let Some((can_id, feedback)) = matching_feedback(config, feedback_can_id, &frame)?
                else {
                    continue;
                };
                if feedback.status_code == 0
                    && feedback.position_rad.abs() <= ZERO_POSITION_TOLERANCE_RAD
                    && feedback.velocity_rad_s.abs() <= ZERO_VELOCITY_TOLERANCE_RAD_S
                {
                    // The 16-bit field can alias -25 rad (or any multiple
                    // of 2*PMAX) to zero. Confirm the actual float position.
                    let positions = self.read_positions_locked().await?;
                    if positions.position_rad.is_some_and(|position| {
                        position.abs() <= ZERO_POSITION_TOLERANCE_RAD
                    }) {
                        return Ok(self.store_confirmed_feedback(can_id, feedback));
                    }
                }
            }
        })
        .await
        .map_err(|_| anyhow!("no disabled, stationary near-zero feedback within {} ms", PARAMETER_RESPONSE_TIMEOUT.as_millis()))
        .and_then(|result| result);
        match result {
            Ok(state) => Ok(state),
            Err(error) => {
                let error = anyhow!("DAMIAO zero-save command was sent, but the new zero was not confirmed; the saved zero is uncertain. Check actual feedback before retrying: {error}");
                self.state.lock().unwrap().view.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Change the volatile 0x0A control-mode register without detaching the
    /// motor. Any periodic target is stopped first. If the last feedback said
    /// the motor was enabled, the transaction restores enable after the new
    /// mode has been written; otherwise the motor remains disabled.
    pub async fn switch_mode(&self, mode: DamiaoMode) -> Result<DamiaoState> {
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        let current = self.current_config();
        if current.mode == mode {
            return Ok(self.snapshot());
        }

        let before = self.snapshot();
        let was_enabled = before.online && before.enabled;
        let result = async {
            self.prepare_mode_locked(mode).await?;
            if was_enabled {
                self.confirm_special_locked(ENABLE_COMMAND, 1).await?;
            }
            self.state.lock().unwrap().view.last_error = None;
            Ok(self.snapshot())
        }
        .await;
        self.finish_mode_operation(result).await
    }

    pub async fn send_target(&self, target: DamiaoTarget, repeat: bool) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        let state = self.snapshot();
        if !state.online || !state.enabled || state.status_code != 1 {
            bail!("DAMIAO motion requires fresh enabled, fault-free feedback");
        }
        let config = self.current_config();
        let frame = build_target_frame(config, target)?;
        self.bus.send(frame).await.map_err(|error| anyhow!(error))?;
        // Position-velocity mode starts an internal trapezoidal trajectory.
        // Re-sending the same goal at 50 Hz can restart that trajectory on
        // this firmware, so those goals are deliberately one-shot.
        if repeat
            && !matches!(
                config.mode,
                DamiaoMode::PositionVelocity | DamiaoMode::ForcePosition
            )
        {
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
        if self.requires_reconnect() {
            // A confirmed disable precedes every bitrate write. Teardown
            // must not send another frame through the host's stale timing.
            self.force_stop();
            return Ok(());
        }
        self.send_special(DISABLE_COMMAND).await?;
        self.force_stop();
        Ok(())
    }

    pub fn requires_reconnect(&self) -> bool {
        self.communication_requires_reconnect
            .load(Ordering::Acquire)
    }

    fn ensure_communication_ready(&self) -> Result<()> {
        if self.requires_reconnect() {
            bail!("DAMIAO communication changed or is uncertain; reconnect and verify the motor ID and CAN bitrate before sending commands");
        }
        Ok(())
    }

    pub async fn read_register(&self, address: u8) -> Result<DamiaoRegisterReading> {
        let definition = register_definition(address)?;
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        Ok(definition.decode(self.read_parameter_locked(address).await?))
    }

    /// ESC_ID and can_br must pass through the app-level address/bitrate
    /// transactions. All other documented RW registers use this transaction.
    pub async fn write_register(&self, address: u8, value: f64) -> Result<DamiaoRegisterReading> {
        let definition = register_definition(address)?;
        let raw = definition.encode(value)?;
        if matches!(address, MOTOR_ID_REGISTER | CAN_BITRATE_REGISTER) {
            bail!("Use the dedicated motor ID / bitrate transaction for this register");
        }
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        self.stream_target.lock().unwrap().take();
        self.confirm_disabled_locked().await?;
        let affects_session = matches!(address, 0x07 | CONTROL_MODE_REGISTER | 0x15..=0x17);
        let result = async {
            if address == CONTROL_MODE_REGISTER {
                let mode = DamiaoMode::from_register(raw)?;
                if mode == DamiaoMode::Unspecified {
                    self.write_register_raw_locked(address, raw).await?;
                    self.config.lock().unwrap().mode = mode;
                    self.state.lock().unwrap().view.mode = mode;
                } else {
                    // Preload current position, zero velocity and zero current;
                    // configuration writes always leave the motor disabled.
                    self.prepare_mode_locked(mode).await?;
                }
                let bytes = self.read_parameter_locked(address).await?;
                if u32::from_le_bytes(bytes) != raw {
                    bail!("CTRL_MODE readback does not match the requested mode");
                }
                return Ok(definition.decode(bytes));
            }

            if affects_session {
                // Suspend feedback decoding as well as all background traffic
                // until new routing / mapping has been confirmed.
                self.communication_requires_reconnect
                    .store(true, Ordering::Release);
            }
            let bytes = if address == 0x07 {
                self.write_master_id_locked(raw as u16).await?
            } else {
                self.write_register_raw_locked(address, raw).await?
            };
            if matches!(address, 0x15..=0x17) {
                self.sync_mapping_locked().await?;
                let mut shared = self.state.lock().unwrap();
                shared.last_feedback = None;
                shared.view.feedback_position_rad = None;
                shared.view.velocity_rad_s = None;
                shared.view.torque_nm = None;
            }
            Ok(definition.decode(bytes))
        }
        .await;
        match result {
            Ok(reading) => {
                self.communication_requires_reconnect
                    .store(false, Ordering::Release);
                self.state.lock().unwrap().view.last_error = None;
                Ok(reading)
            }
            Err(error) => {
                // A write may have taken effect even if its reply was lost.
                // Never enable or stream with uncertain mode/mapping/routing.
                if affects_session {
                    self.communication_requires_reconnect
                        .store(true, Ordering::Release);
                }
                let message = format!(
                    "{} write was not verified; read back before retrying{}: {error}",
                    definition.name,
                    if affects_session {
                        "; reconnect CAN first"
                    } else {
                        ""
                    }
                );
                self.state.lock().unwrap().view.last_error = Some(message.clone());
                bail!("{message}")
            }
        }
    }

    async fn write_register_raw_locked(&self, address: u8, raw: u32) -> Result<[u8; 4]> {
        let motor_id = self.current_config().motor_id;
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        self.bus
            .send(CanFrame::new_data(
                STATUS_REQUEST_ID,
                &parameter_write_payload(motor_id, address, raw),
            )?)
            .await?;
        // A missing write acknowledgement does not justify repeating a write.
        // Always perform a separate read and compare the four on-wire bytes.
        let _ = wait_parameter_reply(
            &mut *rx,
            feedback_can_id,
            motor_id,
            WRITE_PARAMETER_COMMAND,
            address,
            Some(raw),
        )
        .await;
        let bytes = self.read_parameter_locked(address).await?;
        if u32::from_le_bytes(bytes) != raw {
            bail!("Register 0x{address:02X} readback differs from the requested value");
        }
        Ok(bytes)
    }

    async fn write_master_id_locked(&self, master_id: u16) -> Result<[u8; 4]> {
        let motor_id = self.current_config().motor_id;
        self.bus
            .send(CanFrame::new_data(
                STATUS_REQUEST_ID,
                &parameter_write_payload(motor_id, 0x07, u32::from(master_id)),
            )?)
            .await?;
        // The write reply can arrive on either the previous or the new ID.
        // Verify using a fresh read subscribed exclusively to the new ID.
        let mut rx = self
            .bus
            .subscribe(CanFilter::exact_standard(master_id))
            .await?;
        self.bus
            .send(CanFrame::new_data(
                STATUS_REQUEST_ID,
                &[
                    motor_id as u8,
                    (motor_id >> 8) as u8,
                    READ_PARAMETER_COMMAND,
                    0x07,
                ],
            )?)
            .await?;
        let reply = wait_parameter_reply(
            &mut *rx,
            Some(master_id),
            motor_id,
            READ_PARAMETER_COMMAND,
            0x07,
            Some(u32::from(master_id)),
        )
        .await?;
        self.config.lock().unwrap().master_id = master_id;
        let mut shared = self.state.lock().unwrap();
        shared.view.master_id = master_id;
        shared.view.feedback_can_id = Some(master_id);
        shared.last_feedback = None;
        Ok(reply.data()[4..8].try_into().unwrap())
    }

    async fn read_parameter_locked(&self, register: u8) -> Result<[u8; 4]> {
        let config = self.current_config();
        self.read_parameter_at_locked(config.motor_id, register)
            .await
    }

    async fn read_parameter_at_locked(&self, motor_id: u16, register: u8) -> Result<[u8; 4]> {
        let request = [
            motor_id as u8,
            (motor_id >> 8) as u8,
            READ_PARAMETER_COMMAND,
            register,
        ];
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        self.bus
            .send(CanFrame::new_data(STATUS_REQUEST_ID, &request)?)
            .await?;
        let reply = wait_parameter_reply(
            &mut *rx,
            feedback_can_id,
            motor_id,
            READ_PARAMETER_COMMAND,
            register,
            None,
        )
        .await?;
        if let CanId::Standard(id) = reply.id() {
            self.state.lock().unwrap().view.feedback_can_id = Some(id);
        }
        Ok(reply.data()[4..8].try_into().unwrap())
    }

    async fn sync_mapping_locked(&self) -> Result<()> {
        let p_max = f32::from_le_bytes(
            self.read_parameter_locked(POSITION_MAPPING_REGISTER)
                .await?,
        );
        let v_max = f32::from_le_bytes(
            self.read_parameter_locked(VELOCITY_MAPPING_REGISTER)
                .await?,
        );
        let t_max = f32::from_le_bytes(self.read_parameter_locked(TORQUE_MAPPING_REGISTER).await?);
        let config = DamiaoConfig {
            p_max,
            v_max,
            t_max,
            ..self.current_config()
        }
        .validate()?;
        *self.config.lock().unwrap() = config;
        let mut shared = self.state.lock().unwrap();
        shared.view.p_max = p_max;
        shared.view.v_max = v_max;
        shared.view.t_max = t_max;
        Ok(())
    }

    /// Query both float position registers without changing motor parameters.
    /// Read-only polling is bounded so a missing encoder reply cannot hold
    /// motion/disable operations behind the gate for a full parameter timeout.
    pub async fn read_positions(&self) -> Result<DamiaoState> {
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        self.read_positions_locked().await
    }

    async fn read_positions_locked(&self) -> Result<DamiaoState> {
        let result = tokio::time::timeout(POSITION_READ_TIMEOUT, async {
            let position = f32::from_le_bytes(
                self.read_parameter_locked(PRECISE_POSITION_REGISTER)
                    .await?,
            );
            let output =
                f32::from_le_bytes(self.read_parameter_locked(OUTPUT_POSITION_REGISTER).await?);
            ensure_finite("DAMIAO p_m position", position)?;
            ensure_finite("DAMIAO xout output-encoder angle", output)?;
            let mut shared = self.state.lock().unwrap();
            shared.view.position_rad = Some(position);
            shared.view.precise_position_rad = Some(position);
            shared.view.output_position_rad = Some(output);
            shared.view.position_read_count = shared.view.position_read_count.saturating_add(1);
            shared.view.position_read_age_ms = Some(0);
            shared.view.position_read_error = None;
            shared.last_position_read = Some(Instant::now());
            Ok::<_, anyhow::Error>(shared.view.clone())
        })
        .await
        .map_err(|_| {
            anyhow!(
                "DAMIAO position-register read timed out after {} ms",
                POSITION_READ_TIMEOUT.as_millis()
            )
        })
        .and_then(|result| result);
        if let Err(error) = &result {
            let mut shared = self.state.lock().unwrap();
            shared.view.position_rad = None;
            shared.view.precise_position_rad = None;
            shared.view.output_position_rad = None;
            shared.view.position_read_error = Some(error.to_string());
        }
        result
    }

    /// Read can_br (RID 0x23); accepting the host transmit alone never counts
    /// as success. The response must come from this motor's feedback CAN ID.
    pub async fn get_bitrate(&self) -> Result<DamiaoBitrateState> {
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        self.read_bitrate_locked().await
    }

    async fn read_bitrate_locked(&self) -> Result<DamiaoBitrateState> {
        let config = self.current_config();
        let command = [
            config.motor_id as u8,
            (config.motor_id >> 8) as u8,
            READ_PARAMETER_COMMAND,
            CAN_BITRATE_REGISTER,
        ];
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        let frame = CanFrame::new_data(STATUS_REQUEST_ID, &command)?;
        self.bus.send(frame).await?;
        let reply = wait_parameter_reply(
            &mut *rx,
            feedback_can_id,
            config.motor_id,
            READ_PARAMETER_COMMAND,
            CAN_BITRATE_REGISTER,
            None,
        )
        .await?;
        if let CanId::Standard(id) = reply.id() {
            self.state.lock().unwrap().view.feedback_can_id = Some(id);
        }
        let code = u32::from_le_bytes(reply.data()[4..8].try_into().unwrap());
        DamiaoBitrateState::confirmed(config.motor_id, code, false)
    }

    /// The drive acknowledges at the old bitrate, then changes timing
    /// immediately. This writes the volatile register only; saving to flash
    /// is an explicit separate operation after reconnecting at the new rate.
    pub async fn set_bitrate(&self, bitrate: u32) -> Result<DamiaoBitrateState> {
        let register_code = DOCUMENTED_BITRATES
            .iter()
            .position(|value| *value == bitrate)
            .ok_or_else(|| anyhow!("unsupported DAMIAO CAN bitrate {bitrate}"))?
            as u32;
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        let previous = self.read_bitrate_locked().await?;
        if previous.register_code == register_code {
            return Ok(previous);
        }

        self.stream_target.lock().unwrap().take();
        self.confirm_disabled_locked().await?;
        let config = self.current_config();
        let payload = parameter_write_payload(config.motor_id, CAN_BITRATE_REGISTER, register_code);
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        let frame = CanFrame::new_data(STATUS_REQUEST_ID, &payload)?;

        // Even a lost acknowledgement can leave the motor at the new rate.
        // Set this before transmitting and retain it on every later error.
        self.communication_requires_reconnect
            .store(true, Ordering::Release);
        let result = async {
            self.bus.send(frame).await?;
            wait_parameter_reply(
                &mut *rx,
                feedback_can_id,
                config.motor_id,
                WRITE_PARAMETER_COMMAND,
                CAN_BITRATE_REGISTER,
                Some(register_code),
            )
            .await?;
            let mut result = DamiaoBitrateState::confirmed(config.motor_id, register_code, true)?;
            // At MST_ID=0x7FF a local write echo is indistinguishable from
            // the acknowledgement. Never call that a confirmed write.
            result.acknowledged = feedback_can_id != Some(STATUS_REQUEST_ID);
            Ok::<_, anyhow::Error>(result)
        }
        .await;
        let mut shared = self.state.lock().unwrap();
        shared.view.enabled = false;
        shared.view.online = false;
        shared.view.last_error = Some(match &result {
            Ok(result) if result.acknowledged => format!(
                "Motor CAN bitrate changed to {bitrate} bit/s; reconfigure the host CAN interface and reconnect"
            ),
            Ok(_) => format!("Bitrate write to {bitrate} bit/s was sent with MST_ID=0x7FF; its acknowledgement cannot be distinguished from a request echo. Reconnect at the requested rate and read back"),
            Err(error) => format!(
                "CAN bitrate write to {bitrate} bit/s was not confirmed; motor bitrate is uncertain. Reconnect at the old or requested rate: {error}"
            ),
        });
        // A write may have reached the motor even if the transport or reply
        // failed. Report uncertainty explicitly so the frontend cannot treat
        // this as a harmless, retryable validation error.
        let mut outcome = DamiaoBitrateState::confirmed(config.motor_id, register_code, true)?;
        outcome.acknowledged = result.as_ref().is_ok_and(|value| value.acknowledged);
        Ok(outcome)
    }

    /// Saves all current drive parameters, including can_br, to flash. The
    /// manual specifies disabled-only storage and a maximum 30 ms write;
    /// the matched storage acknowledgement is required before returning.
    pub async fn save_parameters(&self) -> Result<()> {
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        self.read_bitrate_locked().await?;
        self.stream_target.lock().unwrap().take();
        self.confirm_disabled_locked().await?;
        self.save_parameters_locked().await
    }

    async fn save_parameters_locked(&self) -> Result<()> {
        let config = self.current_config();
        let command = [
            config.motor_id as u8,
            (config.motor_id >> 8) as u8,
            SAVE_PARAMETERS_COMMAND,
            1,
        ];
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        if feedback_can_id == Some(STATUS_REQUEST_ID) {
            bail!("Cannot verify Flash storage with MST_ID=0x7FF: request and acknowledgement have the same ID and payload. Set MST_ID to another value before saving");
        }
        self.bus
            .send(CanFrame::new_data(STATUS_REQUEST_ID, &command)?)
            .await?;
        wait_parameter_reply(
            &mut *rx,
            feedback_can_id,
            config.motor_id,
            SAVE_PARAMETERS_COMMAND,
            1,
            None,
        )
        .await?;
        // Keep refresh/target sends behind the control gate for the manual's
        // maximum flash-write interval even if firmware acknowledges early.
        tokio::time::sleep(PARAMETER_SAVE_SETTLE).await;
        self.state.lock().unwrap().view.last_error = None;
        Ok(())
    }

    /// Change ESC_ID (RID 0x08), leaving the feedback Master ID unchanged.
    /// Read the new address back even when its write acknowledgement is lost;
    /// an unverified write blocks all session traffic instead of retrying it.
    pub async fn set_motor_id(
        &self,
        new_motor_id: u16,
        save: bool,
    ) -> Result<DamiaoIdChangeResult> {
        let _guard = self.control_gate.lock().await;
        self.ensure_communication_ready()?;
        let current = self.current_config();
        let next = DamiaoConfig {
            motor_id: new_motor_id,
            ..current
        }
        .validate()?;
        let actual_id = u32::from_le_bytes(self.read_parameter_locked(MOTOR_ID_REGISTER).await?);
        if actual_id != u32::from(current.motor_id) {
            bail!("DAMIAO ESC_ID readback {actual_id:#X} does not match attached motor {:#X}; ID was not modified", current.motor_id);
        }
        let mut outcome = DamiaoIdChangeResult {
            previous_motor_id: current.motor_id,
            requested_motor_id: new_motor_id,
            verified: true,
            saved: false,
            error: None,
            state: self.snapshot(),
        };
        if new_motor_id == current.motor_id {
            return Ok(outcome);
        }

        self.ensure_motor_id_available_locked(new_motor_id).await?;
        self.stream_target.lock().unwrap().take();
        self.confirm_disabled_locked().await?;
        let payload =
            parameter_write_payload(current.motor_id, MOTOR_ID_REGISTER, u32::from(new_motor_id));
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        let frame = CanFrame::new_data(STATUS_REQUEST_ID, &payload)?;
        self.communication_requires_reconnect
            .store(true, Ordering::Release);
        let verification = async {
            self.bus.send(frame).await?;
            // Firmware may use either the old or already-updated ESC_ID in
            // its write reply. Neither alone proves routing at the new ID.
            let _ = tokio::time::timeout(
                PARAMETER_RESPONSE_TIMEOUT,
                receive_parameter_reply(
                    &mut *rx,
                    feedback_can_id,
                    &[current.motor_id, new_motor_id],
                    WRITE_PARAMETER_COMMAND,
                    MOTOR_ID_REGISTER,
                    Some(u32::from(new_motor_id)),
                ),
            )
            .await;
            let readback = u32::from_le_bytes(
                self.read_parameter_at_locked(new_motor_id, MOTOR_ID_REGISTER)
                    .await?,
            );
            if readback != u32::from(new_motor_id) {
                bail!("ESC_ID readback was {readback:#X}, expected {new_motor_id:#X}");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;

        if let Err(error) = verification {
            let error = format!("Motor ID write {:#X} → {new_motor_id:#X} was not verified; the ID may already have changed. Reconnect and check the old and requested IDs before retrying: {error}", current.motor_id);
            let mut shared = self.state.lock().unwrap();
            shared.last_feedback = None;
            shared.view.enabled = false;
            shared.view.last_error = Some(error.clone());
            drop(shared);
            outcome.verified = false;
            outcome.error = Some(error);
            outcome.state = self.snapshot();
            return Ok(outcome);
        }

        // Commit the new identity before releasing the control gate, even
        // if confirming disabled feedback or saving Flash subsequently fails.
        *self.config.lock().unwrap() = next;
        {
            let mut shared = self.state.lock().unwrap();
            let feedback_can_id = shared.view.feedback_can_id;
            let rx_count = shared.view.rx_count;
            shared.view = DamiaoState::attached(next);
            shared.view.feedback_can_id = feedback_can_id;
            shared.view.rx_count = rx_count;
            shared.last_feedback = None;
            shared.last_position_read = None;
            shared.feedback_rate_window_start = None;
            shared.feedback_rate_intervals = 0;
            shared.feedback_rate_hz = None;
        }
        self.communication_requires_reconnect
            .store(false, Ordering::Release);
        let finish = async {
            self.confirm_disabled_locked().await?;
            if save {
                self.save_parameters_locked().await?;
                outcome.saved = true;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = finish {
            let error = format!(
                "Motor ID {new_motor_id:#X} is verified, but {}: {error}",
                if save {
                    "Flash storage was not confirmed"
                } else {
                    "disabled feedback was not confirmed"
                }
            );
            self.state.lock().unwrap().view.last_error = Some(error.clone());
            outcome.error = Some(error);
        }
        outcome.state = self.snapshot();
        Ok(outcome)
    }

    async fn ensure_motor_id_available_locked(&self, motor_id: u16) -> Result<()> {
        // A motor not yet found by discovery can occupy the requested ID.
        // Query its ESC_ID without disabling or otherwise modifying it.
        let mut rx = self.bus.subscribe(CanFilter::pass_all_standard()).await?;
        self.bus
            .send(CanFrame::new_data(
                STATUS_REQUEST_ID,
                &[
                    motor_id as u8,
                    (motor_id >> 8) as u8,
                    READ_PARAMETER_COMMAND,
                    MOTOR_ID_REGISTER,
                ],
            )?)
            .await?;
        match tokio::time::timeout(
            PARAMETER_RESPONSE_TIMEOUT,
            receive_parameter_reply(
                &mut *rx,
                None,
                &[motor_id],
                READ_PARAMETER_COMMAND,
                MOTOR_ID_REGISTER,
                None,
            ),
        )
        .await
        {
            Ok(Ok(_)) => {
                bail!("DAMIAO motor ID {motor_id:#X} is already in use; ID was not modified")
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Ok(()),
        }
    }

    async fn parameter_subscription(&self) -> Result<(Option<u16>, Box<dyn can_transport::CanRx>)> {
        // The configured Master ID can be stale. Once real telemetry or a
        // parameter reply reveals it, all later replies must match that ID.
        let observed_id = self.state.lock().unwrap().view.feedback_can_id.or_else(|| {
            (self.current_config().master_id == STATUS_REQUEST_ID).then_some(STATUS_REQUEST_ID)
        });
        let filter =
            observed_id.map_or_else(CanFilter::pass_all_standard, CanFilter::exact_standard);
        let rx = self
            .bus
            .subscribe(filter)
            .await
            .map_err(|error| anyhow!("subscribe DAMIAO parameter replies: {error}"))?;
        Ok((observed_id, rx))
    }

    async fn confirm_disabled_locked(&self) -> Result<Feedback> {
        self.confirm_special_locked(DISABLE_COMMAND, 0).await
    }

    async fn confirm_special_locked(&self, command: u8, expected_status: u8) -> Result<Feedback> {
        let config = self.current_config();
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        let operation = if expected_status == 1 {
            "enable"
        } else {
            "disable"
        };
        while rx.try_recv()?.is_some() {}
        self.send_special(command).await?;
        tokio::time::timeout(PARAMETER_RESPONSE_TIMEOUT, async {
            loop {
                let frame = match rx.recv().await {
                    Ok(frame) => frame,
                    Err(CanIoError::Lagged { .. }) => continue,
                    Err(error) => bail!("receive DAMIAO {operation} confirmation: {error}"),
                };
                let Some((can_id, feedback)) = matching_feedback(config, feedback_can_id, &frame)?
                else {
                    continue;
                };
                self.store_confirmed_feedback(can_id, feedback);
                if feedback.status_code == expected_status {
                    return Ok(feedback);
                }
                if feedback.status_code > 1 {
                    bail!(
                        "DAMIAO {operation} failed: {} (0x{:X}); clear the fault before retrying",
                        status_name(feedback.status_code),
                        feedback.status_code
                    );
                }
            }
        })
        .await
        .map_err(|_| {
            anyhow!(
                "DAMIAO {operation} was not confirmed within {} ms",
                PARAMETER_RESPONSE_TIMEOUT.as_millis()
            )
        })?
    }

    fn store_confirmed_feedback(&self, can_id: u16, feedback: Feedback) -> DamiaoState {
        let streaming = self.stream_target.lock().unwrap().is_some();
        let requires_reconnect = self.requires_reconnect();
        let mut shared = self.state.lock().unwrap();
        apply_feedback(&mut shared.view, can_id, feedback);
        // The normal receive loop owns rx_count and rate measurement. This
        // independent transaction subscription still provides measured data
        // immediately, even before that task processes its copy of the frame.
        shared.last_feedback = Some(Instant::now());
        shared.view.feedback_age_ms = Some(0);
        shared.view.online = !requires_reconnect;
        shared.view.streaming = streaming;
        shared.view.requires_reconnect = requires_reconnect;
        shared.view.feedback_rate_hz = shared.feedback_rate_hz;
        // Return this exact measured frame while still holding the state
        // lock; the primary receiver may be processing older queued frames.
        shared.view.clone()
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
        let (feedback_can_id, mut rx) = self.parameter_subscription().await?;
        let frame =
            CanFrame::new_data(STATUS_REQUEST_ID, &payload).map_err(|error| anyhow!(error))?;
        self.bus.send(frame).await?;
        wait_parameter_reply(
            &mut *rx,
            feedback_can_id,
            motor_id,
            WRITE_PARAMETER_COMMAND,
            CONTROL_MODE_REGISTER,
            Some(u32::from_le_bytes(payload[4..8].try_into().unwrap())),
        )
        .await?;
        if feedback_can_id == Some(STATUS_REQUEST_ID) {
            let actual = self.read_parameter_locked(CONTROL_MODE_REGISTER).await?;
            if actual != payload[4..8] {
                bail!("CTRL_MODE readback differs from the requested mode");
            }
        }
        Ok(())
    }

    async fn send_special_to(&self, can_id: u16, command: u8) -> Result<()> {
        let payload = special_payload(command);
        let frame = CanFrame::new_data(can_id & 0xFF, &payload).map_err(|error| anyhow!(error))?;
        self.bus.send(frame).await.map_err(|error| anyhow!(error))
    }
}

async fn position_loop(session: Weak<DamiaoSession>) {
    let mut ticker = tokio::time::interval(POSITION_READ_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let Some(session) = session.upgrade() else {
            break;
        };
        if !session.running.load(Ordering::Acquire) {
            break;
        }
        if !session.requires_reconnect() {
            // A failed read clears the values and records a separate error;
            // ordinary control feedback remains independently usable.
            let _ = session.read_positions().await;
        }
    }
}

async fn receive_loop(
    mut rx: Box<dyn can_transport::CanRx>,
    config: Arc<StdMutex<DamiaoConfig>>,
    state: Arc<StdMutex<SharedState>>,
    running: Arc<AtomicBool>,
    communication_requires_reconnect: Arc<AtomicBool>,
) {
    while running.load(Ordering::Acquire) {
        match rx.recv().await {
            Ok(frame) => {
                if !is_data_frame(&frame)
                    || frame.data().len() != 8
                    || communication_requires_reconnect.load(Ordering::Acquire)
                {
                    continue;
                }
                // Never mistake a locally echoed 0x7FF status request for a
                // disabled feedback packet while listening in auto-detect mode.
                let config = *config.lock().unwrap();
                if frame.id() == CanId::Standard(STATUS_REQUEST_ID)
                    && (config.master_id != STATUS_REQUEST_ID || is_request_echo(frame.data()))
                {
                    continue;
                }
                // Parameter replies share the telemetry CAN ID and motor ID
                // byte, but the following bytes have a different meaning.
                if is_parameter_reply_for(config.motor_id, frame.data()) {
                    continue;
                }
                match decode_feedback(config, frame.data()) {
                    Ok(Some(feedback)) => {
                        let mut shared = state.lock().unwrap();
                        if communication_requires_reconnect.load(Ordering::Acquire)
                            || config.motor_id != shared.view.motor_id
                        {
                            continue;
                        }
                        if let CanId::Standard(id) = frame.id() {
                            apply_feedback(&mut shared.view, id, feedback);
                        }
                        shared.view.rx_count = shared.view.rx_count.saturating_add(1);
                        record_feedback_timing(&mut shared, Instant::now());
                    }
                    Ok(None) => {}
                    Err(error) => set_last_error(&state, error),
                }
            }
            Err(CanIoError::Lagged { dropped }) => {
                set_last_error(
                    &state,
                    anyhow!("DAMIAO feedback dropped {dropped} CAN frames"),
                );
            }
            Err(CanIoError::Disconnected) => {
                set_last_error(&state, anyhow!("DAMIAO feedback receive disconnected"));
                break;
            }
            Err(error) => {
                set_last_error(&state, anyhow!("DAMIAO feedback receive failed: {error}"));
                // Backend errors may be transient (for example, recovery from
                // a controller fault). Avoid killing the session permanently.
                tokio::time::sleep(STATUS_REQUEST_PERIOD).await;
            }
        }
    }
}

async fn refresh_loop(
    bus: Arc<dyn CanBus>,
    config: Arc<StdMutex<DamiaoConfig>>,
    state: Arc<StdMutex<SharedState>>,
    running: Arc<AtomicBool>,
    control_gate: Arc<AsyncMutex<()>>,
    communication_requires_reconnect: Arc<AtomicBool>,
) {
    let mut ticker = tokio::time::interval(STATUS_REQUEST_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while running.load(Ordering::Acquire) {
        ticker.tick().await;
        let _guard = control_gate.lock().await;
        if communication_requires_reconnect.load(Ordering::Acquire) {
            continue;
        }
        let config = *config.lock().unwrap();
        let payload = status_request_payload(config.motor_id);
        let Ok(frame) = CanFrame::new_data(STATUS_REQUEST_ID, &payload) else {
            continue;
        };
        if let Err(error) = bus.send(frame).await {
            set_last_error(&state, anyhow!("request DAMIAO status: {error}"));
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
        let ready = {
            let shared = state.lock().unwrap();
            shared.view.enabled
                && shared.view.status_code == 1
                && shared
                    .last_feedback
                    .is_some_and(|last| last.elapsed() <= FEEDBACK_FRESH_FOR)
        };
        if !ready {
            target.lock().unwrap().take();
            let motor_id = config.lock().unwrap().motor_id;
            if let Ok(frame) =
                CanFrame::new_data(motor_id & 0xFF, &special_payload(DISABLE_COMMAND))
            {
                let _ = bus.send(frame).await;
            }
            set_last_error(&state, anyhow!("DAMIAO periodic target stopped: enabled feedback is stale or the motor is disabled/faulted; enable and send again after checking feedback"));
            continue;
        }
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
        DamiaoMode::Unspecified | DamiaoMode::Mit => 0x000,
        DamiaoMode::PositionVelocity => 0x100,
        DamiaoMode::Velocity => 0x200,
        DamiaoMode::ForcePosition => 0x300,
    };
    (motor_id & 0xFF)
        .checked_add(offset)
        .filter(|value| *value <= 0x7FF)
        .ok_or_else(|| anyhow!("DAMIAO control CAN ID exceeds 0x7FF"))
}

fn target_dlc(mode: DamiaoMode) -> usize {
    match mode {
        DamiaoMode::Unspecified
        | DamiaoMode::Mit
        | DamiaoMode::PositionVelocity
        | DamiaoMode::ForcePosition => 8,
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
        DamiaoMode::Unspecified => 0,
        DamiaoMode::Mit => 1,
        DamiaoMode::PositionVelocity => 2,
        DamiaoMode::Velocity => 3,
        DamiaoMode::ForcePosition => 4,
    };
    parameter_write_payload(motor_id, CONTROL_MODE_REGISTER, mode_value)
}

fn parameter_write_payload(motor_id: u16, register: u8, value: u32) -> [u8; 8] {
    let value = value.to_le_bytes();
    [
        motor_id as u8,
        (motor_id >> 8) as u8,
        WRITE_PARAMETER_COMMAND,
        register,
        value[0],
        value[1],
        value[2],
        value[3],
    ]
}

async fn wait_parameter_reply(
    rx: &mut dyn can_transport::CanRx,
    feedback_can_id: Option<u16>,
    motor_id: u16,
    command: u8,
    register: u8,
    expected_value: Option<u32>,
) -> Result<CanFrame> {
    tokio::time::timeout(PARAMETER_RESPONSE_TIMEOUT, receive_parameter_reply(
        rx, feedback_can_id, &[motor_id], command, register, expected_value,
    ))
    .await
    .map_err(|_| anyhow!(
        "DAMIAO command 0x{command:02X}, register 0x{register:02X} was not acknowledged within {} ms",
        PARAMETER_RESPONSE_TIMEOUT.as_millis()
    ))?
}

async fn receive_parameter_reply(
    rx: &mut dyn can_transport::CanRx,
    feedback_can_id: Option<u16>,
    motor_ids: &[u16],
    command: u8,
    register: u8,
    expected_value: Option<u32>,
) -> Result<CanFrame> {
    loop {
        let frame = match rx.recv().await {
            Ok(frame) => frame,
            Err(CanIoError::Lagged { .. }) => continue,
            Err(error) => bail!("receive DAMIAO parameter reply: {error}"),
        };
        let data = frame.data();
        if !matches!(frame.id(), CanId::Standard(id)
                if (id != STATUS_REQUEST_ID || feedback_can_id == Some(STATUS_REQUEST_ID))
                    && feedback_can_id.is_none_or(|expected| expected == id))
            || !is_data_frame(&frame)
            || !motor_ids.iter().any(|id| is_parameter_reply_for(*id, data))
            || data[2] != command
            || data[3] != register
        {
            continue;
        }
        if command == SAVE_PARAMETERS_COMMAND {
            if data.len() == 4 || (data.len() == 8 && data[4..] == [0, 0, 0, 0]) {
                return Ok(frame);
            }
        } else if data.len() == 8 {
            let value = u32::from_le_bytes(data[4..8].try_into().unwrap());
            if expected_value.is_none_or(|expected| expected == value) {
                return Ok(frame);
            }
        }
    }
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
            if !(-config.p_max..=config.p_max).contains(&position_rad) {
                bail!("position must be in {}..={}", -config.p_max, config.p_max);
            }
            // The scales were read from the drive before attaching. Commands
            // and feedback must use exactly the same mapping in both directions.
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
            if velocity_rad_s < 0.0 {
                bail!("position-velocity maximum speed must be nonnegative");
            }
            let mut payload = [0u8; 8];
            payload[..4].copy_from_slice(&position_rad.to_le_bytes());
            payload[4..].copy_from_slice(&velocity_rad_s.to_le_bytes());
            Ok(payload)
        }
        (DamiaoMode::Velocity, DamiaoTarget::Velocity { velocity_rad_s }) => {
            ensure_finite("velocity", velocity_rad_s)?;
            let mut payload = [0u8; 8];
            payload[..4].copy_from_slice(&velocity_rad_s.to_le_bytes());
            Ok(payload)
        }
        (
            DamiaoMode::ForcePosition,
            DamiaoTarget::ForcePosition {
                position_rad,
                velocity_rad_s,
                current_limit_ratio,
            },
        ) => {
            ensure_finite("position", position_rad)?;
            if !(0.0..=100.0).contains(&velocity_rad_s) {
                bail!("force-position maximum speed must be in 0..=100 rad/s");
            }
            if !(0.0..=1.0).contains(&current_limit_ratio) {
                bail!("force-position current limit must be in 0..=1 per unit");
            }
            let mut payload = [0u8; 8];
            payload[..4].copy_from_slice(&position_rad.to_le_bytes());
            payload[4..6].copy_from_slice(&((velocity_rad_s * 100.0).round() as u16).to_le_bytes());
            payload[6..8]
                .copy_from_slice(&((current_limit_ratio * 10000.0).round() as u16).to_le_bytes());
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
    let scale = ((1u32 << bits) - 1) as f64;
    Ok((((value as f64 - min as f64) * scale) / (max as f64 - min as f64)) as u16)
}

fn uint_to_float(value: u16, min: f32, max: f32, bits: u8) -> f32 {
    let scale = ((1u32 << bits) - 1) as f64;
    (f64::from(value) * (max as f64 - min as f64) / scale + min as f64) as f32
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

fn decode_feedback(config: DamiaoConfig, data: &[u8]) -> Result<Option<Feedback>> {
    if data.len() != 8 {
        bail!("DAMIAO feedback must contain 8 bytes");
    }
    if data[0] & 0x0F != config.motor_id as u8 & 0x0F {
        return Ok(None);
    }
    let status_code = data[0] >> 4;
    if !valid_status_code(status_code) {
        bail!("DAMIAO feedback has an invalid status code");
    }
    let position = u16::from_be_bytes([data[1], data[2]]);
    let velocity = (u16::from(data[3]) << 4) | u16::from(data[4] >> 4);
    let torque = (u16::from(data[4] & 0x0F) << 8) | u16::from(data[5]);
    Ok(Some(Feedback {
        status_code,
        position_rad: uint_to_float(position, -config.p_max, config.p_max, 16),
        velocity_rad_s: uint_to_float(velocity, -config.v_max, config.v_max, 12),
        torque_nm: uint_to_float(torque, -config.t_max, config.t_max, 12),
        mos_temp_c: data[6],
        rotor_temp_c: data[7],
    }))
}

fn matching_feedback(
    config: DamiaoConfig,
    feedback_can_id: Option<u16>,
    frame: &CanFrame,
) -> Result<Option<(u16, Feedback)>> {
    let CanId::Standard(can_id) = frame.id() else {
        return Ok(None);
    };
    if (can_id == STATUS_REQUEST_ID && (config.master_id != STATUS_REQUEST_ID || is_request_echo(frame.data())))
        || feedback_can_id.is_some_and(|expected| expected != can_id)
        || !is_data_frame(frame)
        || frame.data().len() != 8
        || is_parameter_reply_for(config.motor_id, frame.data())
        // Ignore special-command echoes and unrelated standard CAN payloads.
        || !valid_status_code(frame.data()[0] >> 4)
    {
        return Ok(None);
    }
    Ok(decode_feedback(config, frame.data())?.map(|feedback| (can_id, feedback)))
}

fn apply_feedback(view: &mut DamiaoState, can_id: u16, feedback: Feedback) {
    view.feedback_can_id = Some(can_id);
    view.enabled = feedback.status_code == 1;
    view.status_code = feedback.status_code;
    view.status = status_name(feedback.status_code).into();
    view.feedback_position_rad = Some(feedback.position_rad);
    view.velocity_rad_s = Some(feedback.velocity_rad_s);
    view.torque_nm = Some(feedback.torque_nm);
    view.mos_temp_c = Some(feedback.mos_temp_c);
    view.rotor_temp_c = Some(feedback.rotor_temp_c);
    view.last_error = None;
}

fn status_name(code: u8) -> &'static str {
    match code {
        0x0 => "Disabled",
        0x1 => "Enabled",
        0x3 => "Output-shaft calibration fault",
        0x4 => "Sensor output fault",
        0x5 => "Motor encoder calibration fault",
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
    use std::collections::VecDeque;

    use can_transport::CanCapabilities;
    use tokio::sync::mpsc;

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

    struct TestRx(mpsc::UnboundedReceiver<CanFrame>);

    #[async_trait::async_trait]
    impl can_transport::CanRx for TestRx {
        async fn recv(&mut self) -> std::result::Result<CanFrame, CanIoError> {
            self.0.recv().await.ok_or(CanIoError::Disconnected)
        }

        fn try_recv(&mut self) -> std::result::Result<Option<CanFrame>, CanIoError> {
            match self.0.try_recv() {
                Ok(frame) => Ok(Some(frame)),
                Err(mpsc::error::TryRecvError::Empty) => Ok(None),
                Err(mpsc::error::TryRecvError::Disconnected) => Err(CanIoError::Disconnected),
            }
        }
    }

    /// Each send releases the next batch of simulated motor replies. A new
    /// subscription only sees future frames, just as on a real CAN bus.
    #[derive(Default)]
    struct ScriptedBus {
        frames: StdMutex<Vec<CanFrame>>,
        replies: StdMutex<VecDeque<Vec<CanFrame>>>,
        subscribers: StdMutex<Vec<(CanFilter, mpsc::UnboundedSender<CanFrame>)>>,
    }

    impl ScriptedBus {
        fn new(replies: Vec<Vec<CanFrame>>) -> Arc<Self> {
            Arc::new(Self {
                replies: StdMutex::new(replies.into()),
                ..Self::default()
            })
        }
    }

    #[async_trait::async_trait]
    impl CanBus for ScriptedBus {
        async fn send(&self, frame: CanFrame) -> std::result::Result<(), CanIoError> {
            self.frames.lock().unwrap().push(frame);
            let replies = self.replies.lock().unwrap().pop_front().unwrap_or_default();
            let subscribers = self.subscribers.lock().unwrap();
            for reply in replies {
                for (filter, sender) in subscribers.iter() {
                    if filter.matches(&reply) {
                        let _ = sender.send(reply);
                    }
                }
            }
            Ok(())
        }

        async fn subscribe(
            &self,
            filter: CanFilter,
        ) -> std::result::Result<Box<dyn can_transport::CanRx>, CanIoError> {
            let (sender, receiver) = mpsc::unbounded_channel();
            self.subscribers.lock().unwrap().push((filter, sender));
            Ok(Box::new(TestRx(receiver)))
        }

        fn capabilities(&self) -> CanCapabilities {
            CanCapabilities {
                fd: true,
                max_dlen: 64,
            }
        }
    }

    fn bitrate_reply(command: u8, code: u32) -> CanFrame {
        let mut data = parameter_write_payload(1, CAN_BITRATE_REGISTER, code);
        data[2] = command;
        CanFrame::new_data(0x23u16, &data).unwrap()
    }

    fn float_parameter_reply(register: u8, value: f32) -> CanFrame {
        let mut data = parameter_write_payload(1, register, value.to_bits());
        data[2] = READ_PARAMETER_COMMAND;
        CanFrame::new_data(0x23u16, &data).unwrap()
    }

    fn mode_reply(mode: DamiaoMode) -> CanFrame {
        CanFrame::new_data(0x23u16, &control_mode_payload(1, mode)).unwrap()
    }

    fn disabled_reply() -> CanFrame {
        CanFrame::new_data(0x23u16, &[1, 0x7F, 0xFF, 0x7F, 0xF7, 0xFF, 25, 26]).unwrap()
    }

    fn id_reply(motor_id: u16, command: u8, value: u32) -> CanFrame {
        let mut data = parameter_write_payload(motor_id, MOTOR_ID_REGISTER, value);
        data[2] = command;
        CanFrame::new_data(0x23u16, &data).unwrap()
    }

    fn disabled_reply_for(motor_id: u16) -> CanFrame {
        let mut data = disabled_reply().data().to_vec();
        data[0] = motor_id as u8 & 0x0F;
        CanFrame::new_data(0x23u16, &data).unwrap()
    }

    fn save_reply_for(motor_id: u16) -> CanFrame {
        CanFrame::new_data(0x23u16, &[motor_id as u8, (motor_id >> 8) as u8, 0xAA, 1]).unwrap()
    }

    fn telemetry_reply(status: u8, position_rad: f32, velocity_rad_s: f32) -> CanFrame {
        let position = float_to_uint(position_rad, -12.5, 12.5, 16, "position").unwrap();
        let velocity = float_to_uint(velocity_rad_s, -30.0, 30.0, 12, "velocity").unwrap();
        let torque = float_to_uint(0.0, -10.0, 10.0, 12, "torque").unwrap();
        let position = position.to_be_bytes();
        CanFrame::new_data(
            0x23u16,
            &[
                (status << 4) | 1,
                position[0],
                position[1],
                (velocity >> 4) as u8,
                ((velocity & 0x0F) << 4) as u8 | (torque >> 8) as u8,
                torque as u8,
                25,
                26,
            ],
        )
        .unwrap()
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
        view.status_code = if enabled { 1 } else { 0 };
        DamiaoSession {
            bus,
            config: Arc::new(StdMutex::new(config)),
            state: Arc::new(StdMutex::new(SharedState {
                view,
                last_feedback: enabled.then(Instant::now),
                last_position_read: None,
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
            communication_requires_reconnect: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            tasks: StdMutex::new(Vec::new()),
        }
    }

    fn register_reply(can_id: u16, command: u8, address: u8, raw: u32) -> CanFrame {
        let mut data = parameter_write_payload(1, address, raw);
        data[2] = command;
        CanFrame::new_data(can_id, &data).unwrap()
    }

    #[test]
    fn force_position_encodes_float_angle_and_scaled_little_endian_limits() {
        let frame = build_target_frame(
            config(DamiaoMode::ForcePosition),
            DamiaoTarget::ForcePosition {
                position_rad: 1.25,
                velocity_rad_s: 2.5,
                current_limit_ratio: 0.5,
            },
        )
        .unwrap();
        assert_eq!(frame.id(), CanId::Standard(0x301));
        assert_eq!(
            frame.data(),
            &[0x00, 0x00, 0xA0, 0x3F, 0xFA, 0x00, 0x88, 0x13]
        );
        let limits = encode_target(
            config(DamiaoMode::ForcePosition),
            DamiaoTarget::ForcePosition {
                position_rad: -31.25,
                velocity_rad_s: 100.0,
                current_limit_ratio: 1.0,
            },
        )
        .unwrap();
        assert_eq!(&limits[..4], &(-31.25f32).to_le_bytes());
        assert_eq!(&limits[4..], &[0x10, 0x27, 0x10, 0x27]);
        assert_eq!(
            control_mode_payload(1, DamiaoMode::ForcePosition),
            [1, 0, 0x55, 0x0A, 4, 0, 0, 0]
        );
    }

    #[tokio::test]
    async fn force_position_rejects_invalid_values_without_sending_and_never_repeats() {
        let bus = Arc::new(RecordingBus::default());
        let session = test_session(bus.clone(), DamiaoMode::ForcePosition, true);
        for (position, speed, limit) in [
            (f32::NAN, 1.0, 0.5),
            (f32::INFINITY, 1.0, 0.5),
            (0.0, -0.01, 0.5),
            (0.0, 100.01, 0.5),
            (0.0, f32::NAN, 0.5),
            (0.0, 1.0, -0.01),
            (0.0, 1.0, 1.01),
            (0.0, 1.0, f32::INFINITY),
        ] {
            assert!(session
                .send_target(
                    DamiaoTarget::ForcePosition {
                        position_rad: position,
                        velocity_rad_s: speed,
                        current_limit_ratio: limit,
                    },
                    true
                )
                .await
                .is_err());
        }
        assert!(bus.frames.lock().unwrap().is_empty());
        session
            .send_target(
                DamiaoTarget::ForcePosition {
                    position_rad: 31.25,
                    velocity_rad_s: 2.5,
                    current_limit_ratio: 0.5,
                },
                true,
            )
            .await
            .unwrap();
        assert_eq!(bus.frames.lock().unwrap().len(), 1);
        assert!(!session.snapshot().streaming);
    }

    #[tokio::test]
    async fn invalid_register_writes_do_not_stop_or_transmit_to_the_motor() {
        let bus = Arc::new(RecordingBus::default());
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        for (address, value) in [
            (0x50, 1.0),
            (0x03, 1.0),
            (0x08, 1.5),
            (0x26, 0.0),
            (0x08, 2.0),
            (0x23, 4.0),
        ] {
            assert!(session.write_register(address, value).await.is_err());
        }
        assert!(bus.frames.lock().unwrap().is_empty());
        assert!(session.snapshot().streaming);
        assert!(session.snapshot().enabled);
    }

    #[tokio::test]
    async fn register_write_recovers_lost_ack_by_readback_without_rewriting_or_saving() {
        let bus = ScriptedBus::new(vec![
            vec![disabled_reply()],
            vec![],
            vec![register_reply(
                0x23,
                READ_PARAMETER_COMMAND,
                0x19,
                2.5f32.to_bits(),
            )],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let reading = session.write_register(0x19, 2.5).await.unwrap();
        assert_eq!(reading.value, Some(2.5));
        assert!(!session.snapshot().enabled);
        assert!(!session.snapshot().streaming);
        assert!(!session.requires_reconnect());
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].data(), special_payload(DISABLE_COMMAND));
        assert_eq!(frames[1].data(), &[1, 0, 0x55, 0x19, 0, 0, 0x20, 0x40]);
        assert_eq!(frames[2].data(), &[1, 0, 0x33, 0x19]);
    }

    #[tokio::test]
    async fn mapping_write_resynchronizes_all_scales_and_discards_old_telemetry() {
        let bus = ScriptedBus::new(vec![
            vec![disabled_reply()],
            vec![register_reply(
                0x23,
                WRITE_PARAMETER_COMMAND,
                0x15,
                20.0f32.to_bits(),
            )],
            vec![float_parameter_reply(0x15, 20.0)],
            vec![float_parameter_reply(0x15, 20.0)],
            vec![float_parameter_reply(0x16, 10.0)],
            vec![float_parameter_reply(0x17, 5.0)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let reading = session.write_register(0x15, 20.0).await.unwrap();
        assert_eq!(reading.value, Some(20.0));
        let snapshot = session.snapshot();
        assert_eq!(
            (snapshot.p_max, snapshot.v_max, snapshot.t_max),
            (20.0, 10.0, 5.0)
        );
        assert_eq!(snapshot.feedback_position_rad, None);
        assert_eq!(snapshot.velocity_rad_s, None);
        assert_eq!(snapshot.torque_nm, None);
        assert!(!snapshot.online && !snapshot.enabled && !snapshot.streaming);
        assert!(!snapshot.requires_reconnect);
        assert_eq!(bus.frames.lock().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn mismatched_mapping_readback_blocks_commands_and_preserves_previous_scales() {
        let bus = ScriptedBus::new(vec![
            vec![disabled_reply()],
            vec![register_reply(
                0x23,
                WRITE_PARAMETER_COMMAND,
                0x15,
                20.0f32.to_bits(),
            )],
            vec![float_parameter_reply(0x15, 19.0)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let error = session.write_register(0x15, 20.0).await.unwrap_err();
        assert!(error.to_string().contains("readback differs"));
        let snapshot = session.snapshot();
        assert_eq!(snapshot.p_max, 12.5);
        assert!(snapshot.requires_reconnect);
        assert!(!snapshot.streaming && !snapshot.enabled);
        assert!(session.enable().await.is_err());
        session.shutdown().await.unwrap();
        assert_eq!(bus.frames.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn feedback_id_write_verifies_new_route_and_updates_the_session() {
        let bus = ScriptedBus::new(vec![
            vec![disabled_reply()],
            vec![],
            vec![register_reply(0x24, READ_PARAMETER_COMMAND, 0x07, 0x24)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let reading = session.write_register(0x07, 0x24 as f64).await.unwrap();
        assert_eq!(reading.value, Some(0x24 as f64));
        let snapshot = session.snapshot();
        assert_eq!(snapshot.master_id, 0x24);
        assert_eq!(snapshot.feedback_can_id, Some(0x24));
        assert!(!snapshot.requires_reconnect);
        assert!(!snapshot.enabled && !snapshot.online);
        assert_eq!(bus.frames.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn id_changes_verify_new_routing_keep_master_id_and_optionally_save_in_every_mode() {
        for (mode, new_id, ack_id, save) in [
            (DamiaoMode::Mit, 2, 1, true),
            (DamiaoMode::PositionVelocity, 0, 0, false),
            (DamiaoMode::Velocity, 0x11, 0x11, true),
        ] {
            let mut replies = vec![
                vec![id_reply(1, READ_PARAMETER_COMMAND, 1)],
                vec![], // The requested ID is unoccupied.
                vec![disabled_reply()],
                vec![id_reply(ack_id, WRITE_PARAMETER_COMMAND, u32::from(new_id))],
                vec![id_reply(new_id, READ_PARAMETER_COMMAND, u32::from(new_id))],
                vec![disabled_reply_for(new_id)],
            ];
            if save {
                replies.push(vec![save_reply_for(new_id)]);
            }
            let bus = ScriptedBus::new(replies);
            let session = test_session(bus.clone(), mode, true);
            let result = session.set_motor_id(new_id, save).await.unwrap();
            assert!(result.verified);
            assert_eq!(result.saved, save);
            assert!(result.error.is_none());
            assert_eq!(result.state.motor_id, new_id);
            assert_eq!(result.state.master_id, 0);
            assert_eq!(result.state.feedback_can_id, Some(0x23));
            assert!(result.state.online);
            assert!(
                !result.state.enabled
                    && !result.state.streaming
                    && !result.state.requires_reconnect
            );
            let frames = bus.frames.lock().unwrap().clone();
            assert_eq!(frames[0].data(), [1, 0, 0x33, 8]);
            assert_eq!(frames[1].data(), [new_id as u8, 0, 0x33, 8]);
            assert_eq!(frames[2].id(), CanId::Standard(1));
            assert_eq!(frames[3].id(), CanId::Standard(0x7FF));
            assert_eq!(frames[3].data(), [1, 0, 0x55, 8, new_id as u8, 0, 0, 0]);
            assert_eq!(frames[4].data(), [new_id as u8, 0, 0x33, 8]);
            assert_eq!(frames[5].id(), CanId::Standard(new_id));
            assert_eq!(frames.len(), if save { 7 } else { 6 });
            if save {
                assert_eq!(frames[6].data(), [new_id as u8, 0, 0xAA, 1]);
            }

            let target = match mode {
                DamiaoMode::Unspecified | DamiaoMode::ForcePosition => unreachable!(),
                DamiaoMode::Mit => DamiaoTarget::Mit {
                    position_rad: 0.0,
                    velocity_rad_s: 0.0,
                    torque_nm: 0.0,
                    kp: 0.0,
                    kd: 0.0,
                },
                DamiaoMode::PositionVelocity => DamiaoTarget::PositionVelocity {
                    position_rad: 0.0,
                    velocity_rad_s: 0.0,
                },
                DamiaoMode::Velocity => DamiaoTarget::Velocity {
                    velocity_rad_s: 0.0,
                },
            };
            assert!(
                session.send_target(target, false).await.is_err(),
                "an ID change leaves the drive disabled"
            );
            {
                let mut shared = session.state.lock().unwrap();
                shared.view.enabled = true;
                shared.view.status_code = 1;
                shared.last_feedback = Some(Instant::now());
            }
            session.send_target(target, false).await.unwrap();
            session.shutdown().await.unwrap();
            let frames = bus.frames.lock().unwrap();
            assert_eq!(
                frames[frames.len() - 2].id(),
                CanId::Standard(control_id(new_id, mode).unwrap())
            );
            assert_eq!(frames.last().unwrap().id(), CanId::Standard(new_id));
        }
    }

    #[tokio::test]
    async fn id_change_rejects_invalid_same_occupied_and_mismatched_addresses_before_writing() {
        let bus = ScriptedBus::new(vec![]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        assert!(session.set_motor_id(0x800, true).await.is_err());
        assert!(bus.frames.lock().unwrap().is_empty());

        let bus = ScriptedBus::new(vec![vec![id_reply(1, READ_PARAMETER_COMMAND, 1)]]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let result = session.set_motor_id(1, true).await.unwrap();
        assert!(result.verified && !result.saved);
        assert!(result.state.enabled && result.state.streaming);
        assert_eq!(bus.frames.lock().unwrap().len(), 1);

        let bus = ScriptedBus::new(vec![
            vec![id_reply(1, READ_PARAMETER_COMMAND, 1)],
            vec![
                CanFrame::new_data(0x24u16, id_reply(2, READ_PARAMETER_COMMAND, 2).data()).unwrap(),
            ],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        assert!(session
            .set_motor_id(2, true)
            .await
            .unwrap_err()
            .to_string()
            .contains("already in use"));
        assert_eq!(bus.frames.lock().unwrap().len(), 2);
        assert_eq!(session.snapshot().motor_id, 1);
        assert!(!session.requires_reconnect());

        let bus = ScriptedBus::new(vec![vec![id_reply(1, READ_PARAMETER_COMMAND, 3)]]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        assert!(session
            .set_motor_id(2, true)
            .await
            .unwrap_err()
            .to_string()
            .contains("does not match"));
        assert_eq!(bus.frames.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn id_change_requires_fresh_disabled_feedback_before_writing() {
        let bus = ScriptedBus::new(vec![
            vec![id_reply(1, READ_PARAMETER_COMMAND, 1)],
            vec![],
            vec![telemetry_reply(1, 0.0, 0.0), disabled_reply_for(2)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        assert!(session
            .set_motor_id(2, true)
            .await
            .unwrap_err()
            .to_string()
            .contains("disable was not confirmed"));
        assert!(!session.snapshot().streaming);
        assert!(!session.requires_reconnect());
        assert_eq!(bus.frames.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn id_readback_recovers_a_lost_write_reply_without_repeating_the_write() {
        let bus = ScriptedBus::new(vec![
            vec![id_reply(1, READ_PARAMETER_COMMAND, 1)],
            vec![],
            vec![disabled_reply()],
            vec![],
            vec![id_reply(2, READ_PARAMETER_COMMAND, 2)],
            vec![disabled_reply_for(2)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let result = session.set_motor_id(2, false).await.unwrap();
        assert!(result.verified && result.error.is_none());
        assert_eq!(result.state.motor_id, 2);
        assert!(!session.requires_reconnect());
        assert_eq!(
            bus.frames
                .lock()
                .unwrap()
                .iter()
                .filter(|frame| frame.data().get(2) == Some(&WRITE_PARAMETER_COMMAND))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn id_write_acknowledgement_without_valid_readback_blocks_commands_and_stale_teardown() {
        for readback in [vec![], vec![id_reply(2, READ_PARAMETER_COMMAND, 3)]] {
            let bus = ScriptedBus::new(vec![
                vec![id_reply(1, READ_PARAMETER_COMMAND, 1)],
                vec![],
                vec![disabled_reply()],
                vec![id_reply(1, WRITE_PARAMETER_COMMAND, 2)],
                readback,
            ]);
            let session = test_session(bus.clone(), DamiaoMode::Mit, true);
            let result = session.set_motor_id(2, true).await.unwrap();
            assert!(!result.verified && !result.saved);
            assert!(result.state.requires_reconnect);
            assert!(!result.state.online && !result.state.streaming);
            assert_eq!(result.state.motor_id, 1);
            assert!(result.error.unwrap().contains("may already have changed"));
            assert!(session.enable().await.is_err());
            assert!(session.disable().await.is_err());
            assert!(session.save_parameters().await.is_err());
            assert!(session.set_motor_id(2, true).await.is_err());
            assert!(session
                .send_target(
                    DamiaoTarget::Mit {
                        position_rad: 0.0,
                        velocity_rad_s: 0.0,
                        torque_nm: 0.0,
                        kp: 0.0,
                        kd: 0.0,
                    },
                    true
                )
                .await
                .is_err());
            session.shutdown().await.unwrap();
            assert_eq!(bus.frames.lock().unwrap().len(), 5);
        }
    }

    #[tokio::test]
    async fn id_save_failure_retains_verified_new_address_and_allows_saving_without_rewriting_id() {
        let mut bitrate = parameter_write_payload(2, CAN_BITRATE_REGISTER, 4);
        bitrate[2] = READ_PARAMETER_COMMAND;
        let bus = ScriptedBus::new(vec![
            vec![id_reply(1, READ_PARAMETER_COMMAND, 1)],
            vec![],
            vec![disabled_reply()],
            vec![id_reply(1, WRITE_PARAMETER_COMMAND, 2)],
            vec![id_reply(2, READ_PARAMETER_COMMAND, 2)],
            vec![disabled_reply_for(2)],
            vec![],
            vec![CanFrame::new_data(0x23u16, &bitrate).unwrap()],
            vec![disabled_reply_for(2)],
            vec![save_reply_for(2)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let result = session.set_motor_id(2, true).await.unwrap();
        assert!(result.verified && !result.saved);
        assert_eq!(result.state.motor_id, 2);
        assert!(!result.state.requires_reconnect);
        assert!(result
            .error
            .unwrap()
            .contains("Flash storage was not confirmed"));
        session.save_parameters().await.unwrap();
        assert!(session.snapshot().last_error.is_none());
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames.len(), 10);
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.data().get(2) == Some(&WRITE_PARAMETER_COMMAND))
                .count(),
            1
        );
        assert_eq!(frames[9].data(), [2, 0, 0xAA, 1]);
    }

    #[tokio::test]
    async fn discovery_migrates_an_attached_id_without_old_entries_or_probe_interference() {
        let shared = Arc::new(StdMutex::new(DiscoveryState::default()));
        shared.lock().unwrap().attached.insert(1);
        record_discovery_feedback(&mut shared.lock().unwrap(), 1, 0x23, 0, Instant::now());
        let discovery = DamiaoDiscovery {
            state: shared.clone(),
            scan_gate: Arc::new(AsyncMutex::new(())),
            running: Arc::new(AtomicBool::new(false)),
            request_safe_sweep: Arc::new(AtomicBool::new(false)),
            tasks: StdMutex::new(Vec::new()),
        };
        let session = test_session(ScriptedBus::new(vec![]), DamiaoMode::Mit, false);
        let mut snapshot = session.snapshot();
        snapshot.motor_id = 0x12;
        snapshot.online = true;
        snapshot.feedback_can_id = Some(0x23);
        discovery.rename_attached(1, &snapshot);
        assert!(!record_discovery_feedback(
            &mut shared.lock().unwrap(),
            1,
            0x23,
            0,
            Instant::now()
        ));
        assert!(record_discovery_feedback(
            &mut shared.lock().unwrap(),
            2,
            0x23,
            0,
            Instant::now()
        ));
        let devices = discovery.snapshot();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].motor_id, 0x12);
        assert!(devices[0].attached && devices[0].online);
        let guard = discovery.suspend_scanning().await;
        assert!(discovery.scan_gate.try_lock().is_err());
        drop(guard);
        assert!(discovery.scan_gate.try_lock().is_ok());
    }

    #[tokio::test]
    async fn attach_mapping_uses_read_only_drive_scales_instead_of_ui_defaults() {
        let bus = ScriptedBus::new(vec![
            vec![float_parameter_reply(POSITION_MAPPING_REGISTER, 25.0)],
            vec![float_parameter_reply(VELOCITY_MAPPING_REGISTER, 50.0)],
            vec![float_parameter_reply(TORQUE_MAPPING_REGISTER, 20.0)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, false);
        session.sync_mapping_locked().await.unwrap();
        let state = session.snapshot();
        assert_eq!((state.p_max, state.v_max, state.t_max), (25.0, 50.0, 20.0));
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames.len(), 3);
        for (frame, register) in frames.iter().zip([0x15, 0x16, 0x17]) {
            assert_eq!(frame.id(), CanId::Standard(STATUS_REQUEST_ID));
            assert_eq!(frame.data(), [1, 0, READ_PARAMETER_COMMAND, register]);
        }
    }

    #[tokio::test]
    async fn invalid_drive_mapping_cannot_commit_partial_scaling() {
        let bus = ScriptedBus::new(vec![
            vec![float_parameter_reply(POSITION_MAPPING_REGISTER, 25.0)],
            vec![float_parameter_reply(VELOCITY_MAPPING_REGISTER, f32::NAN)],
            vec![float_parameter_reply(TORQUE_MAPPING_REGISTER, 20.0)],
        ]);
        let session = test_session(bus, DamiaoMode::Mit, false);
        assert!(session.sync_mapping_locked().await.is_err());
        assert_eq!(session.snapshot().p_max, 12.5);
        assert_eq!(session.current_config().v_max, 30.0);
    }

    #[tokio::test]
    async fn positions_keep_float_control_and_output_encoder_coordinates_distinct() {
        let bus = ScriptedBus::new(vec![
            vec![float_parameter_reply(PRECISE_POSITION_REGISTER, 7.344)],
            vec![
                float_parameter_reply(POSITION_MAPPING_REGISTER, 12.5),
                float_parameter_reply(OUTPUT_POSITION_REGISTER, 1.1765633),
            ],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, false);
        let state = session.read_positions().await.unwrap();
        assert_eq!(state.precise_position_rad, Some(7.344));
        assert_eq!(state.position_rad, Some(7.344));
        assert_eq!(state.output_position_rad, Some(1.1765633));
        assert_eq!(state.position_read_count, 1);
        assert!(state.position_read_error.is_none());
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames[0].data(), [1, 0, READ_PARAMETER_COMMAND, 0x50]);
        assert_eq!(frames[1].data(), [1, 0, READ_PARAMETER_COMMAND, 0x51]);
    }

    #[tokio::test]
    async fn cyclic_feedback_cannot_overwrite_a_multiturn_float_position() {
        let bus = ScriptedBus::new(vec![
            vec![float_parameter_reply(
                PRECISE_POSITION_REGISTER,
                -17.6222305,
            )],
            vec![float_parameter_reply(OUTPUT_POSITION_REGISTER, -17.6239052)],
        ]);
        let session = test_session(bus, DamiaoMode::Mit, false);
        session.read_positions().await.unwrap();
        // Actual can0 feedback: 03 CB 8D 7F F7 FE 1E 1C. Adapt only
        // the motor nibble to this fixture's ID; the position is unchanged.
        let feedback = decode_feedback(
            config(DamiaoMode::Mit),
            &[1, 0xCB, 0x8D, 0x7F, 0xF7, 0xFE, 30, 28],
        )
        .unwrap()
        .unwrap();
        apply_feedback(&mut session.state.lock().unwrap().view, 0x23, feedback);
        let state = session.snapshot();
        assert_eq!(state.position_rad, Some(-17.6222305));
        assert_eq!(state.output_position_rad, Some(-17.6239052));
        assert!((state.feedback_position_rad.unwrap() - 7.3783093).abs() < 0.001);
        assert!(
            (state.feedback_position_rad.unwrap() - state.position_rad.unwrap() - 25.0).abs()
                < 0.001
        );
    }

    #[tokio::test]
    async fn missing_output_encoder_reply_clears_old_angles_with_bounded_read_time() {
        let bus = ScriptedBus::new(vec![
            vec![float_parameter_reply(PRECISE_POSITION_REGISTER, 1.1907268)],
            vec![],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, false);
        session.state.lock().unwrap().view.output_position_rad = Some(0.5);
        let started = Instant::now();
        assert!(session.read_positions().await.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        let state = session.snapshot();
        assert!(state.output_position_rad.is_none());
        assert!(state.precise_position_rad.is_none());
        assert!(state.position_read_error.unwrap().contains("timed out"));
        assert_eq!(bus.frames.lock().unwrap().len(), 2);
    }

    #[test]
    fn old_position_register_values_expire_independently_of_control_feedback() {
        let session = test_session(ScriptedBus::new(vec![]), DamiaoMode::Mit, false);
        {
            let mut shared = session.state.lock().unwrap();
            shared.last_feedback = Some(Instant::now());
            shared.last_position_read = Some(Instant::now() - Duration::from_secs(1));
            shared.view.precise_position_rad = Some(1.2);
            shared.view.output_position_rad = Some(1.1);
        }
        let state = session.snapshot();
        assert!(state.online);
        assert!(state.precise_position_rad.is_none());
        assert!(state.output_position_rad.is_none());
        assert!(state.position_read_age_ms.unwrap() >= 1000);
    }

    #[tokio::test]
    async fn saving_zero_uses_base_motor_id_in_every_mode_and_returns_measured_disabled_state() {
        for mode in [
            DamiaoMode::Mit,
            DamiaoMode::PositionVelocity,
            DamiaoMode::Velocity,
        ] {
            let zero = telemetry_reply(0, 0.0, 0.0);
            let measured = decode_feedback(config(mode), zero.data()).unwrap().unwrap();
            let bus = ScriptedBus::new(vec![
                vec![telemetry_reply(0, 1.2, 0.0)],
                vec![zero],
                vec![float_parameter_reply(PRECISE_POSITION_REGISTER, 0.000321)],
                vec![float_parameter_reply(OUTPUT_POSITION_REGISTER, 0.0001)],
            ]);
            let session = test_session(bus.clone(), mode, true);
            let state = session.set_zero().await.unwrap();
            assert!(state.online);
            assert!(!state.enabled && !state.streaming);
            assert_eq!(state.status, "Disabled");
            assert_eq!(state.feedback_can_id, Some(0x23));
            assert_eq!(state.position_rad, Some(0.000321));
            assert_eq!(state.output_position_rad, Some(0.0001));
            assert_eq!(state.feedback_position_rad, Some(measured.position_rad));
            assert_eq!(state.velocity_rad_s, Some(measured.velocity_rad_s));
            assert_ne!(
                state.position_rad,
                Some(0.0),
                "retain the measured float position, not a synthetic local zero"
            );
            let frames = bus.frames.lock().unwrap();
            assert_eq!(frames.len(), 4);
            for frame in frames[..2].iter() {
                assert_eq!(frame.id(), CanId::Standard(1));
            }
            assert_eq!(frames[0].data(), special_payload(DISABLE_COMMAND));
            assert_eq!(frames[1].data(), special_payload(SET_ZERO_COMMAND));
            assert_eq!(frames[2].data(), [1, 0, READ_PARAMETER_COMMAND, 0x50]);
            assert_eq!(frames[3].data(), [1, 0, READ_PARAMETER_COMMAND, 0x51]);
        }
    }

    #[tokio::test]
    async fn saving_zero_refuses_a_shaft_still_moving_after_disable() {
        let bus = ScriptedBus::new(vec![vec![telemetry_reply(0, 1.2, 0.3)]]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let error = session.set_zero().await.unwrap_err();
        assert!(error.to_string().contains("still moving"));
        assert!(error.to_string().contains("zero-save command was not sent"));
        assert!(!session.snapshot().streaming);
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data(), special_payload(DISABLE_COMMAND));
    }

    #[tokio::test]
    async fn saving_zero_requires_new_disabled_feedback_even_with_a_cached_disabled_zero() {
        let bus = ScriptedBus::new(vec![vec![]]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, false);
        {
            let mut shared = session.state.lock().unwrap();
            apply_feedback(
                &mut shared.view,
                0x23,
                decode_feedback(config(DamiaoMode::Mit), disabled_reply().data())
                    .unwrap()
                    .unwrap(),
            );
            shared.last_feedback = Some(Instant::now());
        }
        let error = session.set_zero().await.unwrap_err();
        assert!(error.to_string().contains("disable was not confirmed"));
        assert!(!session.snapshot().streaming);
        assert_eq!(bus.frames.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn saving_zero_polls_status_when_the_command_does_not_emit_feedback() {
        let bus = ScriptedBus::new(vec![
            vec![telemetry_reply(0, 1.2, 0.0)],
            vec![],
            vec![disabled_reply()],
            vec![float_parameter_reply(PRECISE_POSITION_REGISTER, 0.000321)],
            vec![float_parameter_reply(OUTPUT_POSITION_REGISTER, 0.0001)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::PositionVelocity, true);
        let state = session.set_zero().await.unwrap();
        assert!(state.position_rad.unwrap().abs() <= ZERO_POSITION_TOLERANCE_RAD);
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[1].data(), special_payload(SET_ZERO_COMMAND));
        assert_eq!(frames[2].id(), CanId::Standard(STATUS_REQUEST_ID));
        assert_eq!(frames[2].data(), status_request_payload(1));
    }

    #[tokio::test]
    async fn wrapped_zero_feedback_is_not_proof_of_a_saved_zero() {
        let bus = ScriptedBus::new(vec![
            vec![telemetry_reply(0, 1.2, 0.0)],
            vec![disabled_reply()],
            vec![float_parameter_reply(PRECISE_POSITION_REGISTER, -25.0)],
            vec![float_parameter_reply(OUTPUT_POSITION_REGISTER, -25.0)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, false);
        assert!(session
            .set_zero()
            .await
            .unwrap_err()
            .to_string()
            .contains("saved zero is uncertain"));
        let frames = bus.frames.lock().unwrap();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.data() == special_payload(SET_ZERO_COMMAND))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn unverified_zero_ignores_other_motors_echoes_parameters_motion_and_old_coordinates_without_retrying(
    ) {
        let mut other_motor = disabled_reply().data().to_vec();
        other_motor[0] = 2;
        let bus = ScriptedBus::new(vec![
            vec![telemetry_reply(0, 1.2, 0.0)],
            vec![
                CanFrame::new_data(0x23u16, &other_motor).unwrap(),
                CanFrame::new_data(0x24u16, disabled_reply().data()).unwrap(),
                CanFrame::new_data(STATUS_REQUEST_ID, disabled_reply().data()).unwrap(),
                CanFrame::new_data(1u16, &special_payload(SET_ZERO_COMMAND)).unwrap(),
                bitrate_reply(READ_PARAMETER_COMMAND, 4),
                telemetry_reply(1, 0.0, 0.0),
                telemetry_reply(0, 1.2, 0.0),
                telemetry_reply(0, 0.0, 0.3),
            ],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Velocity, true);
        let error = session.set_zero().await.unwrap_err();
        assert!(error.to_string().contains("saved zero is uncertain"));
        let state = session.snapshot();
        assert!(!state.streaming && !state.enabled);
        assert!(
            state.position_rad.is_none(),
            "an unconfirmed reference must not display a fabricated coordinate"
        );
        assert!(state.feedback_position_rad.unwrap() > 1.0);
        assert!(state
            .last_error
            .unwrap()
            .contains("saved zero is uncertain"));
        let frames = bus.frames.lock().unwrap();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.data() == special_payload(SET_ZERO_COMMAND))
                .count(),
            1
        );
        assert!(!frames
            .iter()
            .any(|frame| frame.data().get(2) == Some(&SAVE_PARAMETERS_COMMAND)));
    }

    #[test]
    fn supports_every_documented_classic_and_fd_bitrate_code() {
        for (code, bitrate) in DOCUMENTED_BITRATES.into_iter().enumerate() {
            let value = DamiaoBitrateState::confirmed(3, code as u32, false).unwrap();
            assert_eq!(value.bitrate, bitrate);
            assert_eq!(value.can_fd, code > 4);
        }
        assert!(DamiaoBitrateState::confirmed(3, 10, false).is_err());
        assert!(DamiaoBitrateState::confirmed(3, u32::MAX, false).is_err());
    }

    #[tokio::test]
    async fn discovers_unknown_master_id_and_reads_fd_parameter_reply() {
        let reply = CanFrame::new_fd(
            0x23u16,
            bitrate_reply(READ_PARAMETER_COMMAND, 6).data(),
            true,
        )
        .unwrap();
        let bus = ScriptedBus::new(vec![vec![reply]]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, false);
        let value = session.get_bitrate().await.unwrap();
        assert_eq!(value.bitrate, 2_500_000);
        assert!(value.can_fd);
        assert!(value.acknowledged);
        assert!(!value.requires_reconnect);
        assert_eq!(session.snapshot().feedback_can_id, Some(0x23));
        assert_eq!(bus.frames.lock().unwrap()[0].data(), [1, 0, 0x33, 0x23]);
    }

    #[tokio::test]
    async fn rejects_echo_and_other_motors_operations_registers_and_values() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let correct = bitrate_reply(WRITE_PARAMETER_COMMAND, 3);
        let mut wrong_motor = *correct.data().first().unwrap();
        wrong_motor += 1;
        let mut wrong_motor_payload = correct.data().to_vec();
        wrong_motor_payload[0] = wrong_motor;
        let mut wrong_register_payload = correct.data().to_vec();
        wrong_register_payload[3] = CONTROL_MODE_REGISTER;
        for frame in [
            CanFrame::new_data(STATUS_REQUEST_ID, correct.data()).unwrap(),
            CanFrame::new_data(0x24u16, correct.data()).unwrap(),
            CanFrame::new_data(CanId::Extended(0x23), correct.data()).unwrap(),
            CanFrame::new_data(0x23u16, &wrong_motor_payload).unwrap(),
            bitrate_reply(READ_PARAMETER_COMMAND, 3),
            CanFrame::new_data(0x23u16, &wrong_register_payload).unwrap(),
            bitrate_reply(WRITE_PARAMETER_COMMAND, 4),
            correct,
        ] {
            sender.send(frame).unwrap();
        }
        let reply = wait_parameter_reply(
            &mut TestRx(receiver),
            Some(0x23),
            1,
            WRITE_PARAMETER_COMMAND,
            CAN_BITRATE_REGISTER,
            Some(3),
        )
        .await
        .unwrap();
        assert_eq!(reply, correct);
    }

    #[tokio::test]
    async fn setting_the_current_bitrate_only_reads_and_keeps_the_stream() {
        let bus = ScriptedBus::new(vec![vec![bitrate_reply(READ_PARAMETER_COMMAND, 4)]]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let value = session.set_bitrate(1_000_000).await.unwrap();
        assert!(!value.requires_reconnect);
        assert!(session.snapshot().streaming);
        assert!(session.snapshot().enabled);
        assert_eq!(bus.frames.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bitrate_change_confirms_disabled_writes_once_and_blocks_motion_and_stale_teardown() {
        let bus = ScriptedBus::new(vec![
            vec![bitrate_reply(READ_PARAMETER_COMMAND, 4)],
            vec![disabled_reply()],
            vec![bitrate_reply(WRITE_PARAMETER_COMMAND, 3)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let value = session.set_bitrate(500_000).await.unwrap();
        assert!(value.acknowledged && value.requires_reconnect);
        assert!(session.requires_reconnect());
        assert!(!session.snapshot().streaming);
        assert!(!session.snapshot().enabled);
        assert!(session.snapshot().requires_reconnect);
        assert!(session.enable().await.is_err());
        assert!(session
            .send_target(
                DamiaoTarget::Mit {
                    position_rad: 0.0,
                    velocity_rad_s: 0.0,
                    torque_nm: 0.0,
                    kp: 0.0,
                    kd: 0.0,
                },
                true
            )
            .await
            .is_err());
        session.shutdown().await.unwrap();
        let frames = bus.frames.lock().unwrap();
        assert_eq!(
            frames.len(),
            3,
            "teardown must not send at the old host rate"
        );
        assert_eq!(frames[1].data(), special_payload(DISABLE_COMMAND));
        assert_eq!(frames[2].data(), [1, 0, 0x55, 0x23, 3, 0, 0, 0]);
        assert!(!frames
            .iter()
            .any(|frame| frame.data().get(2) == Some(&SAVE_PARAMETERS_COMMAND)));
    }

    #[tokio::test]
    async fn missing_disable_confirmation_does_not_write_the_bitrate() {
        let bus = ScriptedBus::new(vec![vec![bitrate_reply(READ_PARAMETER_COMMAND, 4)], vec![]]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        let error = session.set_bitrate(500_000).await.unwrap_err();
        assert!(error.to_string().contains("disable was not confirmed"));
        assert!(!session.requires_reconnect());
        assert!(!session.snapshot().streaming);
        assert_eq!(bus.frames.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn lost_write_acknowledgement_reports_uncertainty_and_requires_reconnect() {
        let bus = ScriptedBus::new(vec![
            vec![bitrate_reply(READ_PARAMETER_COMMAND, 4)],
            vec![disabled_reply()],
            vec![bitrate_reply(WRITE_PARAMETER_COMMAND, 4)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, false);
        let value = session.set_bitrate(500_000).await.unwrap();
        assert!(!value.acknowledged);
        assert!(value.requires_reconnect);
        assert!(!session.snapshot().online);
        assert!(session.snapshot().last_error.unwrap().contains("uncertain"));
        assert!(session.set_bitrate(500_000).await.is_err());
        assert!(session.save_parameters().await.is_err());
        session.shutdown().await.unwrap();
        assert_eq!(bus.frames.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn save_requires_disabled_feedback_and_correlated_storage_acknowledgement() {
        let bus = ScriptedBus::new(vec![
            vec![bitrate_reply(READ_PARAMETER_COMMAND, 3)],
            vec![disabled_reply()],
            vec![CanFrame::new_data(0x23u16, &[1, 0, 0xAA, 1]).unwrap()],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        session.save_parameters().await.unwrap();
        assert!(!session.snapshot().streaming);
        assert!(!session.snapshot().enabled);
        assert_eq!(bus.frames.lock().unwrap()[2].data(), [1, 0, 0xAA, 1]);

        let bus = ScriptedBus::new(vec![
            vec![bitrate_reply(READ_PARAMETER_COMMAND, 3)],
            vec![disabled_reply()],
            vec![CanFrame::new_data(0x23u16, &[1, 0, 0xAA, 2]).unwrap()],
        ]);
        let session = test_session(bus, DamiaoMode::Mit, true);
        assert!(session
            .save_parameters()
            .await
            .unwrap_err()
            .to_string()
            .contains("not acknowledged"));
    }

    #[tokio::test]
    async fn parameter_packets_do_not_contaminate_telemetry_or_discovery() {
        let bus = ScriptedBus::new(vec![vec![
            bitrate_reply(READ_PARAMETER_COMMAND, 4),
            bitrate_reply(WRITE_PARAMETER_COMMAND, 3),
            CanFrame::new_data(0x23u16, &[1, 0, 0xAA, 1, 0, 0, 0, 0]).unwrap(),
            disabled_reply(),
        ]]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, false);
        let session_rx = bus.subscribe(CanFilter::pass_all_standard()).await.unwrap();
        let discovery_rx = bus.subscribe(CanFilter::pass_all_standard()).await.unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let discovery_state = Arc::new(StdMutex::new(DiscoveryState::default()));
        discovery_state.lock().unwrap().attached.insert(1);
        let receiver = tokio::spawn(receive_loop(
            session_rx,
            session.config.clone(),
            session.state.clone(),
            running.clone(),
            session.communication_requires_reconnect.clone(),
        ));
        let discovery = tokio::spawn(discovery_receive_loop(
            discovery_rx,
            discovery_state.clone(),
            running,
        ));
        bus.send(CanFrame::new_data(1u16, &[0]).unwrap())
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(session.snapshot().rx_count, 1);
        assert_eq!(discovery_state.lock().unwrap().devices[&1].rx_count, 1);
        receiver.abort();
        discovery.abort();
    }

    #[test]
    fn accepts_documented_j4310_calibration_and_sensor_faults() {
        for (code, expected) in [
            (3, "Output-shaft calibration fault"),
            (4, "Sensor output fault"),
            (5, "Motor encoder calibration fault"),
        ] {
            let mut payload = *disabled_reply().data().first().unwrap();
            payload |= code << 4;
            let mut data = disabled_reply().data().to_vec();
            data[0] = payload;
            assert_eq!(feedback_identity(&data), Some((1, code)));
            assert_eq!(
                decode_feedback(config(DamiaoMode::Mit), &data)
                    .unwrap()
                    .unwrap()
                    .status_code,
                code
            );
            assert_eq!(status_name(code), expected);
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
        let bus = ScriptedBus::new(vec![
            vec![disabled_reply()],
            vec![mode_reply(DamiaoMode::Velocity)],
            vec![],
            vec![telemetry_reply(1, 0.0, 0.0)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);

        let state = session.switch_mode(DamiaoMode::Velocity).await.unwrap();
        assert_eq!(state.mode, DamiaoMode::Velocity);
        assert!(!state.streaming, "a mode switch must stop the old stream");

        assert!(state.enabled && state.online);
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0].data(), special_payload(DISABLE_COMMAND));
        assert_eq!(
            frames[1].data(),
            control_mode_payload(1, DamiaoMode::Velocity)
        );
        assert_eq!(frames[2].id(), CanId::Standard(0x201));
        assert_eq!(frames[2].data(), 0.0f32.to_le_bytes());
        assert_eq!(frames[3].data(), special_payload(ENABLE_COMMAND));
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
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames[4].id(), CanId::Standard(0x201));
        assert_eq!(frames[4].data(), 5.0f32.to_le_bytes());
        assert!(session.snapshot().streaming);
    }

    #[tokio::test]
    async fn enabling_preloads_precise_multiturn_position_and_requires_enabled_feedback() {
        let bus = ScriptedBus::new(vec![
            vec![disabled_reply()],
            vec![float_parameter_reply(PRECISE_POSITION_REGISTER, 31.25)],
            vec![float_parameter_reply(OUTPUT_POSITION_REGISTER, 31.24)],
            vec![mode_reply(DamiaoMode::PositionVelocity)],
            vec![],
            vec![telemetry_reply(1, 6.25, 0.0)],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::PositionVelocity, false);
        let state = session.enable().await.unwrap();
        assert!(state.enabled && state.online && !state.streaming);
        assert_eq!(state.position_rad, Some(31.25));
        let frames = bus.frames.lock().unwrap();
        assert_eq!(frames[4].id(), CanId::Standard(0x101));
        assert_eq!(&frames[4].data()[..4], &31.25f32.to_le_bytes());
        assert_eq!(&frames[4].data()[4..], &0.0f32.to_le_bytes());
        assert_eq!(frames[5].data(), special_payload(ENABLE_COMMAND));
        assert!(!frames
            .iter()
            .any(|frame| frame.data().get(2) == Some(&SAVE_PARAMETERS_COMMAND)));
    }

    #[tokio::test]
    async fn lost_enable_reply_recovers_to_disabled_without_starting_a_stream() {
        let bus = ScriptedBus::new(vec![
            vec![disabled_reply()],
            vec![mode_reply(DamiaoMode::Velocity)],
            vec![],
            vec![],
            vec![disabled_reply()],
        ]);
        let session = test_session(bus.clone(), DamiaoMode::Velocity, false);
        let error = session.enable().await.unwrap_err().to_string();
        assert!(error.contains("enable was not confirmed"));
        assert!(!session.snapshot().enabled && !session.snapshot().streaming);
        assert_eq!(
            bus.frames.lock().unwrap().last().unwrap().data(),
            special_payload(DISABLE_COMMAND)
        );
    }

    #[tokio::test]
    async fn missing_mode_ack_or_moving_shaft_never_sends_enable() {
        for replies in [
            vec![vec![disabled_reply()], vec![], vec![disabled_reply()]],
            vec![vec![telemetry_reply(0, 0.0, 1.0)], vec![disabled_reply()]],
        ] {
            let bus = ScriptedBus::new(replies);
            let session = test_session(bus.clone(), DamiaoMode::Velocity, false);
            assert!(session.enable().await.is_err());
            assert!(!bus
                .frames
                .lock()
                .unwrap()
                .iter()
                .any(|frame| frame.data() == special_payload(ENABLE_COMMAND)));
        }
    }

    #[tokio::test]
    async fn stale_feedback_rejects_motion_even_if_the_last_state_was_enabled() {
        let bus = Arc::new(RecordingBus::default());
        let session = test_session(bus.clone(), DamiaoMode::Velocity, true);
        session.state.lock().unwrap().last_feedback =
            Some(Instant::now() - FEEDBACK_FRESH_FOR - Duration::from_millis(1));
        assert!(session
            .send_target(
                DamiaoTarget::Velocity {
                    velocity_rad_s: 1.0
                },
                true
            )
            .await
            .is_err());
        assert!(bus.frames.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn periodic_stream_disables_once_when_feedback_goes_stale() {
        let bus = Arc::new(RecordingBus::default());
        let session = test_session(bus.clone(), DamiaoMode::Mit, true);
        session.state.lock().unwrap().last_feedback =
            Some(Instant::now() - FEEDBACK_FRESH_FOR - Duration::from_millis(1));
        session.running.store(true, Ordering::Release);
        let task = tokio::spawn(stream_loop(
            bus.clone(),
            session.config.clone(),
            session.state.clone(),
            session.stream_target.clone(),
            session.control_gate.clone(),
            session.running.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while session.stream_target.lock().unwrap().is_some() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        session.running.store(false, Ordering::Release);
        task.await.unwrap();
        assert_eq!(
            *bus.frames.lock().unwrap(),
            vec![(
                CanId::Standard(1),
                special_payload(DISABLE_COMMAND).to_vec()
            )]
        );
        assert!(session
            .snapshot()
            .last_error
            .unwrap()
            .contains("periodic target stopped"));
    }

    #[test]
    fn commands_share_the_protocol_without_voltage_specific_caps() {
        let mapping = DamiaoConfig {
            v_max: 50.0,
            t_max: 15.0,
            ..config(DamiaoMode::Mit)
        };
        assert!(encode_target(
            mapping,
            DamiaoTarget::Mit {
                position_rad: 0.0,
                velocity_rad_s: 45.0,
                torque_nm: 12.5,
                kp: 0.0,
                kd: 0.0
            }
        )
        .is_ok());
        assert!(encode_target(
            mapping,
            DamiaoTarget::Mit {
                position_rad: 0.0,
                velocity_rad_s: 51.0,
                torque_nm: 0.0,
                kp: 0.0,
                kd: 0.0
            }
        )
        .is_err());
        for speed in [-100.0, 100.0] {
            let bytes = encode_target(
                config(DamiaoMode::Velocity),
                DamiaoTarget::Velocity {
                    velocity_rad_s: speed,
                },
            )
            .unwrap();
            assert_eq!(&bytes[..4], &speed.to_le_bytes());
        }
        assert!(encode_target(
            config(DamiaoMode::PositionVelocity),
            DamiaoTarget::PositionVelocity {
                position_rad: 100.0,
                velocity_rad_s: 100.0
            }
        )
        .is_ok());
        for speed in [-1.0, f32::NAN, f32::INFINITY] {
            assert!(encode_target(
                config(DamiaoMode::PositionVelocity),
                DamiaoTarget::PositionVelocity {
                    position_rad: 0.0,
                    velocity_rad_s: speed
                }
            )
            .is_err());
        }
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
        assert!(is_parameter_reply_for(
            4,
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
    fn discovery_consumes_matching_probes_and_expires_detached_devices() {
        let started = Instant::now();
        let mut state = DiscoveryState {
            pending: Some(PendingProbe {
                motor_id: 1,
                sent_at: started,
            }),
            ..DiscoveryState::default()
        };

        assert!(record_discovery_feedback(
            &mut state,
            1,
            0,
            1,
            started + Duration::from_millis(10),
        ));
        assert!(state.pending.is_none());
        assert!(state.devices.contains_key(&1));

        let expired_at = started + DISCOVERY_FRESH_FOR + Duration::from_millis(11);
        prune_discovery_devices(&mut state, expired_at);
        assert!(!state.devices.contains_key(&1));

        // A late unsolicited frame cannot resurrect an expired, detached ID.
        assert!(!record_discovery_feedback(&mut state, 1, 0, 1, expired_at));
        assert!(!state.devices.contains_key(&1));

        // Attached sessions remain visible while offline and may recover from
        // feedback without waiting for the scanner's current probe slot.
        state.attached.insert(1);
        assert!(record_discovery_feedback(&mut state, 1, 0, 1, expired_at));
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
    fn mit_targets_use_the_confirmed_drive_mapping_in_both_directions() {
        let small_mapping = DamiaoConfig {
            p_max: 6.0,
            v_max: 10.0,
            t_max: 5.0,
            ..config(DamiaoMode::Mit)
        };
        let bytes = encode_target(
            small_mapping,
            DamiaoTarget::Mit {
                position_rad: 1.0,
                velocity_rad_s: 1.0,
                torque_nm: 1.0,
                kp: 0.0,
                kd: 0.0,
            },
        )
        .unwrap();
        let velocity = (u16::from(bytes[2]) << 4) | u16::from(bytes[3] >> 4);
        let torque = (u16::from(bytes[6] & 0x0F) << 8) | u16::from(bytes[7]);

        assert_eq!(
            velocity,
            float_to_uint(
                1.0,
                -small_mapping.v_max,
                small_mapping.v_max,
                12,
                "velocity"
            )
            .unwrap()
        );
        assert_eq!(
            torque,
            float_to_uint(1.0, -small_mapping.t_max, small_mapping.t_max, 12, "torque").unwrap()
        );
        assert_ne!(velocity, 0x0FFF);
        assert_ne!(torque, 0x0FFF);
        let feedback = [
            1,
            bytes[0],
            bytes[1],
            (velocity >> 4) as u8,
            ((velocity & 0x0F) << 4) as u8 | (torque >> 8) as u8,
            torque as u8,
            25,
            26,
        ];
        let decoded = decode_feedback(small_mapping, &feedback).unwrap().unwrap();
        assert!((decoded.position_rad - 1.0).abs() < 0.001);
        assert!((decoded.velocity_rad_s - 1.0).abs() < 0.01);
        assert!((decoded.torque_nm - 1.0).abs() < 0.01);
    }

    #[test]
    fn configurable_mapping_ranges_allow_larger_than_default_scales() {
        assert!(DamiaoConfig {
            p_max: DM_J4310_MAPPING_P_MAX_RAD + 0.1,
            ..config(DamiaoMode::Mit)
        }
        .validate()
        .is_ok());
        assert!(DamiaoConfig {
            v_max: DM_J4310_MAPPING_V_MAX_RAD_S + 0.1,
            ..config(DamiaoMode::Mit)
        }
        .validate()
        .is_ok());
        assert!(DamiaoConfig {
            t_max: DM_J4310_MAPPING_T_MAX_NM + 0.1,
            ..config(DamiaoMode::Mit)
        }
        .validate()
        .is_ok());
        for value in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(DamiaoConfig {
                p_max: value,
                ..config(DamiaoMode::Mit)
            }
            .validate()
            .is_err());
        }
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
        .unwrap()
        .expect("feedback for configured motor");
        assert_eq!(feedback.status_code, 1);
        assert!(feedback.position_rad.abs() < 0.001);
        assert!(feedback.velocity_rad_s.abs() < 0.02);
        assert!(feedback.torque_nm.abs() < 0.01);
        assert_eq!(feedback.mos_temp_c, 42);
        assert_eq!(feedback.rotor_temp_c, 39);
        assert!(decode_feedback(
            config(DamiaoMode::Mit),
            &[0x12, 0x7F, 0xFF, 0x7F, 0xF7, 0xFF, 42, 39],
        )
        .unwrap()
        .is_none());
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
