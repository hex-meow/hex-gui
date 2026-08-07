//! Unloaded command-domain friction calibration for new-protocol Meow Motors.
//!
//! The Rust side owns the complete motion sequence. The WebView can poll and
//! cancel it, but never owns CAN timing. Every motion pass uses bounded SDO
//! commands, fresh TPDO feedback, an exact identity/session fence, and a final
//! zero-target + Disabled cleanup.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use can_transport::CanBus;
use hex_motor::canopen::sdo;
use hex_motor::meow_motor::{
    MeowMotorLifecycle, MeowMotorManager, MeowMotorTarget, MeowProfileLimits, Tpdo1Rate,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::calibration_transport::CalibrationHeartbeat;

const KINETIC_REFERENCE_SPEED_RAD_PER_S: f64 = 1.0;
const KINETIC_MEAN_SPEED_TOLERANCE_RAD_PER_S: f64 = 0.15;
const KINETIC_SPEED_STDDEV_LIMIT_RAD_PER_S: f64 = 0.25;
const KINETIC_STABLE_WINDOW: Duration = Duration::from_millis(800);
const KINETIC_SETTLE_TIMEOUT: Duration = Duration::from_secs(7);
const STILL_SPEED_RAD_PER_S: f64 = 0.03;
const STILL_TIME: Duration = Duration::from_millis(300);
const STILL_TIMEOUT: Duration = Duration::from_secs(4);
const SAMPLE_TIMEOUT: Duration = Duration::from_millis(300);
const POLL_PERIOD: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrictionCalibrationRequest {
    pub node_id: u8,
    pub expected_vendor_id: u32,
    pub expected_product_code: u32,
    pub expected_revision_number: u32,
    pub expected_serial_number: u32,
    pub torque_step_permille: u16,
    pub max_torque_permille: u16,
    pub step_dwell_ms: u16,
    pub movement_position_threshold_rad: f64,
    pub movement_velocity_threshold_rad_per_s: f64,
    pub kinetic_sample_ms: u16,
}

impl FrictionCalibrationRequest {
    fn validate(self) -> Result<(), String> {
        if !(1..=127).contains(&self.node_id) {
            return Err(format!("node ID must be 1..=127, got {}", self.node_id));
        }
        if self.expected_vendor_id == 0
            || self.expected_product_code == 0
            || self.expected_serial_number == 0
        {
            return Err("the exact expected vendor/product/serial identity is required".into());
        }
        if !(1..=20).contains(&self.torque_step_permille) {
            return Err("torque step must be 1..=20 permille".into());
        }
        if self.max_torque_permille < self.torque_step_permille || self.max_torque_permille > 200 {
            return Err("maximum test torque must be >= one step and <= 200 permille".into());
        }
        if !(100..=1_000).contains(&self.step_dwell_ms) {
            return Err("step dwell must be 100..=1000 ms".into());
        }
        if !self.movement_position_threshold_rad.is_finite()
            || !(0.001..=0.1).contains(&self.movement_position_threshold_rad)
        {
            return Err("movement position threshold must be 0.001..=0.1 rad".into());
        }
        if !self.movement_velocity_threshold_rad_per_s.is_finite()
            || !(0.01..=0.5).contains(&self.movement_velocity_threshold_rad_per_s)
        {
            return Err("movement velocity threshold must be 0.01..=0.5 rad/s".into());
        }
        if !(500..=5_000).contains(&self.kinetic_sample_ms) {
            return Err("kinetic sample window must be 500..=5000 ms".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FrictionCalibrationResult {
    pub node_id: u8,
    pub vendor_id: u32,
    pub product_code: u32,
    pub revision_number: u32,
    pub serial_number: u32,
    pub peak_torque_nm: f64,
    pub static_pos_raw_nm: f64,
    pub static_neg_raw_nm: f64,
    pub kinetic_pos_raw_nm: f64,
    pub kinetic_neg_raw_nm: f64,
    pub static_pos_permille: u16,
    pub static_neg_permille: u16,
    pub kinetic_pos_mean_permille: f64,
    pub kinetic_neg_mean_permille: f64,
    pub kinetic_pos_stddev_permille: f64,
    pub kinetic_neg_stddev_permille: f64,
    pub kinetic_reference_speed_rad_per_s: f64,
    pub kinetic_pos_mean_speed_rad_per_s: f64,
    pub kinetic_neg_mean_speed_rad_per_s: f64,
    pub calibration_temperature_c: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrictionCalibrationView {
    pub running: bool,
    pub phase: String,
    pub progress_percent: u8,
    pub node_id: Option<u8>,
    pub current_command_permille: i16,
    pub position_rad: Option<f64>,
    pub velocity_rad_per_s: Option<f64>,
    pub actual_torque_permille: Option<i16>,
    pub motor_temperature_c: Option<f64>,
    pub result: Option<FrictionCalibrationResult>,
    pub error: Option<String>,
    pub cleanup_warning: Option<String>,
}

impl Default for FrictionCalibrationView {
    fn default() -> Self {
        Self {
            running: false,
            phase: "idle".into(),
            progress_percent: 0,
            node_id: None,
            current_command_permille: 0,
            position_rad: None,
            velocity_rad_per_s: None,
            actual_torque_permille: None,
            motor_temperature_c: None,
            result: None,
            error: None,
            cleanup_warning: None,
        }
    }
}

#[derive(Default)]
struct Session {
    view: FrictionCalibrationView,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Clone, Default)]
pub struct FrictionCalibrationState {
    inner: Arc<Mutex<Session>>,
}

impl FrictionCalibrationState {
    pub async fn view(&self) -> FrictionCalibrationView {
        self.inner.lock().await.view.clone()
    }

    pub async fn start(
        &self,
        manager: Arc<MeowMotorManager>,
        bus: Arc<dyn CanBus>,
        host_node_id: u8,
        request: FrictionCalibrationRequest,
    ) -> Result<FrictionCalibrationView, String> {
        request.validate()?;

        let previous = {
            let mut session = self.inner.lock().await;
            if session.view.running {
                return Err("a friction calibration is already running".into());
            }
            session.task.take()
        };
        if let Some(previous) = previous {
            let _ = previous.await;
        }

        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut session = self.inner.lock().await;
            session.view = FrictionCalibrationView {
                running: true,
                phase: "preparing".into(),
                progress_percent: 1,
                node_id: Some(request.node_id),
                ..Default::default()
            };
            session.cancel = Some(cancel.clone());
        }

        let shared = self.inner.clone();
        let task = tokio::spawn(async move {
            let outcome = match CalibrationHeartbeat::start(bus.clone(), host_node_id) {
                Ok(heartbeat) => {
                    let outcome = run_and_cleanup(&shared, &manager, &bus, request, &cancel).await;
                    heartbeat.stop().await;
                    outcome
                }
                Err(error) => Err(error),
            };
            let mut session = shared.lock().await;
            session.view.running = false;
            session.view.current_command_permille = 0;
            match outcome {
                Ok(result) => {
                    session.view.phase = "completed".into();
                    session.view.progress_percent = 100;
                    session.view.result = Some(result);
                }
                Err(error) if cancel.load(Ordering::Acquire) => {
                    session.view.phase = "cancelled".into();
                    session.view.error = Some(error);
                }
                Err(error) => {
                    session.view.phase = "failed".into();
                    session.view.error = Some(error);
                }
            }
            session.cancel = None;
        });
        self.inner.lock().await.task = Some(task);
        Ok(self.view().await)
    }

    pub async fn stop(&self) -> FrictionCalibrationView {
        let task = {
            let mut session = self.inner.lock().await;
            if let Some(cancel) = &session.cancel {
                cancel.store(true, Ordering::Release);
            }
            session.task.take()
        };
        if let Some(task) = task {
            let _ = task.await;
        }
        self.view().await
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeSettings {
    consumer_heartbeat: u32,
    max_torque_permille: u16,
    profile_limits: MeowProfileLimits,
}

impl RuntimeSettings {
    pub(crate) async fn read(bus: &dyn CanBus, node_id: u8) -> Result<Self, String> {
        let timeout = Some(Duration::from_millis(500));
        let consumer_heartbeat = sdo::upload_u32(bus, node_id, 0x1016, 1, timeout)
            .await
            .map_err(to_string)?;
        let max_torque_permille = sdo::upload_u16(bus, node_id, 0x4572, 0, timeout)
            .await
            .map_err(to_string)?;
        if max_torque_permille > 1_000 {
            return Err(format!(
                "pre-test 0x4572 maximum torque is not restorable: {max_torque_permille} permille"
            ));
        }
        let profile_limits = MeowProfileLimits {
            velocity_rev_per_s: sdo::upload_f32(bus, node_id, 0x4581, 0, timeout)
                .await
                .map_err(to_string)?,
            acceleration_rev_per_s2: sdo::upload_f32(bus, node_id, 0x4583, 0, timeout)
                .await
                .map_err(to_string)?,
            deceleration_rev_per_s2: sdo::upload_f32(bus, node_id, 0x4584, 0, timeout)
                .await
                .map_err(to_string)?,
        };
        profile_limits.validate().map_err(to_string)?;
        Ok(Self {
            consumer_heartbeat,
            max_torque_permille,
            profile_limits,
        })
    }
}

async fn run_and_cleanup(
    shared: &Arc<Mutex<Session>>,
    manager: &Arc<MeowMotorManager>,
    bus: &Arc<dyn CanBus>,
    request: FrictionCalibrationRequest,
    cancel: &Arc<AtomicBool>,
) -> Result<FrictionCalibrationResult, String> {
    // Identification and the exact expected identity fence are read-only. Do
    // not arm the cleanup writer for an unsupported or unexpected device that
    // merely happens to answer at the selected node ID.
    check_cancel(cancel)?;
    set_phase(shared, "preparing", 3).await;
    manager.identify(request.node_id).await.map_err(to_string)?;
    verify_identity(manager, request)?;

    // Snapshot every RAM value this application changes before initialize()
    // performs its first write. If this read fails, no calibration mutation or
    // cleanup write is attempted.
    let original = RuntimeSettings::read(bus.as_ref(), request.node_id).await?;
    let result = run_sequence(shared, manager, request, cancel).await;
    set_phase(shared, "cleanup", 96).await;
    let cleanup = safe_cleanup(manager, bus.as_ref(), request.node_id, original).await;
    if let Err(error) = cleanup {
        shared.lock().await.view.cleanup_warning = Some(error.clone());
        if result.is_ok() {
            return Err(format!(
                "measurement completed, but safe cleanup failed: {error}"
            ));
        }
    }
    result
}

async fn run_sequence(
    shared: &Arc<Mutex<Session>>,
    manager: &Arc<MeowMotorManager>,
    request: FrictionCalibrationRequest,
    cancel: &Arc<AtomicBool>,
) -> Result<FrictionCalibrationResult, String> {
    check_cancel(cancel)?;
    verify_identity(manager, request)?;
    manager
        .initialize(request.node_id, Tpdo1Rate::Hz1000)
        .await
        .map_err(to_string)?;
    let info = verify_identity(manager, request)?;
    if !info.is_ready() {
        return Err("motor did not enter a fresh Initialized session".into());
    }
    let peak_torque_nm = f64::from(
        info.peak_torque_nm
            .filter(|value| value.is_finite() && *value > 0.0)
            .ok_or_else(|| "0x4576 peak torque is missing or invalid".to_string())?,
    );
    let epoch = info.session_epoch;

    manager
        .set_max_torque(request.node_id, request.max_torque_permille)
        .await
        .map_err(to_string)?;
    manager.disable(request.node_id).await.map_err(to_string)?;
    let mut generation = manager
        .status(request.node_id)
        .map_err(to_string)?
        .measurements
        .tpdo1_generation;
    set_phase(shared, "settling_initial", 7).await;
    wait_still(
        shared,
        manager,
        request.node_id,
        epoch,
        &mut generation,
        cancel,
    )
    .await?;

    let static_pos_permille =
        ramp_static(shared, manager, request, epoch, &mut generation, cancel, 1).await?;
    set_phase(shared, "settling_after_static_positive", 30).await;
    wait_still(
        shared,
        manager,
        request.node_id,
        epoch,
        &mut generation,
        cancel,
    )
    .await?;

    let static_neg_permille =
        ramp_static(shared, manager, request, epoch, &mut generation, cancel, -1).await?;
    set_phase(shared, "settling_after_static_negative", 47).await;
    wait_still(
        shared,
        manager,
        request.node_id,
        epoch,
        &mut generation,
        cancel,
    )
    .await?;

    let profile_speed_rev_per_s = KINETIC_REFERENCE_SPEED_RAD_PER_S / std::f64::consts::TAU;
    let profile_accel_rev_per_s2 = 1.0 / std::f64::consts::TAU;
    manager
        .set_profile_limits(
            request.node_id,
            MeowProfileLimits {
                velocity_rev_per_s: profile_speed_rev_per_s as f32,
                acceleration_rev_per_s2: profile_accel_rev_per_s2 as f32,
                deceleration_rev_per_s2: profile_accel_rev_per_s2 as f32,
            },
        )
        .await
        .map_err(to_string)?;

    let kinetic_pos =
        measure_kinetic(shared, manager, request, epoch, &mut generation, cancel, 1).await?;
    set_phase(shared, "settling_between_kinetic_passes", 72).await;
    wait_still(
        shared,
        manager,
        request.node_id,
        epoch,
        &mut generation,
        cancel,
    )
    .await?;

    let kinetic_neg =
        measure_kinetic(shared, manager, request, epoch, &mut generation, cancel, -1).await?;
    set_phase(shared, "settling_final", 92).await;
    wait_still(
        shared,
        manager,
        request.node_id,
        epoch,
        &mut generation,
        cancel,
    )
    .await?;
    verify_identity(manager, request)?;

    let temperature_c = (kinetic_pos.mean_temperature_c + kinetic_neg.mean_temperature_c) / 2.0;
    Ok(FrictionCalibrationResult {
        node_id: request.node_id,
        vendor_id: request.expected_vendor_id,
        product_code: request.expected_product_code,
        revision_number: request.expected_revision_number,
        serial_number: request.expected_serial_number,
        peak_torque_nm,
        static_pos_raw_nm: f64::from(static_pos_permille) * peak_torque_nm / 1000.0,
        static_neg_raw_nm: f64::from(static_neg_permille) * peak_torque_nm / 1000.0,
        kinetic_pos_raw_nm: kinetic_pos.mean_torque_permille * peak_torque_nm / 1000.0,
        kinetic_neg_raw_nm: kinetic_neg.mean_torque_permille * peak_torque_nm / 1000.0,
        static_pos_permille,
        static_neg_permille,
        kinetic_pos_mean_permille: kinetic_pos.mean_torque_permille,
        kinetic_neg_mean_permille: kinetic_neg.mean_torque_permille,
        kinetic_pos_stddev_permille: kinetic_pos.stddev_torque_permille,
        kinetic_neg_stddev_permille: kinetic_neg.stddev_torque_permille,
        kinetic_reference_speed_rad_per_s: KINETIC_REFERENCE_SPEED_RAD_PER_S,
        kinetic_pos_mean_speed_rad_per_s: kinetic_pos.mean_speed_rad_per_s,
        kinetic_neg_mean_speed_rad_per_s: kinetic_neg.mean_speed_rad_per_s,
        calibration_temperature_c: temperature_c,
    })
}

fn verify_identity(
    manager: &MeowMotorManager,
    request: FrictionCalibrationRequest,
) -> Result<hex_motor::meow_motor::MeowMotorInfo, String> {
    let info = manager
        .list()
        .into_iter()
        .find(|info| info.node_id == request.node_id)
        .ok_or_else(|| format!("node 0x{:02X} disappeared", request.node_id))?;
    let identity = info
        .identity
        .as_ref()
        .ok_or_else(|| "motor identity is unavailable".to_string())?;
    let observed = (
        identity.vendor_id,
        identity.product_code,
        identity.revision_number,
        identity.serial_number,
    );
    let expected = (
        request.expected_vendor_id,
        request.expected_product_code,
        request.expected_revision_number,
        request.expected_serial_number,
    );
    if observed != expected {
        return Err(format!(
            "identity changed: expected {expected:08X?}, observed {observed:08X?}"
        ));
    }
    if !info.online {
        return Err("the selected motor is offline".into());
    }
    Ok(info)
}

async fn ramp_static(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    request: FrictionCalibrationRequest,
    epoch: u64,
    generation: &mut u64,
    cancel: &Arc<AtomicBool>,
    direction: i16,
) -> Result<u16, String> {
    let (phase, progress_start, progress_end) = if direction > 0 {
        ("static_positive", 10_u8, 27_u8)
    } else {
        ("static_negative", 32_u8, 45_u8)
    };
    set_phase(shared, phase, progress_start).await;
    let baseline = fresh_sample(shared, manager, request.node_id, epoch, generation, cancel)
        .await?
        .measurements
        .accumulated_position_rev
        .ok_or_else(|| "wide position is unavailable".to_string())?;

    manager
        .set_mode_sdo(
            request.node_id,
            MeowMotorTarget::Torque { torque_permille: 0 },
        )
        .await
        .map_err(to_string)?;

    let steps = request
        .max_torque_permille
        .div_ceil(request.torque_step_permille);
    for index in 1..=steps {
        check_cancel(cancel)?;
        let magnitude = (index * request.torque_step_permille).min(request.max_torque_permille);
        let command = direction * magnitude as i16;
        {
            let mut session = shared.lock().await;
            session.view.current_command_permille = command;
            session.view.progress_percent = progress_start
                + ((progress_end - progress_start) as u16 * index / steps.max(1)) as u8;
        }
        manager
            .set_target_sdo(
                request.node_id,
                MeowMotorTarget::Torque {
                    torque_permille: command,
                },
            )
            .await
            .map_err(to_string)?;

        let deadline = Instant::now() + Duration::from_millis(u64::from(request.step_dwell_ms));
        while Instant::now() < deadline {
            let sample =
                fresh_sample(shared, manager, request.node_id, epoch, generation, cancel).await?;
            let position = sample
                .measurements
                .accumulated_position_rev
                .ok_or_else(|| "wide position disappeared".to_string())?;
            let velocity = sample
                .measurements
                .velocity_rev_per_s
                .ok_or_else(|| "velocity disappeared".to_string())?
                * std::f64::consts::TAU;
            let signed_displacement =
                (position - baseline) * std::f64::consts::TAU * f64::from(direction);
            let signed_velocity = velocity * f64::from(direction);
            if signed_displacement >= request.movement_position_threshold_rad
                && signed_velocity >= request.movement_velocity_threshold_rad_per_s
            {
                stop_current_mode(
                    manager,
                    request.node_id,
                    MeowMotorTarget::Torque { torque_permille: 0 },
                )
                .await?;
                shared.lock().await.view.current_command_permille = 0;
                return Ok(magnitude);
            }
        }
    }
    Err(format!(
        "no effective {} motion before the {} permille safety ceiling",
        if direction > 0 {
            "positive"
        } else {
            "negative"
        },
        request.max_torque_permille
    ))
}

#[derive(Debug)]
struct KineticMeasurement {
    mean_torque_permille: f64,
    stddev_torque_permille: f64,
    mean_speed_rad_per_s: f64,
    mean_temperature_c: f64,
}

async fn measure_kinetic(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    request: FrictionCalibrationRequest,
    epoch: u64,
    generation: &mut u64,
    cancel: &Arc<AtomicBool>,
    direction: i16,
) -> Result<KineticMeasurement, String> {
    let (phase, progress) = if direction > 0 {
        ("kinetic_positive", 50)
    } else {
        ("kinetic_negative", 75)
    };
    set_phase(shared, phase, progress).await;
    let target_rad_per_s = f64::from(direction) * KINETIC_REFERENCE_SPEED_RAD_PER_S;
    manager
        .set_mode_sdo(
            request.node_id,
            MeowMotorTarget::ProfileVelocity {
                velocity_rev_per_s: (target_rad_per_s / std::f64::consts::TAU) as f32,
            },
        )
        .await
        .map_err(to_string)?;

    let deadline = Instant::now() + KINETIC_SETTLE_TIMEOUT;
    let mut window_started = Instant::now();
    let mut settle_speeds = Vec::new();
    loop {
        let sample =
            fresh_sample(shared, manager, request.node_id, epoch, generation, cancel).await?;
        let speed = sample
            .measurements
            .velocity_rev_per_s
            .ok_or_else(|| "velocity disappeared".to_string())?
            * std::f64::consts::TAU;
        settle_speeds.push(speed);
        if window_started.elapsed() >= KINETIC_STABLE_WINDOW {
            let (mean_speed, speed_stddev) = trimmed_mean_stddev(&settle_speeds)?;
            if (mean_speed - target_rad_per_s).abs() <= KINETIC_MEAN_SPEED_TOLERANCE_RAD_PER_S
                && speed_stddev <= KINETIC_SPEED_STDDEV_LIMIT_RAD_PER_S
            {
                break;
            }
            settle_speeds.clear();
            window_started = Instant::now();
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} velocity did not settle at {target_rad_per_s:.3} rad/s",
                if direction > 0 {
                    "positive"
                } else {
                    "negative"
                }
            ));
        }
    }

    let sample_deadline =
        Instant::now() + Duration::from_millis(u64::from(request.kinetic_sample_ms));
    let mut torque = Vec::new();
    let mut speed = Vec::new();
    let mut temperature = Vec::new();
    while Instant::now() < sample_deadline {
        let sample =
            fresh_sample(shared, manager, request.node_id, epoch, generation, cancel).await?;
        let sample_speed = sample
            .measurements
            .velocity_rev_per_s
            .ok_or_else(|| "velocity disappeared".to_string())?
            * std::f64::consts::TAU;
        let sample_torque = sample
            .measurements
            .torque_permille
            .ok_or_else(|| "actual torque feedback disappeared".to_string())?;
        let sample_temperature = sample
            .measurements
            .motor_temp_c
            .filter(|value| value.is_finite())
            .ok_or_else(|| "motor temperature disappeared".to_string())?;
        torque.push(f64::from(direction) * f64::from(sample_torque));
        speed.push(sample_speed);
        temperature.push(f64::from(sample_temperature));
    }
    stop_current_mode(
        manager,
        request.node_id,
        MeowMotorTarget::ProfileVelocity {
            velocity_rev_per_s: 0.0,
        },
    )
    .await?;

    if torque.len() < 20 {
        return Err(format!(
            "only {} fresh kinetic samples were captured",
            torque.len()
        ));
    }
    let (mean_speed_rad_per_s, speed_stddev_rad_per_s) = trimmed_mean_stddev(&speed)?;
    if (mean_speed_rad_per_s - target_rad_per_s).abs() > KINETIC_MEAN_SPEED_TOLERANCE_RAD_PER_S
        || speed_stddev_rad_per_s > KINETIC_SPEED_STDDEV_LIMIT_RAD_PER_S
    {
        return Err(format!(
            "kinetic speed window is not stable: mean={mean_speed_rad_per_s:.3}, \
             stddev={speed_stddev_rad_per_s:.3} rad/s"
        ));
    }
    let (mean_torque_permille, stddev_torque_permille) = trimmed_mean_stddev(&torque)?;
    if mean_torque_permille <= 0.0 {
        return Err(format!(
            "opposing actual torque has invalid mean {mean_torque_permille:.3} permille"
        ));
    }
    Ok(KineticMeasurement {
        mean_torque_permille,
        stddev_torque_permille,
        mean_speed_rad_per_s,
        mean_temperature_c: mean(&temperature)?,
    })
}

async fn stop_current_mode(
    manager: &MeowMotorManager,
    node_id: u8,
    zero: MeowMotorTarget,
) -> Result<(), String> {
    manager
        .set_target_sdo(node_id, zero)
        .await
        .map_err(to_string)?;
    manager.disable(node_id).await.map_err(to_string)
}

pub(crate) async fn safe_cleanup(
    manager: &MeowMotorManager,
    bus: &dyn CanBus,
    node_id: u8,
    original: RuntimeSettings,
) -> Result<(), String> {
    // Only the target matching the active mode can be accepted. Trying both
    // zero forms is bounded and never retries a missing ACK.
    let _ = manager
        .set_target_sdo(node_id, MeowMotorTarget::Torque { torque_permille: 0 })
        .await;
    let _ = manager
        .set_target_sdo(
            node_id,
            MeowMotorTarget::ProfileVelocity {
                velocity_rev_per_s: 0.0,
            },
        )
        .await;
    let mut errors = Vec::new();
    if let Err(error) = manager.disable(node_id).await {
        errors.push(format!("initial Disabled: {error}"));
    }
    if let Err(error) = manager
        .set_profile_limits(node_id, original.profile_limits)
        .await
    {
        errors.push(format!("restore profile limits: {error}"));
    }
    if let Err(error) = manager
        .set_max_torque(node_id, original.max_torque_permille)
        .await
    {
        errors.push(format!("restore maximum torque: {error}"));
    }

    // New-protocol recovery does not use NMT Reset. Disarm heartbeat
    // consumption first, verify 0x1016:01 really is zero, then issue one
    // 0x4401=0xFF pulse and require Disabled confirmation. Only after that is
    // the exact pre-test heartbeat setting restored and read back.
    let timeout = Some(Duration::from_millis(500));
    if let Err(error) = sdo::download(bus, node_id, 0x1016, 1, &0_u32.to_le_bytes(), timeout).await
    {
        errors.push(format!("clear 0x1016:01 before 0x4401=0xFF: {error}"));
    } else {
        match sdo::upload_u32(bus, node_id, 0x1016, 1, timeout).await {
            Ok(0) => {}
            Ok(observed) => errors.push(format!(
                "0x1016:01 clear mismatch before 0x4401=0xFF: got 0x{observed:08X}"
            )),
            Err(error) => errors.push(format!("read back cleared 0x1016:01: {error}")),
        }
    }
    if let Err(error) = manager.clear_error(node_id).await {
        errors.push(format!("0x4401=0xFF clear/Disabled confirmation: {error}"));
    }
    if let Err(error) = sdo::download(
        bus,
        node_id,
        0x1016,
        1,
        &original.consumer_heartbeat.to_le_bytes(),
        timeout,
    )
    .await
    {
        errors.push(format!("restore 0x1016:01: {error}"));
    } else {
        match sdo::upload_u32(bus, node_id, 0x1016, 1, timeout).await {
            Ok(observed) if observed == original.consumer_heartbeat => {}
            Ok(observed) => errors.push(format!(
                "0x1016:01 restore mismatch: expected 0x{:08X}, got 0x{observed:08X}",
                original.consumer_heartbeat
            )),
            Err(error) => errors.push(format!("read back restored 0x1016:01: {error}")),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

async fn wait_still(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    node_id: u8,
    epoch: u64,
    generation: &mut u64,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let deadline = Instant::now() + STILL_TIMEOUT;
    let mut still_since = None;
    loop {
        let sample = fresh_sample(shared, manager, node_id, epoch, generation, cancel).await?;
        let velocity = sample
            .measurements
            .velocity_rev_per_s
            .ok_or_else(|| "velocity is unavailable".to_string())?
            * std::f64::consts::TAU;
        if velocity.abs() <= STILL_SPEED_RAD_PER_S {
            let since = still_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= STILL_TIME {
                return Ok(());
            }
        } else {
            still_since = None;
        }
        if Instant::now() >= deadline {
            return Err("shaft did not become stationary within 4 seconds".into());
        }
    }
}

async fn fresh_sample(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    node_id: u8,
    epoch: u64,
    generation: &mut u64,
    cancel: &Arc<AtomicBool>,
) -> Result<hex_motor::meow_motor::MeowMotorLiveState, String> {
    let deadline = Instant::now() + SAMPLE_TIMEOUT;
    loop {
        check_cancel(cancel)?;
        let state = manager.status(node_id).map_err(to_string)?;
        if state.session_epoch != epoch {
            return Err("motor session changed during calibration".into());
        }
        if !state.connection.online {
            return Err("motor went offline during calibration".into());
        }
        if !matches!(state.lifecycle, MeowMotorLifecycle::Initialized) {
            return Err(format!(
                "motor left Initialized during calibration: {:?}",
                state.lifecycle
            ));
        }
        if state.measurements.tpdo1_generation > *generation {
            *generation = state.measurements.tpdo1_generation;
            if !state.measurements.position_accumulation_valid {
                return Err("position accumulation became invalid".into());
            }
            let mut session = shared.lock().await;
            session.view.position_rad = state
                .measurements
                .accumulated_position_rev
                .map(|value| value * std::f64::consts::TAU);
            session.view.velocity_rad_per_s = state
                .measurements
                .velocity_rev_per_s
                .map(|value| value * std::f64::consts::TAU);
            session.view.actual_torque_permille = state.measurements.torque_permille;
            session.view.motor_temperature_c = state.measurements.motor_temp_c.map(f64::from);
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err("no fresh TPDO1 feedback within 300 ms".into());
        }
        tokio::time::sleep(POLL_PERIOD).await;
    }
}

async fn set_phase(shared: &Arc<Mutex<Session>>, phase: &str, progress: u8) {
    let mut session = shared.lock().await;
    session.view.phase = phase.into();
    session.view.progress_percent = progress;
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("calibration cancelled by operator".into())
    } else {
        Ok(())
    }
}

fn trimmed_mean_stddev(values: &[f64]) -> Result<(f64, f64), String> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return Err("sample set is empty or non-finite".into());
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let trim = sorted.len() / 10;
    let kept = &sorted[trim..sorted.len() - trim];
    let average = mean(kept)?;
    let variance = kept
        .iter()
        .map(|value| (value - average).powi(2))
        .sum::<f64>()
        / kept.len() as f64;
    Ok((average, variance.sqrt()))
}

fn mean(values: &[f64]) -> Result<f64, String> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return Err("sample set is empty or non-finite".into());
    }
    Ok(values.iter().sum::<f64>() / values.len() as f64)
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> FrictionCalibrationRequest {
        FrictionCalibrationRequest {
            node_id: 1,
            expected_vendor_id: 1,
            expected_product_code: 2,
            expected_revision_number: 3,
            expected_serial_number: 4,
            torque_step_permille: 1,
            max_torque_permille: 100,
            step_dwell_ms: 1_000,
            movement_position_threshold_rad: 0.01,
            movement_velocity_threshold_rad_per_s: 0.05,
            kinetic_sample_ms: 3_000,
        }
    }

    #[test]
    fn default_profile_is_inside_hard_safety_bounds() {
        request().validate().unwrap();
    }

    #[test]
    fn rejects_unbounded_or_non_finite_motion_inputs() {
        let mut value = request();
        value.max_torque_permille = 201;
        assert!(value.validate().is_err());
        value = request();
        value.movement_position_threshold_rad = f64::NAN;
        assert!(value.validate().is_err());
    }

    #[test]
    fn trimmed_estimator_drops_ten_percent_tails() {
        let mut values = vec![10.0; 18];
        values.insert(0, -1_000.0);
        values.push(1_000.0);
        let (mean, stddev) = trimmed_mean_stddev(&values).unwrap();
        assert_eq!(mean, 10.0);
        assert_eq!(stddev, 0.0);
    }

    /// Explicitly gated hardware qualification for the prepared can0/node-1
    /// bench. It exercises the same state object used by the Tauri commands.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "moves a real unloaded motor on can0"]
    async fn hardware_can0_node1_full_sequence() {
        assert_eq!(
            std::env::var("HEX_MOTOR_FRICTION_HARDWARE").as_deref(),
            Ok("I_UNDERSTAND_THIS_MOVES_THE_MOTOR"),
            "set the explicit hardware-test guard"
        );
        use can_transport::socketcan::SocketCanBus;
        use hex_motor::meow_motor::MeowMotorManagerOptions;

        let bus: Arc<dyn CanBus> = Arc::new(SocketCanBus::open("can0").expect("open can0"));
        let manager = Arc::new(
            MeowMotorManager::new(
                bus.clone(),
                MeowMotorManagerOptions {
                    heartbeat_node_id: 10,
                    broadcast_heartbeat: true,
                    auto_identify: false,
                    sdo_timeout: Duration::from_millis(500),
                    ..Default::default()
                },
            )
            .expect("create Meow Motor manager"),
        );
        // The motor currently produces a 1000 ms heartbeat. Discovery must see
        // one before the explicit identify performed by start().
        tokio::time::sleep(Duration::from_millis(1_200)).await;

        let state = FrictionCalibrationState::default();
        state
            .start(
                manager,
                bus,
                10,
                FrictionCalibrationRequest {
                    expected_vendor_id: 0x0068_6578,
                    expected_product_code: 0x6C64_BC78,
                    expected_revision_number: 0x6578_0001,
                    expected_serial_number: 0x2510_4409,
                    ..request()
                },
            )
            .await
            .expect("start calibration");
        loop {
            let view = state.view().await;
            eprintln!(
                "phase={} progress={} command={} velocity={:?}",
                view.phase,
                view.progress_percent,
                view.current_command_permille,
                view.velocity_rad_per_s
            );
            if !view.running {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&view).expect("serialize final view")
                );
                assert_eq!(view.phase, "completed", "hardware run failed: {view:?}");
                assert!(view.result.is_some());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = state.stop().await;
    }
}
