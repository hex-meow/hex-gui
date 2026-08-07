//! Gravity-fixture torque-factor calibration and no-friction acceptance mode.
//!
//! Measurement uses a bounded 1000 Hz Host trajectory and compressed-MIT Tff
//! controller, sampling raw 0x4577 only in the constant-velocity interior.
//! Acceptance uses a 100 Hz single-motor compressed-MIT RPDO with kp=kd=0.
//! Both operations own their heartbeat only while active and always converge
//! on Disabled.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use can_transport::{CanBus, CanFrame};
use hex_motor::canopen::heartbeat::encode_consumer_heartbeat_entry;
use hex_motor::canopen::sdo;
use hex_motor::meow_motor::{
    host_tpdo_cob_id, CompressedMitMapping, CompressedMitTarget, MeowMotorCanSettingsStatus,
    MeowMotorInitializeOptions, MeowMotorLifecycle, MeowMotorLogic, MeowMotorManager,
    MeowMotorMode, SharedHostPdoConfig, Tpdo1Rate,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::calibration_transport::CalibrationHeartbeat;
use crate::friction_calibration::{safe_cleanup, RuntimeSettings};

const STANDARD_GRAVITY_M_PER_S2: f64 = 9.806_65;
const SWEEP_ENDPOINT_RAD: f64 = 65.0_f64.to_radians();
const FIT_ANGLE_LIMIT_RAD: f64 = 60.0_f64.to_radians();
const MEASUREMENT_OVERSHOOT_RAD: f64 = 72.0_f64.to_radians();
const HARD_ANGLE_RAD: f64 = 80.0_f64.to_radians();
const HARD_SPEED_RAD_PER_S: f64 = 2.0;
const MIT_MEASUREMENT_SPEED_ABORT_RAD_PER_S: f64 = 1.0;
const MIT_INSTANTANEOUS_SPEED_FAULT_RAD_PER_S: f64 = 8.0;
const MIT_VELOCITY_WINDOW_US: u16 = 5_000;
const MIT_VELOCITY_WINDOW_MAX_US: u16 = 10_000;
const SETTLED_SPEED_RAD_PER_S: f64 = 0.03;
const SETTLED_POSITION_RAD: f64 = 3.0_f64.to_radians();
const SETTLED_TIME: Duration = Duration::from_millis(300);
const ZERO_CAPTURE_TIME: Duration = Duration::from_millis(500);
const ZERO_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const SAMPLE_TIMEOUT: Duration = Duration::from_millis(300);
const POLL_PERIOD: Duration = Duration::from_millis(5);
const ACCEPTANCE_PERIOD: Duration = Duration::from_millis(10);
const MIT_CONTROL_PERIOD: Duration = Duration::from_micros(1_000);
const MIT_CONTROL_STALE_TIMEOUT: Duration = Duration::from_millis(30);
const MIT_CONTROL_JITTER_TRIP: Duration = Duration::from_millis(25);
const MIT_TRACKING_ERROR_RAD: f64 = 15.0_f64.to_radians();
const MIT_TRACKING_ERROR_TIME: Duration = Duration::from_millis(150);
const BIN_HALF_WIDTH_RAD: f64 = 2.0_f64.to_radians();
const FIT_GRID_DEG: [f64; 13] = [
    -60.0, -50.0, -40.0, -30.0, -20.0, -10.0, 0.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0,
];

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TorqueCalibrationRequest {
    pub node_id: u8,
    pub expected_vendor_id: u32,
    pub expected_product_code: u32,
    pub expected_revision_number: u32,
    pub expected_serial_number: u32,
    pub mass_kg: f64,
    pub center_distance_m: f64,
    pub sweep_speed_rad_per_s: f64,
    pub sweep_acceleration_rad_per_s2: f64,
    pub sweep_cycles: u8,
    pub controller_kp_nm_per_rad: f64,
    pub controller_kd_nm_s_per_rad: f64,
    pub max_torque_permille: u16,
}

impl TorqueCalibrationRequest {
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
        if !self.mass_kg.is_finite() || !(0.05..=20.0).contains(&self.mass_kg) {
            return Err("fixture mass must be 0.05..=20 kg".into());
        }
        if !self.center_distance_m.is_finite() || !(0.01..=2.0).contains(&self.center_distance_m) {
            return Err("fixture center distance must be 0.01..=2 m".into());
        }
        if !self.sweep_speed_rad_per_s.is_finite()
            || !(0.05..=0.5).contains(&self.sweep_speed_rad_per_s)
        {
            return Err("MIT trajectory speed must be 0.05..=0.5 rad/s".into());
        }
        if !self.sweep_acceleration_rad_per_s2.is_finite()
            || !(0.1..=2.0).contains(&self.sweep_acceleration_rad_per_s2)
        {
            return Err("MIT trajectory acceleration must be 0.1..=2 rad/s^2".into());
        }
        if !(1..=5).contains(&self.sweep_cycles) {
            return Err("MIT trajectory cycles must be 1..=5".into());
        }
        if !self.controller_kp_nm_per_rad.is_finite()
            || !(1.0..=20.0).contains(&self.controller_kp_nm_per_rad)
        {
            return Err("host MIT position gain must be 1..=20 Nm/rad".into());
        }
        if !self.controller_kd_nm_s_per_rad.is_finite()
            || !(0.2..=5.0).contains(&self.controller_kd_nm_s_per_rad)
        {
            return Err("host MIT velocity gain must be 0.2..=5 Nm*s/rad".into());
        }
        let ramp_angle =
            self.sweep_speed_rad_per_s.powi(2) / (2.0 * self.sweep_acceleration_rad_per_s2);
        if ramp_angle > (SWEEP_ENDPOINT_RAD - FIT_ANGLE_LIMIT_RAD - 1.0_f64.to_radians()) {
            return Err(format!(
                "MIT trajectory ramp distance {:.2} degrees leaves no constant-speed samples at 60 degrees; increase acceleration or reduce speed",
                ramp_angle.to_degrees()
            ));
        }
        if self.max_torque_permille < 100 || self.max_torque_permille > 500 {
            return Err("maximum fixture torque must be 100..=500 permille".into());
        }
        Ok(())
    }

    fn expected_identity(self) -> ExpectedIdentity {
        ExpectedIdentity {
            vendor_id: self.expected_vendor_id,
            product_code: self.expected_product_code,
            revision_number: self.expected_revision_number,
            serial_number: self.expected_serial_number,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ExpectedIdentity {
    vendor_id: u32,
    product_code: u32,
    revision_number: u32,
    serial_number: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorqueFitPoint {
    pub angle_deg: f64,
    pub gravity_torque_nm: f64,
    pub forward_raw_nm: f64,
    pub reverse_raw_nm: f64,
    pub midpoint_raw_nm: f64,
    pub fitted_raw_nm: f64,
    pub friction_half_difference_raw_nm: f64,
    pub corrected_residual_nm: f64,
    pub forward_stddev_raw_nm: f64,
    pub reverse_stddev_raw_nm: f64,
    pub forward_samples: u32,
    pub reverse_samples: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorquePassSummary {
    pub cycle: u8,
    pub direction: String,
    pub accepted_samples: u32,
    pub rejected_samples: u32,
    pub mean_velocity_rad_per_s: f64,
    pub velocity_stddev_rad_per_s: f64,
    pub peak_absolute_velocity_rad_per_s: f64,
    pub peak_tracking_error_deg: f64,
    pub minimum_raw_torque_nm: f64,
    pub maximum_raw_torque_nm: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorqueCalibrationResult {
    #[serde(skip_serializing)]
    session_epoch: u64,
    pub node_id: u8,
    pub vendor_id: u32,
    pub product_code: u32,
    pub revision_number: u32,
    pub serial_number: u32,
    pub mass_kg: f64,
    pub center_distance_m: f64,
    pub standard_gravity_m_per_s2: f64,
    pub maximum_gravity_torque_nm: f64,
    pub peak_torque_nm: f64,
    pub torque_factor: f64,
    pub torque_fit_rmse_nm: f64,
    pub positive_torque_factor: f64,
    pub negative_torque_factor: f64,
    pub directional_asymmetry_percent: f64,
    pub mean_hysteresis_half_width_raw_nm: f64,
    pub forward_friction_offset_raw_nm: f64,
    pub reverse_friction_offset_raw_nm: f64,
    pub calibration_temperature_c: f64,
    pub zero_position_raw: i32,
    pub sweep_endpoint_deg: f64,
    pub fit_angle_limit_deg: f64,
    pub sweep_speed_rad_per_s: f64,
    pub sweep_acceleration_rad_per_s2: f64,
    pub sweep_cycles: u8,
    pub control_rate_hz: u16,
    pub controller_kp_nm_per_rad: f64,
    pub controller_kd_nm_s_per_rad: f64,
    pub max_torque_permille: u16,
    pub accepted_sample_count: u32,
    pub rejected_sample_count: u32,
    pub pass_summaries: Vec<TorquePassSummary>,
    pub fit_points: Vec<TorqueFitPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorqueCalibrationView {
    pub running: bool,
    pub acceptance_active: bool,
    pub traffic_active: bool,
    pub phase: String,
    pub progress_percent: u8,
    pub node_id: Option<u8>,
    pub current_command_permille: i16,
    pub current_command_nm: f64,
    pub angle_deg: Option<f64>,
    pub target_angle_deg: Option<f64>,
    pub trajectory_angle_deg: Option<f64>,
    pub trajectory_velocity_rad_per_s: Option<f64>,
    pub tracking_error_deg: Option<f64>,
    pub velocity_rad_per_s: Option<f64>,
    pub acceleration_rad_per_s2: Option<f64>,
    pub actual_torque_permille: Option<i16>,
    pub actual_torque_nm: Option<f64>,
    pub motor_temperature_c: Option<f64>,
    pub current_pass: u8,
    pub total_passes: u8,
    pub accepted_samples: u32,
    pub rejected_samples: u32,
    pub sample_valid: bool,
    pub sample_rejection_reason: Option<String>,
    pub result: Option<TorqueCalibrationResult>,
    pub error: Option<String>,
    pub cleanup_warning: Option<String>,
}

impl Default for TorqueCalibrationView {
    fn default() -> Self {
        Self {
            running: false,
            acceptance_active: false,
            traffic_active: false,
            phase: "idle".into(),
            progress_percent: 0,
            node_id: None,
            current_command_permille: 0,
            current_command_nm: 0.0,
            angle_deg: None,
            target_angle_deg: None,
            trajectory_angle_deg: None,
            trajectory_velocity_rad_per_s: None,
            tracking_error_deg: None,
            velocity_rad_per_s: None,
            acceleration_rad_per_s2: None,
            actual_torque_permille: None,
            actual_torque_nm: None,
            motor_temperature_c: None,
            current_pass: 0,
            total_passes: 0,
            accepted_samples: 0,
            rejected_samples: 0,
            sample_valid: false,
            sample_rejection_reason: None,
            result: None,
            error: None,
            cleanup_warning: None,
        }
    }
}

#[derive(Default)]
struct Session {
    view: TorqueCalibrationView,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Clone, Default)]
pub struct TorqueCalibrationState {
    inner: Arc<Mutex<Session>>,
}

impl TorqueCalibrationState {
    pub async fn view(&self) -> TorqueCalibrationView {
        self.inner.lock().await.view.clone()
    }

    pub async fn start_measurement(
        &self,
        manager: Arc<MeowMotorManager>,
        bus: Arc<dyn CanBus>,
        host_node_id: u8,
        request: TorqueCalibrationRequest,
    ) -> Result<TorqueCalibrationView, String> {
        request.validate()?;
        self.reap_or_reject().await?;

        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut session = self.inner.lock().await;
            session.view = TorqueCalibrationView {
                running: true,
                traffic_active: true,
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
                    let outcome = measure_and_cleanup(
                        &shared,
                        &manager,
                        &bus,
                        host_node_id,
                        request,
                        &cancel,
                    )
                    .await;
                    heartbeat.stop().await;
                    outcome
                }
                Err(error) => Err(error),
            };
            let mut session = shared.lock().await;
            session.view.running = false;
            session.view.traffic_active = false;
            session.view.current_command_permille = 0;
            session.view.current_command_nm = 0.0;
            match outcome {
                Ok(result) => {
                    session.view.phase = "measured".into();
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

    pub async fn start_acceptance(
        &self,
        manager: Arc<MeowMotorManager>,
        bus: Arc<dyn CanBus>,
        host_node_id: u8,
    ) -> Result<TorqueCalibrationView, String> {
        self.reap_or_reject().await?;
        let result = self
            .inner
            .lock()
            .await
            .view
            .result
            .clone()
            .ok_or_else(|| "run and review a torque measurement first".to_string())?;
        if !result.torque_factor.is_finite() || !(0.5..=1.5).contains(&result.torque_factor) {
            return Err(format!(
                "torque factor {:.6} is outside the 0.5..=1.5 acceptance safety envelope",
                result.torque_factor
            ));
        }

        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut session = self.inner.lock().await;
            session.view.running = true;
            session.view.acceptance_active = true;
            session.view.traffic_active = true;
            session.view.phase = "acceptance".into();
            session.view.error = None;
            session.view.cleanup_warning = None;
            session.cancel = Some(cancel.clone());
        }

        let shared = self.inner.clone();
        let task = tokio::spawn(async move {
            let outcome = match CalibrationHeartbeat::start(bus.clone(), host_node_id) {
                Ok(heartbeat) => {
                    let outcome = acceptance_and_cleanup(
                        &shared,
                        &manager,
                        &bus,
                        host_node_id,
                        &result,
                        &cancel,
                    )
                    .await;
                    heartbeat.stop().await;
                    outcome
                }
                Err(error) => Err(error),
            };
            let mut session = shared.lock().await;
            session.view.running = false;
            session.view.acceptance_active = false;
            session.view.traffic_active = false;
            session.view.current_command_permille = 0;
            session.view.current_command_nm = 0.0;
            match outcome {
                Ok(()) => {
                    session.view.phase = "measured".into();
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

    pub async fn stop(&self) -> TorqueCalibrationView {
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

    /// A fitted zero position is valid only for the connection/session that
    /// measured it. Disconnect must discard it so acceptance cannot reuse a
    /// stale encoder origin after a motor power cycle or manager recreation.
    pub async fn reset(&self) {
        self.stop().await;
        *self.inner.lock().await = Session::default();
    }

    async fn reap_or_reject(&self) -> Result<(), String> {
        let previous = {
            let mut session = self.inner.lock().await;
            if session.view.running || session.view.acceptance_active {
                return Err("a torque calibration operation is already running".into());
            }
            session.task.take()
        };
        if let Some(previous) = previous {
            let _ = previous.await;
        }
        Ok(())
    }
}

fn measurement_mit_mapping(request: TorqueCalibrationRequest) -> CompressedMitMapping {
    let maximum_gravity_torque_nm =
        request.mass_kg * STANDARD_GRAVITY_M_PER_S2 * request.center_distance_m;
    mit_mapping_for_gravity(maximum_gravity_torque_nm)
}

fn mit_mapping_for_gravity(maximum_gravity_torque_nm: f64) -> CompressedMitMapping {
    let torque_range_nm = (maximum_gravity_torque_nm * 2.0 + 0.5).max(5.0) as f32;
    CompressedMitMapping {
        // Measurement keeps position/Kp at zero so compressed MIT never depends
        // on the motor's absolute multi-turn position. The host supplies the
        // gravity + position correction through Tff, while the motor closes the
        // velocity loop through its own filtered velocity and compressed-MIT Kd.
        // The asymmetric positive endpoints put mathematical zero exactly on
        // an integer compressed code (32767 for position, 2047 for 12-bit
        // velocity/Tff), so a zero frame does not decode as a small torque.
        position_min: -0.01,
        position_max: 0.01 * (32_768.0 / 32_767.0),
        // Binary-exact endpoints: code 2047 decodes to exactly zero while the
        // range still covers the maximum allowed 0.5 rad/s trajectory.
        velocity_min: -2_047.0 / 16_384.0,
        velocity_max: 2_048.0 / 16_384.0,
        torque_min: -torque_range_nm,
        torque_max: torque_range_nm * (2_048.0 / 2_047.0),
        kp_min: 0.0,
        kp_max: 1.0,
        kd_min: 0.0,
        // Hardware Kd is Nm*s/Rev, so the largest allowed GUI value of
        // 5 Nm*s/rad needs 10*pi ~= 31.42 Nm*s/Rev.
        kd_max: 40.0,
    }
}

async fn enter_compressed_mit(
    bus: &dyn CanBus,
    manager: &MeowMotorManager,
    node_id: u8,
    cancel: &AtomicBool,
) -> Result<(), String> {
    sdo::download(
        bus,
        node_id,
        0x4401,
        0,
        &[MeowMotorMode::Mit.command_code()],
        Some(Duration::from_millis(500)),
    )
    .await
    .map_err(to_string)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        check_cancel(cancel)?;
        let live = manager.status(node_id).map_err(to_string)?;
        match live.logic {
            Some(MeowMotorLogic::Enabled(MeowMotorMode::Mit)) => return Ok(()),
            Some(MeowMotorLogic::Error {
                mode_display,
                detailed_error,
            }) => {
                return Err(format!(
                    "motor rejected compressed MIT: mode=0x{mode_display:02X}, detail=0x{detailed_error:04X}"
                ));
            }
            _ if Instant::now() >= deadline => {
                return Err(format!(
                    "compressed MIT mode confirmation timed out; observed {:?}",
                    live.logic
                ));
            }
            _ => tokio::time::sleep(Duration::from_millis(2)).await,
        }
    }
}

async fn send_compressed_zero(
    bus: &dyn CanBus,
    host_node_id: u8,
    mapping: CompressedMitMapping,
    brs: bool,
) -> Result<(), String> {
    let mut last_error = None;
    for _ in 0..3 {
        if let Err(error) = send_mit_target(bus, host_node_id, mapping, brs, 0.0).await {
            last_error = Some(error);
        }
        tokio::time::sleep(MIT_CONTROL_PERIOD).await;
    }
    last_error.map_or(Ok(()), Err)
}

fn merge_cleanup_errors(
    first: Result<(), String>,
    second: Result<(), String>,
) -> Result<(), String> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(format!("{first}; {second}")),
    }
}

async fn measure_and_cleanup(
    shared: &Arc<Mutex<Session>>,
    manager: &Arc<MeowMotorManager>,
    bus: &Arc<dyn CanBus>,
    host_node_id: u8,
    request: TorqueCalibrationRequest,
    cancel: &AtomicBool,
) -> Result<TorqueCalibrationResult, String> {
    check_cancel(cancel)?;
    manager.identify(request.node_id).await.map_err(to_string)?;
    let identified = verify_identity(manager, request.node_id, request.expected_identity())?;
    let brs = match identified.can_settings {
        MeowMotorCanSettingsStatus::Available(settings) => settings.transmit_pdo_brs,
        _ => return Err("motor CAN settings are unavailable for MIT control".into()),
    };
    let mapping = measurement_mit_mapping(request);
    let original = RuntimeSettings::read(bus.as_ref(), request.node_id).await?;
    let result = run_measurement(
        shared,
        manager,
        bus,
        host_node_id,
        mapping,
        brs,
        request,
        cancel,
    )
    .await;
    set_phase(shared, "cleanup", 97).await;
    let zero_cleanup = send_compressed_zero(bus.as_ref(), host_node_id, mapping, brs).await;
    let cleanup = safe_cleanup(manager, bus.as_ref(), request.node_id, original).await;
    let cleanup = merge_cleanup_errors(zero_cleanup, cleanup);
    merge_cleanup(shared, result, cleanup).await
}

async fn run_measurement(
    shared: &Arc<Mutex<Session>>,
    manager: &Arc<MeowMotorManager>,
    bus: &Arc<dyn CanBus>,
    host_node_id: u8,
    mapping: CompressedMitMapping,
    brs: bool,
    request: TorqueCalibrationRequest,
    cancel: &AtomicBool,
) -> Result<TorqueCalibrationResult, String> {
    manager
        .initialize_with_options(
            request.node_id,
            MeowMotorInitializeOptions {
                tpdo1_rate: Tpdo1Rate::Hz1000,
                configure_consumer_heartbeat: true,
                shared_host_pdo: Some(SharedHostPdoConfig {
                    host_node_id,
                    motor_slot: 0,
                    total_motors: 1,
                    compressed_mit_mapping: mapping,
                }),
            },
        )
        .await
        .map_err(to_string)?;
    let info = verify_identity(manager, request.node_id, request.expected_identity())?;
    let peak_torque_nm = f64::from(
        info.peak_torque_nm
            .filter(|value| value.is_finite() && *value > 0.0)
            .ok_or_else(|| "0x4576 peak torque is missing or invalid".to_string())?,
    );
    let epoch = info.session_epoch;
    let maximum_gravity_torque_nm =
        request.mass_kg * STANDARD_GRAVITY_M_PER_S2 * request.center_distance_m;
    let available_torque_nm = peak_torque_nm * f64::from(request.max_torque_permille) / 1000.0;
    let required_margin_nm = (maximum_gravity_torque_nm * 0.15).max(0.5);
    if maximum_gravity_torque_nm + required_margin_nm > available_torque_nm {
        return Err(format!(
            "fixture needs {:.3} Nm gravity plus {:.3} Nm control margin, but the configured ceiling provides only {:.3} Nm",
            maximum_gravity_torque_nm, required_margin_nm, available_torque_nm
        ));
    }
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

    set_phase(shared, "capturing_bottom", 5).await;
    let zero_position_raw = capture_bottom_zero(
        shared,
        manager,
        request.node_id,
        epoch,
        &mut generation,
        cancel,
    )
    .await?;

    let total_passes = request.sweep_cycles.saturating_mul(2);
    {
        let mut session = shared.lock().await;
        session.view.total_passes = total_passes;
        session.view.target_angle_deg = Some(-SWEEP_ENDPOINT_RAD.to_degrees());
    }

    set_phase(shared, "positioning_start", 8).await;
    let mut samples = Vec::new();
    let mut pass_summaries = Vec::new();
    let mut controller = MitTrajectoryController::start(
        manager.clone(),
        bus.clone(),
        host_node_id,
        mapping,
        brs,
        request,
        epoch,
        zero_position_raw,
        peak_torque_nm,
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    enter_compressed_mit(bus.as_ref(), manager, request.node_id, cancel).await?;
    controller.set_target(-SWEEP_ENDPOINT_RAD)?;
    execute_mit_move(
        shared,
        manager,
        request,
        epoch,
        zero_position_raw,
        &mut generation,
        cancel,
        peak_torque_nm,
        -SWEEP_ENDPOINT_RAD,
        None,
        0,
        total_passes,
        &mut samples,
        &controller,
    )
    .await?;

    for cycle in 1..=request.sweep_cycles {
        check_cancel(cancel)?;
        let forward_pass = cycle.saturating_mul(2).saturating_sub(1);
        set_phase(shared, "sweep_forward", 10).await;
        controller.set_target(SWEEP_ENDPOINT_RAD)?;
        pass_summaries.push(
            execute_mit_move(
                shared,
                manager,
                request,
                epoch,
                zero_position_raw,
                &mut generation,
                cancel,
                peak_torque_nm,
                SWEEP_ENDPOINT_RAD,
                Some(SweepDirection::Forward),
                forward_pass,
                total_passes,
                &mut samples,
                &controller,
            )
            .await?
            .ok_or_else(|| "forward MIT traversal produced no summary".to_string())?,
        );

        let reverse_pass = cycle.saturating_mul(2);
        set_phase(shared, "sweep_reverse", 10).await;
        controller.set_target(-SWEEP_ENDPOINT_RAD)?;
        pass_summaries.push(
            execute_mit_move(
                shared,
                manager,
                request,
                epoch,
                zero_position_raw,
                &mut generation,
                cancel,
                peak_torque_nm,
                -SWEEP_ENDPOINT_RAD,
                Some(SweepDirection::Reverse),
                reverse_pass,
                total_passes,
                &mut samples,
                &controller,
            )
            .await?
            .ok_or_else(|| "reverse MIT traversal produced no summary".to_string())?,
        );
    }

    set_phase(shared, "returning_bottom", 91).await;
    shared.lock().await.view.target_angle_deg = Some(0.0);
    controller.set_target(0.0)?;
    execute_mit_move(
        shared,
        manager,
        request,
        epoch,
        zero_position_raw,
        &mut generation,
        cancel,
        peak_torque_nm,
        0.0,
        None,
        total_passes,
        total_passes,
        &mut samples,
        &controller,
    )
    .await?;
    controller.stop().await?;
    manager.disable(request.node_id).await.map_err(to_string)?;
    verify_identity(manager, request.node_id, request.expected_identity())?;

    set_phase(shared, "fitting", 96).await;
    fit_result(
        request,
        peak_torque_nm,
        epoch,
        zero_position_raw,
        samples,
        pass_summaries,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepDirection {
    Forward,
    Reverse,
}

impl SweepDirection {
    const fn velocity_sign(self) -> f64 {
        match self {
            Self::Forward => 1.0,
            Self::Reverse => -1.0,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Reverse => "reverse",
        }
    }
}

#[derive(Debug, Clone)]
struct RawSweepSample {
    direction: SweepDirection,
    angle_rad: f64,
    velocity_rad_per_s: f64,
    raw_torque_nm: f64,
    temperature_c: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct MitControllerSnapshot {
    desired_angle_rad: f64,
    desired_velocity_rad_per_s: f64,
    tracking_error_rad: f64,
    observed_velocity_rad_per_s: f64,
    commanded_torque_nm: f64,
}

#[derive(Debug, Default)]
struct WindowedVelocity {
    samples: VecDeque<(i32, u16)>,
}

impl WindowedVelocity {
    fn observe(&mut self, position_raw: i32, timestamp_us: u16) -> Option<f64> {
        if self
            .samples
            .back()
            .is_some_and(|(_, previous_timestamp)| *previous_timestamp == timestamp_us)
        {
            return None;
        }
        self.samples.push_back((position_raw, timestamp_us));

        while self.samples.len() >= 2 {
            let next_timestamp = self.samples.get(1).unwrap().1;
            if timestamp_us.wrapping_sub(next_timestamp) >= MIT_VELOCITY_WINDOW_US {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        let (previous_raw, previous_timestamp) = *self.samples.front()?;
        let dt_us = timestamp_us.wrapping_sub(previous_timestamp);
        if dt_us < MIT_VELOCITY_WINDOW_US || dt_us > MIT_VELOCITY_WINDOW_MAX_US {
            if dt_us > MIT_VELOCITY_WINDOW_MAX_US {
                self.samples.clear();
                self.samples.push_back((position_raw, timestamp_us));
            }
            return None;
        }
        let delta_rev = position_raw.wrapping_sub(previous_raw) as f64 / 16_777_216.0;
        Some(delta_rev * std::f64::consts::TAU * 1_000_000.0 / f64::from(dt_us))
    }
}

#[derive(Debug, Default)]
struct MitControllerState {
    target_angle_rad: f64,
    snapshot: MitControllerSnapshot,
    error: Option<String>,
}

struct MitTrajectoryController {
    state: Arc<StdMutex<MitControllerState>>,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<Result<(), String>>>,
}

impl MitTrajectoryController {
    #[allow(clippy::too_many_arguments)]
    fn start(
        manager: Arc<MeowMotorManager>,
        bus: Arc<dyn CanBus>,
        host_node_id: u8,
        mapping: CompressedMitMapping,
        brs: bool,
        request: TorqueCalibrationRequest,
        epoch: u64,
        zero_position_raw: i32,
        peak_torque_nm: f64,
    ) -> Self {
        let state = Arc::new(StdMutex::new(MitControllerState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let task_state = state.clone();
        let task_stop = stop.clone();
        let task = tokio::spawn(async move {
            run_mit_controller(
                manager,
                bus,
                host_node_id,
                mapping,
                brs,
                request,
                epoch,
                zero_position_raw,
                peak_torque_nm,
                task_state,
                task_stop,
            )
            .await
        });
        Self {
            state,
            stop,
            task: Some(task),
        }
    }

    fn set_target(&self, target_angle_rad: f64) -> Result<(), String> {
        if !target_angle_rad.is_finite() || target_angle_rad.abs() > SWEEP_ENDPOINT_RAD {
            return Err(format!(
                "MIT trajectory target must be within +/-65 degrees, got {:.2}",
                target_angle_rad.to_degrees()
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "MIT controller state lock is poisoned".to_string())?;
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        state.target_angle_rad = target_angle_rad;
        Ok(())
    }

    fn snapshot(&self) -> Result<MitControllerSnapshot, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "MIT controller state lock is poisoned".to_string())?;
        if let Some(error) = &state.error {
            Err(error.clone())
        } else {
            Ok(state.snapshot)
        }
    }

    async fn stop(&mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await
            .map_err(|error| format!("MIT controller task failed: {error}"))?
    }
}

impl Drop for MitTrajectoryController {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_mit_controller(
    manager: Arc<MeowMotorManager>,
    bus: Arc<dyn CanBus>,
    host_node_id: u8,
    mapping: CompressedMitMapping,
    brs: bool,
    request: TorqueCalibrationRequest,
    epoch: u64,
    zero_position_raw: i32,
    peak_torque_nm: f64,
    state: Arc<StdMutex<MitControllerState>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut ticker = tokio::time::interval(MIT_CONTROL_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let maximum_gravity_torque_nm =
        request.mass_kg * STANDARD_GRAVITY_M_PER_S2 * request.center_distance_m;
    let torque_limit_nm = peak_torque_nm * f64::from(request.max_torque_permille) / 1000.0;
    let mut desired_angle_rad = 0.0_f64;
    let mut desired_velocity_rad_per_s = 0.0_f64;
    let mut last_tick = Instant::now();
    let mut feedback_generation = 0_u64;
    let mut last_feedback = Instant::now();
    let mut tracking_limit_since = None;
    let mut torque_limit_since = None;
    let mut velocity_window = WindowedVelocity::default();
    let mut observed_velocity_rad_per_s = 0.0_f64;
    let mut instantaneous_velocity_rad_per_s = 0.0_f64;
    let mut failure = None;

    while !stop.load(Ordering::Acquire) {
        ticker.tick().await;
        if stop.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(last_tick);
        last_tick = now;
        if elapsed > MIT_CONTROL_JITTER_TRIP {
            failure = Some(format!(
                "1000 Hz MIT controller was not scheduled for {:.1} ms",
                elapsed.as_secs_f64() * 1000.0
            ));
            break;
        }
        let dt = elapsed
            .as_secs_f64()
            .clamp(MIT_CONTROL_PERIOD.as_secs_f64() * 0.25, 0.005);
        let live = match manager.status(request.node_id) {
            Ok(live) => live,
            Err(error) => {
                failure = Some(error.to_string());
                break;
            }
        };
        if live.session_epoch != epoch || !live.connection.online {
            failure = Some("motor session or online state changed in the MIT controller".into());
            break;
        }
        if !matches!(live.lifecycle, MeowMotorLifecycle::Initialized) {
            failure = Some(format!(
                "motor left Initialized in the MIT controller: {:?}",
                live.lifecycle
            ));
            break;
        }
        let fresh_feedback = live.measurements.tpdo1_generation > feedback_generation;
        if fresh_feedback {
            feedback_generation = live.measurements.tpdo1_generation;
            last_feedback = now;
        } else if last_feedback.elapsed() >= MIT_CONTROL_STALE_TIMEOUT {
            failure = Some("1000 Hz MIT controller had no fresh TPDO1 for 30 ms".into());
            break;
        }
        if !live.measurements.position_accumulation_valid {
            failure = Some("position accumulation became invalid in the MIT controller".into());
            break;
        }
        let Some(position) = live.measurements.position else {
            failure = Some("position is unavailable in the MIT controller".into());
            break;
        };
        let angle_rad = angle_from_zero(position.raw(), zero_position_raw);
        if fresh_feedback {
            let Some(timestamp_us) = live.measurements.timestamp_us else {
                failure = Some("TPDO1 timestamp is unavailable in the MIT controller".into());
                break;
            };
            let Some(velocity_rev_per_s) = live.measurements.velocity_rev_per_s else {
                failure =
                    Some("instantaneous velocity is unavailable in the MIT controller".into());
                break;
            };
            instantaneous_velocity_rad_per_s = velocity_rev_per_s * std::f64::consts::TAU;
            if instantaneous_velocity_rad_per_s.abs() >= MIT_INSTANTANEOUS_SPEED_FAULT_RAD_PER_S {
                failure = Some(format!(
                    "8 rad/s instantaneous velocity fault tripped at {:.3} rad/s",
                    instantaneous_velocity_rad_per_s
                ));
                break;
            }
            if let Some(windowed) = velocity_window.observe(position.raw(), timestamp_us) {
                observed_velocity_rad_per_s = windowed;
            }
        }
        if angle_rad.abs() >= MEASUREMENT_OVERSHOOT_RAD {
            failure = Some(format!(
                "72 degree MIT controller guard tripped at {:.2} degrees",
                angle_rad.to_degrees()
            ));
            break;
        }
        if observed_velocity_rad_per_s.abs() >= HARD_SPEED_RAD_PER_S {
            failure = Some(format!(
                "2 rad/s MIT 5 ms hard speed protection tripped at {:.3} rad/s (instantaneous {:.3} rad/s)",
                observed_velocity_rad_per_s, instantaneous_velocity_rad_per_s,
            ));
            break;
        }
        if observed_velocity_rad_per_s.abs() >= MIT_MEASUREMENT_SPEED_ABORT_RAD_PER_S {
            failure = Some(format!(
                "1 rad/s MIT 5 ms velocity guard tripped at {:.3} rad/s before the 2 rad/s hard limit (instantaneous {:.3} rad/s)",
                observed_velocity_rad_per_s,
                instantaneous_velocity_rad_per_s,
            ));
            break;
        }

        let target_angle_rad = match state.lock() {
            Ok(state) => state.target_angle_rad,
            Err(_) => {
                failure = Some("MIT controller state lock is poisoned".into());
                break;
            }
        };
        (desired_angle_rad, desired_velocity_rad_per_s) = advance_limited_trajectory(
            desired_angle_rad,
            desired_velocity_rad_per_s,
            target_angle_rad,
            request.sweep_speed_rad_per_s,
            request.sweep_acceleration_rad_per_s2,
            dt,
        );

        let tracking_error_rad = desired_angle_rad - angle_rad;
        if tracking_error_rad.abs() >= MIT_TRACKING_ERROR_RAD {
            let since = tracking_limit_since.get_or_insert(now);
            if since.elapsed() >= MIT_TRACKING_ERROR_TIME {
                failure = Some(format!(
                    "MIT trajectory tracking error stayed above 15 degrees: {:.2} degrees",
                    tracking_error_rad.to_degrees()
                ));
                break;
            }
        } else {
            tracking_limit_since = None;
        }
        let gravity_feedforward_nm = maximum_gravity_torque_nm * angle_rad.sin();
        let correction_nm = request.controller_kp_nm_per_rad * tracking_error_rad;
        let unclamped_torque_nm = gravity_feedforward_nm + correction_nm;
        let commanded_torque_nm = unclamped_torque_nm.clamp(-torque_limit_nm, torque_limit_nm);
        if unclamped_torque_nm.abs() > torque_limit_nm {
            let since = torque_limit_since.get_or_insert(now);
            if since.elapsed() >= Duration::from_millis(100) {
                failure = Some(format!(
                    "MIT Tff command stayed saturated for 100 ms: requested {:.3} Nm, limit {:.3} Nm",
                    unclamped_torque_nm, torque_limit_nm
                ));
                break;
            }
        } else {
            torque_limit_since = None;
        }

        if let Err(error) = send_measurement_mit_target(
            bus.as_ref(),
            host_node_id,
            mapping,
            brs,
            commanded_torque_nm,
            desired_velocity_rad_per_s,
            request.controller_kd_nm_s_per_rad,
        )
        .await
        {
            failure = Some(error);
            break;
        }
        match state.lock() {
            Ok(mut state) => {
                state.snapshot = MitControllerSnapshot {
                    desired_angle_rad,
                    desired_velocity_rad_per_s,
                    tracking_error_rad,
                    observed_velocity_rad_per_s,
                    commanded_torque_nm,
                };
            }
            Err(_) => {
                failure = Some("MIT controller state lock is poisoned".into());
                break;
            }
        }
    }

    let _ = send_compressed_zero(bus.as_ref(), host_node_id, mapping, brs).await;
    if let Some(error) = failure {
        if let Ok(mut state) = state.lock() {
            state.error = Some(error.clone());
        }
        Err(error)
    } else {
        Ok(())
    }
}

fn advance_limited_trajectory(
    angle_rad: f64,
    velocity_rad_per_s: f64,
    target_angle_rad: f64,
    maximum_speed_rad_per_s: f64,
    acceleration_rad_per_s2: f64,
    dt: f64,
) -> (f64, f64) {
    let remaining = target_angle_rad - angle_rad;
    let stopping_velocity = (2.0 * acceleration_rad_per_s2 * remaining.abs()).sqrt();
    let goal_velocity = remaining.signum() * maximum_speed_rad_per_s.min(stopping_velocity);
    let maximum_velocity_change = acceleration_rad_per_s2 * dt;
    let velocity_rad_per_s = velocity_rad_per_s
        + (goal_velocity - velocity_rad_per_s)
            .clamp(-maximum_velocity_change, maximum_velocity_change);
    let next_angle_rad = angle_rad + velocity_rad_per_s * dt;
    if (target_angle_rad - next_angle_rad).abs() <= 0.5 * acceleration_rad_per_s2 * dt.powi(2)
        && velocity_rad_per_s.abs() <= maximum_velocity_change
    {
        (target_angle_rad, 0.0)
    } else {
        (next_angle_rad, velocity_rad_per_s)
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_mit_move(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    request: TorqueCalibrationRequest,
    epoch: u64,
    zero_position_raw: i32,
    generation: &mut u64,
    cancel: &AtomicBool,
    peak_torque_nm: f64,
    target_angle_rad: f64,
    direction: Option<SweepDirection>,
    pass_index: u8,
    total_passes: u8,
    samples: &mut Vec<RawSweepSample>,
    controller: &MitTrajectoryController,
) -> Result<Option<TorquePassSummary>, String> {
    let initial = fresh_sample(
        shared,
        manager,
        request.node_id,
        epoch,
        zero_position_raw,
        generation,
        cancel,
        false,
    )
    .await?;
    let initial_angle = angle_from_zero(initial.position_raw, zero_position_raw);
    let expected_seconds =
        (target_angle_rad - initial_angle).abs() / request.sweep_speed_rad_per_s * 1.5 + 10.0;
    let deadline = Instant::now() + Duration::from_secs_f64(expected_seconds.max(10.0));
    let mut stable_since = None;
    let sample_start = samples.len();
    let mut accepted = 0_u32;
    let mut rejected = 0_u32;
    let mut previous: Option<(Instant, f64)> = None;
    let mut filtered_acceleration = 0.0;
    let mut torque_limit_since = None;
    let mut peak_absolute_velocity_rad_per_s = 0.0_f64;
    let mut peak_tracking_error_rad = 0.0_f64;
    {
        let mut session = shared.lock().await;
        session.view.current_pass = pass_index;
        session.view.total_passes = total_passes;
        session.view.target_angle_deg = Some(target_angle_rad.to_degrees());
    }

    loop {
        let control = controller.snapshot()?;
        let sample = fresh_sample(
            shared,
            manager,
            request.node_id,
            epoch,
            zero_position_raw,
            generation,
            cancel,
            false,
        )
        .await?;
        let angle = angle_from_zero(sample.position_raw, zero_position_raw);
        let velocity_rad_per_s = control.observed_velocity_rad_per_s;
        peak_absolute_velocity_rad_per_s =
            peak_absolute_velocity_rad_per_s.max(velocity_rad_per_s.abs());
        peak_tracking_error_rad = peak_tracking_error_rad.max(control.tracking_error_rad.abs());
        if angle.abs() >= MEASUREMENT_OVERSHOOT_RAD {
            return Err(format!(
                "72 degree MIT trajectory overshoot guard tripped at {:.2} degrees",
                angle.to_degrees()
            ));
        }
        if sample.actual_torque_permille.unsigned_abs()
            >= request.max_torque_permille.saturating_sub(2)
        {
            let since = torque_limit_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(100) {
                return Err(format!(
                    "MIT controller remained at the {} permille torque ceiling for 100 ms",
                    request.max_torque_permille
                ));
            }
        } else {
            torque_limit_since = None;
        }

        if let Some((last_at, last_velocity)) = previous {
            let dt = sample
                .observed_at
                .saturating_duration_since(last_at)
                .as_secs_f64();
            if dt >= 0.001 {
                let instantaneous = (velocity_rad_per_s - last_velocity) / dt;
                if instantaneous.is_finite() {
                    filtered_acceleration = 0.85 * filtered_acceleration + 0.15 * instantaneous;
                }
            }
        }
        previous = Some((sample.observed_at, velocity_rad_per_s));

        let mut sample_valid = false;
        let mut rejection_reason = None;
        if let Some(direction) = direction {
            if angle.abs() <= FIT_ANGLE_LIMIT_RAD {
                let expected_velocity = direction.velocity_sign() * request.sweep_speed_rad_per_s;
                let velocity_tolerance = (request.sweep_speed_rad_per_s * 0.20).max(0.03);
                // TPDO velocity is quantized, so a short-interval derivative is
                // intentionally only a coarse guard. The velocity window and
                // five-degree ramp margin are the primary steady-motion selectors.
                let acceleration_limit = (request.sweep_acceleration_rad_per_s2 * 0.75).max(0.5);
                if (velocity_rad_per_s - expected_velocity).abs() > velocity_tolerance {
                    rejection_reason = Some("velocity outside constant-speed window".to_string());
                } else if filtered_acceleration.abs() > acceleration_limit {
                    rejection_reason =
                        Some("acceleration outside steady-motion window".to_string());
                } else if sample.actual_torque_permille.unsigned_abs()
                    >= request.max_torque_permille.saturating_sub(5)
                {
                    rejection_reason = Some("torque too close to ceiling".to_string());
                } else {
                    sample_valid = true;
                    accepted = accepted.saturating_add(1);
                    samples.push(RawSweepSample {
                        direction,
                        angle_rad: angle,
                        velocity_rad_per_s,
                        raw_torque_nm: f64::from(sample.actual_torque_permille) * peak_torque_nm
                            / 1000.0,
                        temperature_c: sample.temperature_c,
                    });
                }
                if !sample_valid {
                    rejected = rejected.saturating_add(1);
                }
            } else {
                rejection_reason = Some("outside ±60 degree fit window".to_string());
            }
        }

        {
            let mut session = shared.lock().await;
            session.view.trajectory_angle_deg = Some(control.desired_angle_rad.to_degrees());
            session.view.trajectory_velocity_rad_per_s = Some(control.desired_velocity_rad_per_s);
            session.view.tracking_error_deg = Some(control.tracking_error_rad.to_degrees());
            session.view.velocity_rad_per_s = Some(velocity_rad_per_s);
            session.view.acceleration_rad_per_s2 = Some(filtered_acceleration);
            session.view.actual_torque_nm =
                Some(f64::from(sample.actual_torque_permille) * peak_torque_nm / 1000.0);
            session.view.current_command_nm = control.commanded_torque_nm;
            session.view.current_command_permille =
                (control.commanded_torque_nm / peak_torque_nm * 1000.0).round() as i16;
            session.view.sample_valid = sample_valid;
            session.view.sample_rejection_reason = rejection_reason;
            session.view.accepted_samples = session
                .view
                .accepted_samples
                .saturating_add(u32::from(sample_valid));
            if direction.is_some() && !sample_valid && angle.abs() <= FIT_ANGLE_LIMIT_RAD {
                session.view.rejected_samples = session.view.rejected_samples.saturating_add(1);
            }
            if direction.is_some() && total_passes > 0 {
                let traversal_fraction = match direction.unwrap() {
                    SweepDirection::Forward => {
                        ((angle + SWEEP_ENDPOINT_RAD) / (2.0 * SWEEP_ENDPOINT_RAD)).clamp(0.0, 1.0)
                    }
                    SweepDirection::Reverse => {
                        ((SWEEP_ENDPOINT_RAD - angle) / (2.0 * SWEEP_ENDPOINT_RAD)).clamp(0.0, 1.0)
                    }
                };
                let completed = f64::from(pass_index.saturating_sub(1));
                session.view.progress_percent = (10.0
                    + 80.0 * (completed + traversal_fraction) / f64::from(total_passes))
                .round() as u8;
            }
        }

        if (angle - target_angle_rad).abs() <= SETTLED_POSITION_RAD
            && velocity_rad_per_s.abs() <= SETTLED_SPEED_RAD_PER_S
        {
            let since = stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= SETTLED_TIME {
                break;
            }
        } else {
            stable_since = None;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "MIT trajectory did not settle at {:.1} degrees within {:.1} seconds",
                target_angle_rad.to_degrees(),
                expected_seconds.max(10.0)
            ));
        }
    }

    let Some(direction) = direction else {
        return Ok(None);
    };
    let pass_samples = &samples[sample_start..];
    if pass_samples.is_empty() {
        return Err(format!(
            "{} MIT pass produced no accepted constant-speed samples",
            direction.name()
        ));
    }
    let velocities: Vec<f64> = pass_samples
        .iter()
        .map(|sample| sample.velocity_rad_per_s)
        .collect();
    let torques: Vec<f64> = pass_samples
        .iter()
        .map(|sample| sample.raw_torque_nm)
        .collect();
    let (mean_velocity, velocity_stddev) = mean_stddev(&velocities)?;
    Ok(Some(TorquePassSummary {
        cycle: (pass_index + 1) / 2,
        direction: direction.name().into(),
        accepted_samples: accepted,
        rejected_samples: rejected,
        mean_velocity_rad_per_s: mean_velocity,
        velocity_stddev_rad_per_s: velocity_stddev,
        peak_absolute_velocity_rad_per_s,
        peak_tracking_error_deg: peak_tracking_error_rad.to_degrees(),
        minimum_raw_torque_nm: torques.iter().copied().fold(f64::INFINITY, f64::min),
        maximum_raw_torque_nm: torques.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    }))
}

async fn capture_bottom_zero(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    node_id: u8,
    epoch: u64,
    generation: &mut u64,
    cancel: &AtomicBool,
) -> Result<i32, String> {
    let deadline = Instant::now() + ZERO_CAPTURE_TIMEOUT;
    let mut stable_since = None;
    let mut anchor = None;
    let mut deltas = Vec::new();
    loop {
        let sample =
            fresh_sample_without_zero(shared, manager, node_id, epoch, generation, cancel).await?;
        if sample.velocity_rad_per_s.abs() <= SETTLED_SPEED_RAD_PER_S {
            let since = stable_since.get_or_insert_with(Instant::now);
            let base = *anchor.get_or_insert(sample.position_raw);
            deltas.push(sample.position_raw.wrapping_sub(base) as f64);
            if since.elapsed() >= ZERO_CAPTURE_TIME && deltas.len() >= 30 {
                return Ok(base.wrapping_add(mean(&deltas)?.round() as i32));
            }
        } else {
            stable_since = None;
            anchor = None;
            deltas.clear();
        }
        if Instant::now() >= deadline {
            return Err("the hanging lever did not settle at its gravity low point".into());
        }
    }
}

#[derive(Debug)]
struct FreshSample {
    position_raw: i32,
    velocity_rad_per_s: f64,
    actual_torque_permille: i16,
    temperature_c: f64,
    observed_at: Instant,
}

#[allow(clippy::too_many_arguments)]
async fn fresh_sample(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    node_id: u8,
    epoch: u64,
    zero_position_raw: i32,
    generation: &mut u64,
    cancel: &AtomicBool,
    enforce_limits: bool,
) -> Result<FreshSample, String> {
    let sample =
        fresh_sample_without_zero(shared, manager, node_id, epoch, generation, cancel).await?;
    let angle = angle_from_zero(sample.position_raw, zero_position_raw);
    {
        let mut session = shared.lock().await;
        session.view.angle_deg = Some(angle.to_degrees());
    }
    if enforce_limits && angle.abs() >= HARD_ANGLE_RAD {
        return Err(format!(
            "80 degree hard angle protection tripped at {:.2} degrees",
            angle.to_degrees()
        ));
    }
    if enforce_limits && sample.velocity_rad_per_s.abs() >= HARD_SPEED_RAD_PER_S {
        return Err(format!(
            "2 rad/s hard speed protection tripped at {:.3} rad/s",
            sample.velocity_rad_per_s
        ));
    }
    Ok(sample)
}

async fn fresh_sample_without_zero(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    node_id: u8,
    epoch: u64,
    generation: &mut u64,
    cancel: &AtomicBool,
) -> Result<FreshSample, String> {
    let deadline = Instant::now() + SAMPLE_TIMEOUT;
    loop {
        check_cancel(cancel)?;
        let state = manager.status(node_id).map_err(to_string)?;
        if state.session_epoch != epoch {
            return Err("motor session changed during torque calibration".into());
        }
        if !state.connection.online {
            return Err("motor went offline during torque calibration".into());
        }
        if !matches!(state.lifecycle, MeowMotorLifecycle::Initialized) {
            return Err(format!(
                "motor left Initialized during torque calibration: {:?}",
                state.lifecycle
            ));
        }
        if state.measurements.tpdo1_generation > *generation {
            *generation = state.measurements.tpdo1_generation;
            if !state.measurements.position_accumulation_valid {
                return Err("position accumulation became invalid".into());
            }
            let position_raw = state
                .measurements
                .position
                .ok_or_else(|| "position is unavailable".to_string())?
                .raw();
            let velocity_rad_per_s = state
                .measurements
                .velocity_rev_per_s
                .ok_or_else(|| "velocity is unavailable".to_string())?
                * std::f64::consts::TAU;
            let temperature_c = f64::from(
                state
                    .measurements
                    .motor_temp_c
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| "motor temperature is unavailable".to_string())?,
            );
            let actual_torque_permille = state
                .measurements
                .torque_permille
                .ok_or_else(|| "raw 0x4577 torque is unavailable".to_string())?;
            let observed_at = Instant::now();
            let mut session = shared.lock().await;
            session.view.velocity_rad_per_s = Some(velocity_rad_per_s);
            session.view.actual_torque_permille = Some(actual_torque_permille);
            session.view.motor_temperature_c = Some(temperature_c);
            return Ok(FreshSample {
                position_raw,
                velocity_rad_per_s,
                actual_torque_permille,
                temperature_c,
                observed_at,
            });
        }
        if Instant::now() >= deadline {
            return Err("no fresh TPDO1 feedback within 300 ms".into());
        }
        tokio::time::sleep(POLL_PERIOD).await;
    }
}

fn fit_result(
    request: TorqueCalibrationRequest,
    peak_torque_nm: f64,
    session_epoch: u64,
    zero_position_raw: i32,
    samples: Vec<RawSweepSample>,
    pass_summaries: Vec<TorquePassSummary>,
) -> Result<TorqueCalibrationResult, String> {
    let maximum_gravity_torque_nm =
        request.mass_kg * STANDARD_GRAVITY_M_PER_S2 * request.center_distance_m;
    let mut fit_points = Vec::with_capacity(FIT_GRID_DEG.len());
    for angle_deg in FIT_GRID_DEG {
        let angle_rad = angle_deg.to_radians();
        let (forward_raw_nm, forward_stddev_raw_nm, forward_samples) =
            binned_torque(&samples, SweepDirection::Forward, angle_rad)?;
        let (reverse_raw_nm, reverse_stddev_raw_nm, reverse_samples) =
            binned_torque(&samples, SweepDirection::Reverse, angle_rad)?;
        let midpoint_raw_nm = (forward_raw_nm + reverse_raw_nm) / 2.0;
        fit_points.push(TorqueFitPoint {
            angle_deg,
            gravity_torque_nm: maximum_gravity_torque_nm * angle_rad.sin(),
            forward_raw_nm,
            reverse_raw_nm,
            midpoint_raw_nm,
            fitted_raw_nm: 0.0,
            friction_half_difference_raw_nm: (forward_raw_nm - reverse_raw_nm) / 2.0,
            corrected_residual_nm: 0.0,
            forward_stddev_raw_nm,
            reverse_stddev_raw_nm,
            forward_samples,
            reverse_samples,
        });
    }

    let torque_factor = slope_through_origin(&fit_points)?;
    if !torque_factor.is_finite() || torque_factor <= 0.0 {
        return Err(format!("invalid fitted torque factor {torque_factor}"));
    }
    for point in &mut fit_points {
        point.fitted_raw_nm = torque_factor * point.gravity_torque_nm;
        point.corrected_residual_nm =
            point.midpoint_raw_nm / torque_factor - point.gravity_torque_nm;
    }
    let positive_torque_factor = slope_through_origin(
        &fit_points
            .iter()
            .filter(|point| point.angle_deg > 0.0)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    let negative_torque_factor = slope_through_origin(
        &fit_points
            .iter()
            .filter(|point| point.angle_deg < 0.0)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    let torque_fit_rmse_nm = (fit_points
        .iter()
        .map(|point| point.corrected_residual_nm.powi(2))
        .sum::<f64>()
        / fit_points.len() as f64)
        .sqrt();
    let directional_mean = (positive_torque_factor + negative_torque_factor) / 2.0;
    let directional_asymmetry_percent = if directional_mean.abs() > f64::EPSILON {
        (positive_torque_factor - negative_torque_factor).abs() / directional_mean.abs() * 100.0
    } else {
        f64::INFINITY
    };
    let mean_hysteresis_half_width_raw_nm = fit_points
        .iter()
        .map(|point| point.friction_half_difference_raw_nm.abs())
        .sum::<f64>()
        / fit_points.len() as f64;
    let forward_friction_offset_raw_nm = mean(
        &fit_points
            .iter()
            .map(|point| point.forward_raw_nm - point.fitted_raw_nm)
            .collect::<Vec<_>>(),
    )?;
    let reverse_friction_offset_raw_nm = mean(
        &fit_points
            .iter()
            .map(|point| point.reverse_raw_nm - point.fitted_raw_nm)
            .collect::<Vec<_>>(),
    )?;
    let temperatures: Vec<f64> = samples
        .iter()
        .map(|sample| sample.temperature_c)
        .filter(|value| value.is_finite())
        .collect();
    let rejected_sample_count = pass_summaries
        .iter()
        .map(|summary| summary.rejected_samples)
        .sum();

    Ok(TorqueCalibrationResult {
        session_epoch,
        node_id: request.node_id,
        vendor_id: request.expected_vendor_id,
        product_code: request.expected_product_code,
        revision_number: request.expected_revision_number,
        serial_number: request.expected_serial_number,
        mass_kg: request.mass_kg,
        center_distance_m: request.center_distance_m,
        standard_gravity_m_per_s2: STANDARD_GRAVITY_M_PER_S2,
        maximum_gravity_torque_nm,
        peak_torque_nm,
        torque_factor,
        torque_fit_rmse_nm,
        positive_torque_factor,
        negative_torque_factor,
        directional_asymmetry_percent,
        mean_hysteresis_half_width_raw_nm,
        forward_friction_offset_raw_nm,
        reverse_friction_offset_raw_nm,
        calibration_temperature_c: mean(&temperatures)?,
        zero_position_raw,
        sweep_endpoint_deg: SWEEP_ENDPOINT_RAD.to_degrees(),
        fit_angle_limit_deg: FIT_ANGLE_LIMIT_RAD.to_degrees(),
        sweep_speed_rad_per_s: request.sweep_speed_rad_per_s,
        sweep_acceleration_rad_per_s2: request.sweep_acceleration_rad_per_s2,
        sweep_cycles: request.sweep_cycles,
        control_rate_hz: 1000,
        controller_kp_nm_per_rad: request.controller_kp_nm_per_rad,
        controller_kd_nm_s_per_rad: request.controller_kd_nm_s_per_rad,
        max_torque_permille: request.max_torque_permille,
        accepted_sample_count: samples.len().try_into().unwrap_or(u32::MAX),
        rejected_sample_count,
        pass_summaries,
        fit_points,
    })
}

fn binned_torque(
    samples: &[RawSweepSample],
    direction: SweepDirection,
    angle_rad: f64,
) -> Result<(f64, f64, u32), String> {
    let values: Vec<f64> = samples
        .iter()
        .filter(|sample| {
            sample.direction == direction
                && (sample.angle_rad - angle_rad).abs() <= BIN_HALF_WIDTH_RAD
        })
        .map(|sample| sample.raw_torque_nm)
        .collect();
    if values.len() < 20 {
        return Err(format!(
            "{} traversal has only {} accepted samples near {:.0} degrees; at least 20 are required",
            direction.name(),
            values.len(),
            angle_rad.to_degrees()
        ));
    }
    let (mean, stddev) = trimmed_mean_stddev(&values)?;
    Ok((mean, stddev, values.len().try_into().unwrap_or(u32::MAX)))
}

fn slope_through_origin(points: &[TorqueFitPoint]) -> Result<f64, String> {
    let denominator = points
        .iter()
        .map(|point| point.gravity_torque_nm.powi(2))
        .sum::<f64>();
    if denominator <= f64::EPSILON {
        return Err("gravity fit has no usable excitation".into());
    }
    Ok(points
        .iter()
        .map(|point| point.gravity_torque_nm * point.midpoint_raw_nm)
        .sum::<f64>()
        / denominator)
}

async fn acceptance_and_cleanup(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    bus: &Arc<dyn CanBus>,
    host_node_id: u8,
    result: &TorqueCalibrationResult,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let expected = ExpectedIdentity {
        vendor_id: result.vendor_id,
        product_code: result.product_code,
        revision_number: result.revision_number,
        serial_number: result.serial_number,
    };
    let info = verify_identity(manager, result.node_id, expected)?;
    if info.session_epoch != result.session_epoch {
        return Err(
            "motor session changed after measurement; rerun the torque sweep before acceptance"
                .into(),
        );
    }
    let live = manager.status(result.node_id).map_err(to_string)?;
    if !matches!(live.lifecycle, MeowMotorLifecycle::Initialized) {
        return Err(format!(
            "motor must remain Initialized after measurement for acceptance, observed {:?}",
            live.lifecycle
        ));
    }
    let brs = match info.can_settings {
        MeowMotorCanSettingsStatus::Available(settings) => settings.transmit_pdo_brs,
        _ => return Err("motor CAN settings are unavailable for acceptance RPDO".into()),
    };
    let original = RuntimeSettings::read(bus.as_ref(), result.node_id).await?;
    let mapping = mit_mapping_for_gravity(result.maximum_gravity_torque_nm);
    let run = run_acceptance(
        shared,
        manager,
        bus,
        host_node_id,
        result,
        expected,
        mapping,
        brs,
        cancel,
    )
    .await;
    let run = match run {
        Err(error)
            if cancel.load(Ordering::Acquire)
                && error == "torque calibration cancelled by operator" =>
        {
            Ok(())
        }
        other => other,
    };
    set_phase(shared, "cleanup", 99).await;
    let zero_send = send_compressed_zero(bus.as_ref(), host_node_id, mapping, brs).await;
    let cleanup = safe_cleanup(manager, bus.as_ref(), result.node_id, original).await;
    if let Err(error) = zero_send {
        shared.lock().await.view.cleanup_warning = Some(error.clone());
        if run.is_ok() {
            return Err(error);
        }
    }
    merge_cleanup(shared, run, cleanup).await
}

#[allow(clippy::too_many_arguments)]
async fn run_acceptance(
    shared: &Arc<Mutex<Session>>,
    manager: &MeowMotorManager,
    bus: &Arc<dyn CanBus>,
    host_node_id: u8,
    result: &TorqueCalibrationResult,
    expected: ExpectedIdentity,
    mapping: CompressedMitMapping,
    brs: bool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let info = verify_identity(manager, result.node_id, expected)?;
    let epoch = info.session_epoch;
    let mut generation = manager
        .status(result.node_id)
        .map_err(to_string)?
        .measurements
        .tpdo1_generation;
    // Reject a lever already outside the guarded envelope while the motor is
    // still Disabled. Do not briefly enable MIT and discover this afterward.
    fresh_sample(
        shared,
        manager,
        result.node_id,
        epoch,
        result.zero_position_raw,
        &mut generation,
        cancel,
        true,
    )
    .await?;
    let consumer = encode_consumer_heartbeat_entry(host_node_id, 250);
    sdo::download(
        bus.as_ref(),
        result.node_id,
        0x1016,
        1,
        &consumer.to_le_bytes(),
        Some(Duration::from_millis(500)),
    )
    .await
    .map_err(to_string)?;
    manager
        .set_max_torque(result.node_id, result.max_torque_permille)
        .await
        .map_err(to_string)?;
    send_compressed_zero(bus.as_ref(), host_node_id, mapping, brs).await?;
    enter_compressed_mit(bus.as_ref(), manager, result.node_id, cancel).await?;

    loop {
        if cancel.load(Ordering::Acquire) {
            for _ in 0..3 {
                send_mit_target(bus.as_ref(), host_node_id, mapping, brs, 0.0).await?;
            }
            manager.disable(result.node_id).await.map_err(to_string)?;
            return Ok(());
        }
        let sample = fresh_sample(
            shared,
            manager,
            result.node_id,
            epoch,
            result.zero_position_raw,
            &mut generation,
            cancel,
            true,
        )
        .await?;
        let angle = angle_from_zero(sample.position_raw, result.zero_position_raw);
        let desired_physical_nm = result.maximum_gravity_torque_nm * angle.sin();
        let raw_command_nm = desired_physical_nm * result.torque_factor;
        let command_permille = raw_command_nm / result.peak_torque_nm * 1000.0;
        if command_permille.abs() > f64::from(result.max_torque_permille) {
            return Err(format!(
                "acceptance torque ceiling exceeded: {command_permille:.1} permille"
            ));
        }
        send_mit_target(bus.as_ref(), host_node_id, mapping, brs, raw_command_nm).await?;
        {
            let mut session = shared.lock().await;
            session.view.current_command_nm = raw_command_nm;
            session.view.current_command_permille = command_permille.round() as i16;
            session.view.actual_torque_nm =
                Some(f64::from(sample.actual_torque_permille) * result.peak_torque_nm / 1000.0);
        }
        tokio::time::sleep(ACCEPTANCE_PERIOD).await;
    }
}

async fn send_mit_target(
    bus: &dyn CanBus,
    host_node_id: u8,
    mapping: CompressedMitMapping,
    brs: bool,
    torque_nm: f64,
) -> Result<(), String> {
    send_compressed_target(
        bus,
        host_node_id,
        mapping,
        brs,
        CompressedMitTarget {
            position: 0.0,
            velocity: 0.0,
            torque: torque_nm as f32,
            kp: 0.0,
            kd: 0.0,
        },
    )
    .await
}

async fn send_measurement_mit_target(
    bus: &dyn CanBus,
    host_node_id: u8,
    mapping: CompressedMitMapping,
    brs: bool,
    torque_nm: f64,
    desired_velocity_rad_per_s: f64,
    kd_nm_s_per_rad: f64,
) -> Result<(), String> {
    send_compressed_target(
        bus,
        host_node_id,
        mapping,
        brs,
        CompressedMitTarget {
            // Kp remains zero because the compressed position field cannot
            // represent an arbitrary accumulated Q8.24 multi-turn position.
            position: 0.0,
            velocity: (desired_velocity_rad_per_s / std::f64::consts::TAU) as f32,
            torque: torque_nm as f32,
            kp: 0.0,
            // Hardware velocity is Rev/s, hence Nm*s/rad -> Nm*s/Rev.
            kd: (kd_nm_s_per_rad * std::f64::consts::TAU) as f32,
        },
    )
    .await
}

async fn send_compressed_target(
    bus: &dyn CanBus,
    host_node_id: u8,
    mapping: CompressedMitMapping,
    brs: bool,
    target: CompressedMitTarget,
) -> Result<(), String> {
    let data = target.to_le_bytes(&mapping);
    let cob_id = host_tpdo_cob_id(host_node_id, 0).map_err(to_string)?;
    let frame = if brs {
        CanFrame::new_fd(cob_id, &data, true)
    } else {
        CanFrame::new_data(cob_id, &data)
    }
    .map_err(to_string)?;
    bus.send(frame).await.map_err(to_string)
}

fn verify_identity(
    manager: &MeowMotorManager,
    node_id: u8,
    expected: ExpectedIdentity,
) -> Result<hex_motor::meow_motor::MeowMotorInfo, String> {
    let info = manager
        .list()
        .into_iter()
        .find(|info| info.node_id == node_id)
        .ok_or_else(|| format!("node 0x{node_id:02X} disappeared"))?;
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
    let expected_tuple = (
        expected.vendor_id,
        expected.product_code,
        expected.revision_number,
        expected.serial_number,
    );
    if observed != expected_tuple {
        return Err(format!(
            "identity changed: expected {expected_tuple:08X?}, observed {observed:08X?}"
        ));
    }
    if !info.online {
        return Err("the selected motor is offline".into());
    }
    Ok(info)
}

async fn merge_cleanup<T>(
    shared: &Arc<Mutex<Session>>,
    result: Result<T, String>,
    cleanup: Result<(), String>,
) -> Result<T, String> {
    if let Err(error) = cleanup {
        shared.lock().await.view.cleanup_warning = Some(error.clone());
        if result.is_ok() {
            return Err(format!(
                "operation completed, but safe cleanup failed: {error}"
            ));
        }
    }
    result
}

async fn set_phase(shared: &Arc<Mutex<Session>>, phase: &str, progress: u8) {
    let mut session = shared.lock().await;
    session.view.phase = phase.into();
    session.view.progress_percent = progress;
}

fn angle_from_zero(position_raw: i32, zero_position_raw: i32) -> f64 {
    f64::from(position_raw.wrapping_sub(zero_position_raw)) / 16_777_216.0 * std::f64::consts::TAU
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("torque calibration cancelled by operator".into())
    } else {
        Ok(())
    }
}

fn mean(values: &[f64]) -> Result<f64, String> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return Err("sample set is empty or non-finite".into());
    }
    Ok(values.iter().sum::<f64>() / values.len() as f64)
}

fn mean_stddev(values: &[f64]) -> Result<(f64, f64), String> {
    let mean = mean(values)?;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64;
    Ok((mean, variance.sqrt()))
}

fn trimmed_mean_stddev(values: &[f64]) -> Result<(f64, f64), String> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return Err("sample set is empty or non-finite".into());
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let trim = if sorted.len() >= 20 {
        sorted.len() / 10
    } else {
        0
    };
    mean_stddev(&sorted[trim..sorted.len() - trim])
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> TorqueCalibrationRequest {
        TorqueCalibrationRequest {
            node_id: 1,
            expected_vendor_id: 1,
            expected_product_code: 2,
            expected_revision_number: 3,
            expected_serial_number: 4,
            mass_kg: 1.0,
            center_distance_m: 0.27,
            sweep_speed_rad_per_s: 0.2,
            sweep_acceleration_rad_per_s2: 0.4,
            sweep_cycles: 2,
            controller_kp_nm_per_rad: 12.0,
            controller_kd_nm_s_per_rad: 2.0,
            max_torque_permille: 400,
        }
    }

    fn synthetic_samples(factor: f64, friction_nm: f64) -> Vec<RawSweepSample> {
        let maximum_gravity_nm = STANDARD_GRAVITY_M_PER_S2 * 0.27;
        let mut samples = Vec::new();
        for direction in [SweepDirection::Forward, SweepDirection::Reverse] {
            let friction_sign = direction.velocity_sign();
            for angle_deg in FIT_GRID_DEG {
                for sample_index in 0..30 {
                    let offset_deg = (f64::from(sample_index) - 14.5) / 15.0;
                    let angle_rad = (angle_deg + offset_deg).to_radians();
                    let deterministic_noise = f64::from(sample_index % 5) * 0.000_2 - 0.000_4;
                    samples.push(RawSweepSample {
                        direction,
                        angle_rad,
                        velocity_rad_per_s: direction.velocity_sign() * 0.2,
                        raw_torque_nm: factor * maximum_gravity_nm * angle_rad.sin()
                            + friction_sign * friction_nm
                            + deterministic_noise,
                        temperature_c: 25.0,
                    });
                }
            }
        }
        samples
    }

    #[test]
    fn fixture_maximum_is_2_6478_nm() {
        let value = STANDARD_GRAVITY_M_PER_S2 * 0.27;
        assert!((value - 2.647_795_5).abs() < 1e-9);
    }

    #[test]
    fn paired_mit_fit_cancels_symmetric_coulomb_friction() {
        let passes = vec![
            TorquePassSummary {
                cycle: 1,
                direction: "forward".into(),
                accepted_samples: 390,
                rejected_samples: 7,
                mean_velocity_rad_per_s: 0.2,
                velocity_stddev_rad_per_s: 0.001,
                peak_absolute_velocity_rad_per_s: 0.205,
                peak_tracking_error_deg: 1.0,
                minimum_raw_torque_nm: -3.0,
                maximum_raw_torque_nm: 3.0,
            },
            TorquePassSummary {
                cycle: 1,
                direction: "reverse".into(),
                accepted_samples: 390,
                rejected_samples: 9,
                mean_velocity_rad_per_s: -0.2,
                velocity_stddev_rad_per_s: 0.001,
                peak_absolute_velocity_rad_per_s: 0.205,
                peak_tracking_error_deg: 1.0,
                minimum_raw_torque_nm: -3.0,
                maximum_raw_torque_nm: 3.0,
            },
        ];
        let result = fit_result(
            request(),
            10.0,
            42,
            0,
            synthetic_samples(1.12, 0.08),
            passes,
        )
        .unwrap();
        assert!((result.torque_factor - 1.12).abs() < 2e-4);
        assert!((result.mean_hysteresis_half_width_raw_nm - 0.08).abs() < 2e-4);
        assert_eq!(result.rejected_sample_count, 16);
    }

    #[test]
    fn wrapped_position_delta_stays_local() {
        let zero = i32::MAX - 100;
        let current = zero.wrapping_add((16_777_216_f64 * 0.125) as i32);
        assert!((angle_from_zero(current, zero) - std::f64::consts::FRAC_PI_4).abs() < 1e-9);
    }

    #[test]
    fn request_enforces_fixture_safety_bounds() {
        request().validate().unwrap();
        let mut unsafe_request = request();
        unsafe_request.sweep_speed_rad_per_s = 0.5;
        unsafe_request.sweep_acceleration_rad_per_s2 = 0.1;
        assert!(unsafe_request.validate().is_err());
    }

    #[test]
    fn compressed_mapping_encodes_an_exact_zero_code() {
        let mapping = measurement_mit_mapping(request());
        let bytes = CompressedMitTarget::ZERO.to_le_bytes(&mapping);
        let lower = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let upper = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(lower & 0x0FFF, 2_047, "Tff zero code");
        assert_eq!((lower >> 12) & 0x0FFF, 0, "Kd zero code");
        assert_eq!((upper >> 4) & 0x0FFF, 2_047, "velocity zero code");
        assert_eq!(upper >> 16, 32_767, "position zero code");
    }

    #[test]
    fn measurement_mapping_carries_motor_side_velocity_damping() {
        let request = request();
        let mapping = measurement_mit_mapping(request);
        let velocity_rev_per_s = request.sweep_speed_rad_per_s / std::f64::consts::TAU;
        let kd_nm_s_per_rev = request.controller_kd_nm_s_per_rad * std::f64::consts::TAU;
        assert!(f64::from(mapping.velocity_min) <= -velocity_rev_per_s);
        assert!(f64::from(mapping.velocity_max) >= velocity_rev_per_s);
        assert!(f64::from(mapping.kd_max) >= kd_nm_s_per_rev);
    }

    #[test]
    fn five_ms_velocity_window_rejects_captured_one_tick_reversal_peak() {
        // Representative positions from the 2026-08-07 can1 capture. The
        // one-millisecond derivative reaches 2.07 rad/s even though the whole
        // oscillation is only about 0.4 degrees.
        let positions: [i32; 9] = [
            212_620_749,
            212_625_792,
            212_630_298,
            212_632_602,
            212_631_000,
            212_626_867,
            212_621_338,
            212_616_551,
            212_613_427,
        ];
        let mut estimator = WindowedVelocity::default();
        let mut timestamp = 62_000_u16;
        let mut previous = None;
        let mut peak_instantaneous = 0.0_f64;
        let mut peak_windowed = 0.0_f64;
        for position in positions {
            if let Some(previous) = previous {
                let delta_rev = position.wrapping_sub(previous) as f64 / 16_777_216.0;
                let velocity = delta_rev * std::f64::consts::TAU * 1_000.0;
                peak_instantaneous = peak_instantaneous.max(velocity.abs());
            }
            if let Some(velocity) = estimator.observe(position, timestamp) {
                peak_windowed = peak_windowed.max(velocity.abs());
            }
            previous = Some(position);
            timestamp = timestamp.wrapping_add(1_000);
        }
        assert!(peak_instantaneous > HARD_SPEED_RAD_PER_S);
        assert!(peak_windowed < 1.5);
    }

    #[test]
    fn host_trajectory_respects_speed_acceleration_and_endpoint() {
        let dt = 0.001;
        let mut angle = 0.0;
        let mut velocity = 0.0;
        let mut peak_speed = 0.0_f64;
        let mut peak_acceleration = 0.0_f64;
        for _ in 0..10_000 {
            let previous_velocity = velocity;
            (angle, velocity) =
                advance_limited_trajectory(angle, velocity, SWEEP_ENDPOINT_RAD, 0.2, 0.4, dt);
            peak_speed = peak_speed.max(velocity.abs());
            peak_acceleration = peak_acceleration.max(((velocity - previous_velocity) / dt).abs());
        }
        assert!(peak_speed <= 0.2 + 1e-12);
        assert!(peak_acceleration <= 0.4 + 1e-9);
        assert!((angle - SWEEP_ENDPOINT_RAD).abs() < 1e-12);
        assert_eq!(velocity, 0.0);
    }
}
