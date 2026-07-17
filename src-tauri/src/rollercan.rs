//! RollerCAN firmware-owned SmartKnob session.
//!
//! Unit RollerCAN is not a HEX/CiA402 motor. The default device speaks a
//! proprietary CAN 2.0 29-bit extended-frame protocol at 1 Mbps, with default
//! node id `0xA8`. The STM32 owns the 1 kHz haptic loop; this module sends mode
//! and tuning parameters and decodes the firmware's unsolicited telemetry.
//!
//! The old host-side haptic helpers remain below temporarily as test/reference
//! code, but no runtime path starts that loop or streams current commands.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use can_transport::{CanBus, CanFilter, CanFrame, CanId, CanIoError, CanRx, FrameKind};
use serde::Serialize;
use tokio::task::JoinHandle;

pub use crate::smartknob::KnobConfig;

const HISTORY_CAP: usize = 80;
const CONTROL_HZ: u64 = 500;
const CURRENT_MODE: i32 = 3;
const CURRENT_X100_LIMIT: i32 = 120_000;
const MA_X100_PER_AMP: f64 = 100_000.0;
const ROLLER_DEFAULT_CURRENT_LIMIT_A: f64 = 1.2;
const ROLLER_OUTPUT_DEADBAND_A: f64 = 0.06;
const ROLLER_CURRENT_DIRECTION: f64 = 1.0;
const ROLLER_SENSOR_DIRECTION: f64 = 1.0;
const DEG: f64 = std::f64::consts::PI / 180.0;

const OD_SAVE_FLASH: u16 = 0x7002;
const OD_RELEASE_PROTECTION: u16 = 0x7003;
const OD_ENABLE: u16 = 0x7004;
const OD_RUN_MODE: u16 = 0x7005;
const OD_CURRENT: u16 = 0x7006;
const OD_SPEED_READBACK: u16 = 0x7030;
const OD_POSITION_READBACK: u16 = 0x7031;
const OD_CURRENT_READBACK: u16 = 0x7032;

const RC_CMD_SET_CONFIG: u16 = 0x8001;
const RC_TELEMETRY_ENABLE: u16 = 0x8002;
const RC_TELEMETRY_RATE_HZ: u16 = 0x8003;
const RC_TELEMETRY_HOST_ID: u16 = 0x8004;
const RC_TUNING_P_GAIN: u16 = 0x8101;
const RC_TUNING_D_GAIN: u16 = 0x8102;
const RC_TUNING_STRENGTH: u16 = 0x8103;
const RC_TUNING_TORQUE_LIMIT: u16 = 0x8104;
const RC_TUNING_MAX_TORQUE: u16 = 0x8105;
const RC_TUNING_FRICTION: u16 = 0x8106;
const RC_TUNING_CLICK: u16 = 0x8107;
const RC_CUSTOM_POSITION: u16 = 0x8201;
const RC_CUSTOM_MIN_POSITION: u16 = 0x8202;
const RC_CUSTOM_MAX_POSITION: u16 = 0x8203;
const RC_CUSTOM_WIDTH_DEG: u16 = 0x8204;
const RC_CUSTOM_DETENT_STRENGTH: u16 = 0x8205;
const RC_CUSTOM_ENDSTOP_STRENGTH: u16 = 0x8206;
const RC_CUSTOM_SNAP_POINT: u16 = 0x8207;
const RC_CUSTOM_SNAP_BIAS: u16 = 0x8208;
const RC_CUSTOM_CLICK: u16 = 0x8209;
const RC_CUSTOM_FRICTION: u16 = 0x820A;
const RC_CUSTOM_STRENGTH: u16 = 0x820B;
const RC_CUSTOM_P_GAIN: u16 = 0x820C;
const RC_CUSTOM_D_GAIN: u16 = 0x820D;
const RC_CUSTOM_LED_HUE: u16 = 0x820E;
const SCALE: f64 = 1000.0;

const DEAD_ZONE_DETENT_PERCENT: f64 = 0.2;
const DEAD_ZONE_RAD: f64 = std::f64::consts::PI / 180.0;
const IDLE_VELOCITY_EWMA_ALPHA: f64 = 0.001;
const IDLE_VELOCITY_RAD_PER_SEC: f64 = 0.05;
const IDLE_CORRECTION_DELAY: Duration = Duration::from_millis(500);
const IDLE_CORRECTION_MAX_ANGLE_RAD: f64 = 5.0 * std::f64::consts::PI / 180.0;
const IDLE_CORRECTION_RATE_ALPHA: f64 = 0.0005;
const MAX_VEL_RAD_S: f64 = 60.0;
const PID_LIMIT: f64 = 10.0;
const CLICK_PHASE_DURATION: Duration = Duration::from_millis(2);
const CLICK_TOTAL_DURATION: Duration = Duration::from_millis(4);
const HAPTIC_TIMING_WARN_THRESHOLD: Duration = Duration::from_millis(4);

#[derive(Clone, Default, Serialize)]
pub struct RollerCanFeedback {
    pub node_id: u8,
    pub host_id: u8,
    pub speed_rpm: i16,
    pub position_deg: i16,
    pub current_ma: i16,
    pub voltage_v: i16,
    pub mode: u8,
    pub state: u8,
    pub fault_raw: u8,
    pub fault_over_range: bool,
    pub fault_stall: bool,
    pub fault_over_voltage: bool,
    pub age_ms: u64,
}

#[derive(Clone, Default)]
struct RollerCanRealtime {
    position_deg: Option<(f64, Instant)>,
    speed_rpm: Option<(f64, Instant)>,
    current_a: Option<(f64, Instant)>,
}

#[derive(Clone)]
struct RollerCanSensor {
    shaft_angle_rad: f64,
    position_at: Instant,
    speed_rpm: Option<f64>,
    current_a: Option<f64>,
    feedback: RollerCanFeedback,
}

#[derive(Clone, Serialize)]
pub struct RollerCanEvent {
    pub t_ms: u64,
    pub dir: &'static str,
    pub id: u32,
    pub data: String,
    pub note: String,
}

#[derive(Clone, Default, Serialize)]
pub struct RollerCanStateDto {
    pub connected: bool,
    #[serde(flatten)]
    pub knob: crate::smartknob::SmartKnobState,
    pub feedback: Option<RollerCanFeedback>,
    pub events: Vec<RollerCanEvent>,
}

#[derive(Default)]
struct RollerCanState {
    feedback: Option<(RollerCanFeedback, Instant)>,
    realtime: RollerCanRealtime,
    knob: crate::smartknob::SmartKnobState,
    events: VecDeque<RollerCanEvent>,
}

impl RollerCanState {
    fn push_event(&mut self, t_ms: u64, dir: &'static str, id: u32, data: &[u8], note: String) {
        if self.events.len() >= HISTORY_CAP {
            self.events.pop_front();
        }
        self.events.push_back(RollerCanEvent {
            t_ms,
            dir,
            id,
            data: hex(data),
            note,
        });
    }

    fn feedback(&self) -> Option<RollerCanFeedback> {
        let now = Instant::now();
        self.feedback.as_ref().map(|(f, at)| {
            let mut f = f.clone();
            f.age_ms = now.duration_since(*at).as_millis() as u64;
            f
        })
    }

    fn sensor(&self) -> Option<RollerCanSensor> {
        let feedback = self.feedback()?;
        let now = Instant::now();
        let position = self
            .realtime
            .position_deg
            .filter(|(_, at)| now.duration_since(*at) < Duration::from_millis(250));
        let (position_deg, position_at) = position.unwrap_or((
            feedback.position_deg as f64,
            now - Duration::from_millis(feedback.age_ms),
        ));
        let speed_rpm = self
            .realtime
            .speed_rpm
            .filter(|(_, at)| now.duration_since(*at) < Duration::from_millis(250))
            .map(|(v, _)| v)
            .or(Some(feedback.speed_rpm as f64));
        let current_a = self
            .realtime
            .current_a
            .filter(|(_, at)| now.duration_since(*at) < Duration::from_millis(250))
            .map(|(v, _)| v)
            .or(Some(feedback.current_ma as f64 / 1000.0));

        Some(RollerCanSensor {
            shaft_angle_rad: ROLLER_SENSOR_DIRECTION * position_deg.to_radians(),
            position_at,
            speed_rpm,
            current_a,
            feedback,
        })
    }

    fn snapshot(&self, connected: bool) -> RollerCanStateDto {
        RollerCanStateDto {
            connected,
            knob: self.knob.clone(),
            feedback: self.feedback(),
            events: self.events.iter().cloned().collect(),
        }
    }
}

#[derive(Clone, Copy)]
struct Tuning {
    p_gain: f64,
    d_gain: f64,
    strength_scale: f64,
    torque_limit_nm: f64,
    max_torque_permille: u16,
    friction_compensation: f64,
    click_torque_nm: f64,
}

impl Tuning {
    fn from_config(config: &KnobConfig) -> Self {
        Self {
            p_gain: config.p_gain,
            d_gain: config.d_gain,
            strength_scale: config.strength_scale,
            torque_limit_nm: ROLLER_DEFAULT_CURRENT_LIMIT_A,
            max_torque_permille: crate::smartknob::DEFAULT_MAX_TORQUE_PERMILLE,
            friction_compensation: config.friction_compensation,
            click_torque_nm: config.click_torque_nm,
        }
        .sanitized()
    }

    fn sanitized(self) -> Self {
        Self {
            p_gain: finite_nonnegative(self.p_gain),
            d_gain: finite_nonnegative(self.d_gain),
            strength_scale: finite_nonnegative(self.strength_scale),
            torque_limit_nm: finite_nonnegative(self.torque_limit_nm),
            max_torque_permille: self.max_torque_permille.min(1000),
            friction_compensation: finite_nonnegative(self.friction_compensation),
            click_torque_nm: finite_nonnegative(self.click_torque_nm),
        }
    }
}

fn preset(
    text: &str,
    position: i32,
    min_position: i32,
    max_position: i32,
    width_deg: f64,
    detent_strength_unit: f64,
    endstop_strength_unit: f64,
    snap_point: f64,
    snap_point_bias: f64,
    friction_compensation: f64,
    strength_scale: f64,
    p_gain: f64,
    d_gain: f64,
    led_hue: i32,
) -> KnobConfig {
    KnobConfig {
        position,
        min_position,
        max_position,
        position_width_radians: width_deg * DEG,
        detent_strength_unit,
        endstop_strength_unit,
        snap_point,
        snap_point_bias,
        friction_compensation,
        strength_scale,
        p_gain,
        d_gain,
        text: text.to_string(),
        led_hue,
        ..Default::default()
    }
}

/// RollerCAN-specific haptic presets.
///
/// These deliberately live next to the RollerCAN current-mode controller instead
/// of sharing `smartknob::preset_configs()`: RollerCAN is direct-drive and uses
/// current commands, while the native SmartKnob path targets the HEX actuator's
/// torque interface.
pub fn preset_configs() -> Vec<KnobConfig> {
    let p = preset;
    vec![
        KnobConfig {
            is_custom: true,
            text: "Custom\nEdit me".into(),
            led_hue: 120,
            max_position: -1,
            position_width_radians: 10.0 * DEG,
            snap_point: 0.55,
            friction_compensation: 0.0,
            strength_scale: 0.0875,
            p_gain: 0.0,
            d_gain: 0.0,
            ..p(
                "", 0, 0, -1, 10.0, 0.0, 1.0, 0.55, 0.0, 0.0, 0.0875, 0.0, 0.0, 120,
            )
        },
        p(
            "Unbounded\nNo detents",
            0,
            0,
            -1,
            10.0,
            0.0,
            1.0,
            0.75,
            0.0,
            0.02,
            0.0375,
            0.0,
            0.0,
            200,
        ),
        p(
            "Bounded 0-10\nNo detents",
            0,
            0,
            10,
            10.0,
            0.0,
            1.0,
            1.1,
            0.0,
            0.0,
            0.0625,
            0.0,
            0.0,
            0,
        ),
        p(
            "Multi-rev\nNo detents",
            0,
            0,
            72,
            10.0,
            0.0,
            5.0,
            0.75,
            0.0,
            0.0,
            crate::smartknob::DEFAULT_STRENGTH_SCALE * 0.25,
            0.0,
            0.0,
            73,
        ),
        p(
            "On/off\nStrong detent",
            0,
            0,
            1,
            60.0,
            10.0,
            1.0,
            0.55,
            0.0,
            0.0,
            0.1,
            38.0,
            0.55,
            157,
        ),
        p(
            "Return-to-center",
            0,
            0,
            0,
            60.0,
            0.01,
            0.6,
            1.1,
            0.0,
            crate::smartknob::DEFAULT_FRICTION_COMPENSATION * 0.25,
            0.2,
            40.0,
            0.1,
            45,
        ),
        p(
            "Fine values\nNo detents",
            127,
            0,
            255,
            1.0,
            0.0,
            1.0,
            1.1,
            0.0,
            0.0,
            0.075,
            0.0,
            0.1,
            219,
        ),
        KnobConfig {
            click_torque_nm: 0.1,
            ..p(
                "Fine values\nWith detents",
                127,
                0,
                255,
                1.0,
                1.0,
                1.0,
                0.9,
                0.0,
                crate::smartknob::DEFAULT_FRICTION_COMPENSATION * 0.0,
                0.0625,
                0.0,
                0.1,
                25,
            )
        },
        p(
            "Coarse values\nStrong detents",
            0,
            0,
            31,
            10.0,
            8.0,
            1.0,
            0.75,
            0.0,
            0.0,
            0.2,
            28.0,
            0.16,
            200,
        ),
        KnobConfig {
            click_torque_nm: 0.35,
            ..p(
                "Coarse values\nWeak detents",
                0,
                0,
                31,
                10.0,
                0.2,
                1.0,
                0.9,
                0.0,
                0.0,
                0.2,
                5.0,
                0.16,
                0,
            )
        },
        KnobConfig {
            detent_positions: vec![2, 10, 21, 22],
            ..p(
                "Magnetic detents",
                0,
                0,
                31,
                7.0,
                2.5,
                1.0,
                0.7,
                0.0,
                0.0,
                0.20,
                40.0,
                0.2,
                73,
            )
        },
        p(
            "Return-to-center\nwith detents",
            0,
            -6,
            6,
            60.0,
            1.0,
            1.0,
            0.55,
            0.4,
            0.0,
            0.2,
            10.0,
            0.1,
            157,
        ),
    ]
}

pub struct RollerCanSession {
    bus: Arc<dyn CanBus>,
    state: Arc<StdMutex<RollerCanState>>,
    rx_task: JoinHandle<()>,
    haptic_task: StdMutex<Option<JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    requested_config: Arc<StdMutex<usize>>,
    tuning: Arc<StdMutex<Tuning>>,
    per_mode_tuning: Arc<StdMutex<Vec<Tuning>>>,
    custom_config: Arc<StdMutex<KnobConfig>>,
    custom_config_dirty: Arc<AtomicBool>,
    target_id: StdMutex<Option<u8>>,
    send_lock: Arc<tokio::sync::Mutex<()>>,
    t0: Instant,
}

impl RollerCanSession {
    pub async fn start(spec: &str) -> Result<Self> {
        let bus = crate::backend::open_classic_1m_bus(spec).await?;
        let rx = bus
            .subscribe(CanFilter::pass_all_extended())
            .await
            .map_err(|e| anyhow!("subscribe RollerCAN extended frames: {e}"))?;
        let state = Arc::new(StdMutex::new(RollerCanState::default()));
        let t0 = Instant::now();
        let rx_task = tokio::spawn(drain_loop(rx, state.clone(), t0));
        let configs = preset_configs();
        let per_mode_tuning = configs.iter().map(Tuning::from_config).collect();
        let tuning = Tuning::from_config(&configs[0]);
        log::info!("RollerCAN SmartKnob connected on {spec:?}");
        Ok(Self {
            bus,
            state,
            rx_task,
            haptic_task: StdMutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            requested_config: Arc::new(StdMutex::new(0)),
            tuning: Arc::new(StdMutex::new(tuning)),
            per_mode_tuning: Arc::new(StdMutex::new(per_mode_tuning)),
            custom_config: Arc::new(StdMutex::new(configs[0].clone())),
            custom_config_dirty: Arc::new(AtomicBool::new(false)),
            target_id: StdMutex::new(None),
            send_lock: Arc::new(tokio::sync::Mutex::new(())),
            t0,
        })
    }

    pub fn snapshot(&self) -> RollerCanStateDto {
        self.state.lock().unwrap().snapshot(true)
    }

    pub async fn stop(self) {
        self.stop_knob().await;
        self.rx_task.abort();
        let _ = self.rx_task.await;
        log::info!("RollerCAN SmartKnob disconnected");
    }

    pub async fn ping(&self, host_id: u8, target_id: u8) -> Result<()> {
        self.send_command(0x00, 0, host_id, target_id, [0; 8], "ping")
            .await
    }

    pub async fn enable(&self, config_index: u8, target_id: u8) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        let index = config_index as usize;
        *self.requested_config.lock().unwrap() = index;
        *self.target_id.lock().unwrap() = Some(target_id);
        if index >= preset_configs().len() {
            return Err(anyhow!("invalid RollerCAN SmartKnob mode {index}"));
        }
        self.write_param_raw(0, target_id, OD_RUN_MODE, 4, "firmware SmartKnob mode")
            .await?;
        self.write_param_raw(0, target_id, OD_CURRENT, 0, "zero current")
            .await?;
        self.write_param_raw(
            0,
            target_id,
            RC_CMD_SET_CONFIG,
            index as i32,
            "select firmware preset",
        )
        .await?;
        self.write_param_raw(0, target_id, RC_TELEMETRY_HOST_ID, 0, "telemetry host")
            .await?;
        self.write_param_raw(0, target_id, RC_TELEMETRY_RATE_HZ, 50, "telemetry rate")
            .await?;
        self.write_param_raw(0, target_id, RC_TELEMETRY_ENABLE, 1, "telemetry on")
            .await?;
        self.write_param_raw(0, target_id, OD_ENABLE, 1, "enable")
            .await?;
        self.running.store(true, Ordering::SeqCst);
        let config = preset_configs()[index].clone();
        let tuning = *self.tuning.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        state.knob.running = true;
        state.knob.config_index = index;
        state.knob.config = Some(config.clone());
        state.knob.current_position = config.position;
        state.knob.min_position = config.min_position;
        state.knob.max_position = config.max_position;
        state.knob.num_positions = position_count(&config);
        state.knob.node_id = target_id;
        state.knob.strength_scale = tuning.strength_scale;
        state.knob.torque_limit_nm = tuning.torque_limit_nm;
        state.knob.max_torque_permille = tuning.max_torque_permille;
        state.knob.friction_compensation = tuning.friction_compensation;
        state.knob.click_torque_nm = tuning.click_torque_nm;
        state.knob.p_gain = tuning.p_gain;
        state.knob.d_gain = tuning.d_gain;
        drop(state);
        log::info!("RollerCAN firmware SmartKnob started on 0x{target_id:02X}");
        Ok(())
    }

    pub async fn stop_motor(&self, _host_id: u8, target_id: u8) -> Result<()> {
        self.stop_knob().await;
        let target = self.target_id.lock().unwrap().unwrap_or(target_id);
        self.write_param_raw(0, target, OD_CURRENT, 0, "zero current")
            .await?;
        self.write_param_raw(0, target, OD_ENABLE, 0, "disable")
            .await
    }

    pub async fn release_stall(&self, host_id: u8, target_id: u8) -> Result<()> {
        self.write_param_raw(
            host_id,
            target_id,
            OD_RELEASE_PROTECTION,
            2,
            "release protection",
        )
        .await
    }

    pub async fn save_flash(&self, host_id: u8, target_id: u8) -> Result<()> {
        self.write_param_raw(host_id, target_id, OD_SAVE_FLASH, 2, "save flash")
            .await
    }

    pub async fn set_can_id(&self, host_id: u8, target_id: u8, new_id: u8) -> Result<()> {
        self.send_command(0x07, new_id, host_id, target_id, [0; 8], "set CAN id")
            .await
    }

    pub async fn set_bitrate(&self, host_id: u8, target_id: u8, bitrate: u8) -> Result<()> {
        if bitrate > 2 {
            return Err(anyhow!("bitrate must be 0(1M), 1(500K), or 2(125K)"));
        }
        self.send_command(0x0B, bitrate, host_id, target_id, [0; 8], "set CAN bitrate")
            .await
    }

    pub async fn set_stall_protection(
        &self,
        host_id: u8,
        target_id: u8,
        enabled: bool,
    ) -> Result<()> {
        self.send_command(
            if enabled { 0x0C } else { 0x0D },
            0,
            host_id,
            target_id,
            [0; 8],
            if enabled {
                "stall protection on"
            } else {
                "stall protection off"
            },
        )
        .await
    }

    pub async fn read_param(&self, host_id: u8, target_id: u8, index: u16) -> Result<()> {
        let mut data = [0u8; 8];
        data[0..2].copy_from_slice(&index.to_le_bytes());
        self.send_command(0x11, 0, host_id, target_id, data, "read param")
            .await
    }

    pub async fn write_param(
        &self,
        host_id: u8,
        target_id: u8,
        index: u16,
        value: i32,
    ) -> Result<()> {
        match index {
            RC_CMD_SET_CONFIG => {
                let idx = value.max(0) as usize;
                *self.requested_config.lock().unwrap() = idx;
                if let Some(config) = preset_configs().get(idx).cloned() {
                    let tuning = self
                        .per_mode_tuning
                        .lock()
                        .unwrap()
                        .get(idx)
                        .copied()
                        .unwrap_or_else(|| Tuning::from_config(&config));
                    *self.tuning.lock().unwrap() = tuning;
                    let mut state = self.state.lock().unwrap();
                    state.knob.config_index = idx;
                    state.knob.config = Some(config.clone());
                    state.knob.min_position = config.min_position;
                    state.knob.max_position = config.max_position;
                    state.knob.num_positions = position_count(&config);
                    state.knob.p_gain = tuning.p_gain;
                    state.knob.d_gain = tuning.d_gain;
                    state.knob.strength_scale = tuning.strength_scale;
                    state.knob.torque_limit_nm = tuning.torque_limit_nm;
                    state.knob.max_torque_permille = tuning.max_torque_permille;
                    state.knob.friction_compensation = tuning.friction_compensation;
                    state.knob.click_torque_nm = tuning.click_torque_nm;
                }
                self.write_param_raw(host_id, target_id, index, value, "select firmware preset")
                    .await
            }
            RC_TUNING_P_GAIN
            | RC_TUNING_D_GAIN
            | RC_TUNING_STRENGTH
            | RC_TUNING_TORQUE_LIMIT
            | RC_TUNING_MAX_TORQUE
            | RC_TUNING_FRICTION
            | RC_TUNING_CLICK => {
                let mut t = *self.tuning.lock().unwrap();
                match index {
                    RC_TUNING_P_GAIN => t.p_gain = scaled(value),
                    RC_TUNING_D_GAIN => t.d_gain = scaled(value),
                    RC_TUNING_STRENGTH => t.strength_scale = scaled(value),
                    RC_TUNING_TORQUE_LIMIT => t.torque_limit_nm = scaled(value),
                    RC_TUNING_MAX_TORQUE => t.max_torque_permille = value.clamp(0, 1000) as u16,
                    RC_TUNING_FRICTION => t.friction_compensation = scaled(value),
                    RC_TUNING_CLICK => t.click_torque_nm = scaled(value),
                    _ => unreachable!(),
                }
                t = t.sanitized();
                *self.tuning.lock().unwrap() = t;
                let idx = *self.requested_config.lock().unwrap();
                if let Some(slot) = self.per_mode_tuning.lock().unwrap().get_mut(idx) {
                    *slot = t;
                }
                {
                    let mut state = self.state.lock().unwrap();
                    state.knob.p_gain = t.p_gain;
                    state.knob.d_gain = t.d_gain;
                    state.knob.strength_scale = t.strength_scale;
                    state.knob.torque_limit_nm = t.torque_limit_nm;
                    state.knob.max_torque_permille = t.max_torque_permille;
                    state.knob.friction_compensation = t.friction_compensation;
                    state.knob.click_torque_nm = t.click_torque_nm;
                }
                self.write_param_raw(host_id, target_id, index, value, "firmware tuning")
                    .await
            }
            RC_CUSTOM_POSITION
            | RC_CUSTOM_MIN_POSITION
            | RC_CUSTOM_MAX_POSITION
            | RC_CUSTOM_WIDTH_DEG
            | RC_CUSTOM_DETENT_STRENGTH
            | RC_CUSTOM_ENDSTOP_STRENGTH
            | RC_CUSTOM_SNAP_POINT
            | RC_CUSTOM_SNAP_BIAS
            | RC_CUSTOM_CLICK
            | RC_CUSTOM_FRICTION
            | RC_CUSTOM_STRENGTH
            | RC_CUSTOM_P_GAIN
            | RC_CUSTOM_D_GAIN
            | RC_CUSTOM_LED_HUE => {
                let mut cfg = self.custom_config.lock().unwrap().clone();
                match index {
                    RC_CUSTOM_POSITION => cfg.position = value,
                    RC_CUSTOM_MIN_POSITION => cfg.min_position = value,
                    RC_CUSTOM_MAX_POSITION => cfg.max_position = value,
                    RC_CUSTOM_WIDTH_DEG => {
                        cfg.position_width_radians = scaled(value) * std::f64::consts::PI / 180.0
                    }
                    RC_CUSTOM_DETENT_STRENGTH => cfg.detent_strength_unit = scaled(value),
                    RC_CUSTOM_ENDSTOP_STRENGTH => cfg.endstop_strength_unit = scaled(value),
                    RC_CUSTOM_SNAP_POINT => cfg.snap_point = scaled(value),
                    RC_CUSTOM_SNAP_BIAS => cfg.snap_point_bias = scaled(value),
                    RC_CUSTOM_CLICK => cfg.click_torque_nm = scaled(value),
                    RC_CUSTOM_FRICTION => cfg.friction_compensation = scaled(value),
                    RC_CUSTOM_STRENGTH => cfg.strength_scale = scaled(value),
                    RC_CUSTOM_P_GAIN => cfg.p_gain = scaled(value),
                    RC_CUSTOM_D_GAIN => cfg.d_gain = scaled(value),
                    RC_CUSTOM_LED_HUE => cfg.led_hue = value.clamp(0, 255),
                    _ => unreachable!(),
                }
                *self.custom_config.lock().unwrap() = sanitize_custom_config(cfg);
                self.custom_config_dirty.store(true, Ordering::SeqCst);
                {
                    let mut state = self.state.lock().unwrap();
                    if state.knob.config_index == 0 {
                        state.knob.config = Some(self.custom_config.lock().unwrap().clone());
                    }
                }
                self.write_param_raw(host_id, target_id, index, value, "firmware custom config")
                    .await
            }
            _ => {
                self.write_param_raw(host_id, target_id, index, value, "write param")
                    .await
            }
        }
    }

    async fn write_param_raw(
        &self,
        host_id: u8,
        target_id: u8,
        index: u16,
        value: i32,
        note: &'static str,
    ) -> Result<()> {
        let mut data = [0u8; 8];
        data[0..2].copy_from_slice(&index.to_le_bytes());
        data[4..8].copy_from_slice(&value.to_le_bytes());
        self.send_command(0x12, 0, host_id, target_id, data, note)
            .await
    }

    async fn send_command(
        &self,
        cmd: u8,
        param: u8,
        host_id: u8,
        target_id: u8,
        data: [u8; 8],
        note: &'static str,
    ) -> Result<()> {
        send_command(
            &self.bus,
            &self.send_lock,
            &self.state,
            self.t0,
            cmd,
            param,
            host_id,
            target_id,
            data,
            note,
        )
        .await
    }

    async fn stop_knob(&self) {
        self.running.store(false, Ordering::SeqCst);
        let task = self.haptic_task.lock().unwrap().take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.state.lock().unwrap().knob.running = false;
    }
}

async fn haptic_loop(
    bus: Arc<dyn CanBus>,
    state: Arc<StdMutex<RollerCanState>>,
    running: Arc<AtomicBool>,
    requested_config: Arc<StdMutex<usize>>,
    tuning: Arc<StdMutex<Tuning>>,
    per_mode_tuning: Arc<StdMutex<Vec<Tuning>>>,
    custom_config: Arc<StdMutex<KnobConfig>>,
    custom_config_dirty: Arc<AtomicBool>,
    send_lock: Arc<tokio::sync::Mutex<()>>,
    t0: Instant,
    target_id: u8,
) {
    let configs = preset_configs();
    let period = Duration::from_micros(1_000_000 / CONTROL_HZ);
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut active_index = usize::MAX;
    let mut config = configs[0].clone();
    let mut h = Haptic::new(config.position);
    let mut last_tick_at = Instant::now();
    let mut last_warn: Option<Instant> = None;
    let mut last_position_sample: Option<(f64, Instant)> = None;
    let mut telemetry_phase: u64 = 0;

    while running.load(Ordering::SeqCst) {
        tick.tick().await;
        let tick_at = Instant::now();
        let loop_dt = tick_at.duration_since(last_tick_at);
        last_tick_at = tick_at;
        if loop_dt > HAPTIC_TIMING_WARN_THRESHOLD {
            let should_warn = last_warn
                .map(|t| tick_at.duration_since(t) >= Duration::from_secs(1))
                .unwrap_or(true);
            if should_warn {
                log::warn!(
                    "RollerCAN SmartKnob: loop tick took {:.2} ms",
                    loop_dt.as_secs_f64() * 1000.0
                );
                last_warn = Some(tick_at);
            }
        }

        let sensor = state.lock().unwrap().sensor();
        let Some(sensor) = sensor else {
            telemetry_phase = telemetry_phase.wrapping_add(1);
            let _ =
                request_realtime_sample(&bus, &send_lock, &state, t0, target_id, telemetry_phase)
                    .await;
            continue;
        };
        let feedback = sensor.feedback.clone();
        let mut tun = *tuning.lock().unwrap();
        let wanted = (*requested_config.lock().unwrap()).min(configs.len() - 1);
        if wanted != active_index {
            config = if configs[wanted].is_custom {
                custom_config.lock().unwrap().clone()
            } else {
                configs[wanted].clone()
            };
            active_index = wanted;
            h.detent.current_position = config.position;
            if config.min_position <= config.max_position {
                h.detent.current_position = h
                    .detent
                    .current_position
                    .clamp(config.min_position, config.max_position);
            }
            h.detent.detent_center = sensor.shaft_angle_rad;
            h.detent.last_idle_start = None;
            h.click.prev_current_position = h.detent.current_position;
            h.click.started_at = None;
            h.click.dir = 1.0;
            last_position_sample = Some((sensor.shaft_angle_rad, sensor.position_at));
            let saved = per_mode_tuning.lock().unwrap()[wanted];
            tun = saved;
            *tuning.lock().unwrap() = saved;
        }

        if config.is_custom && custom_config_dirty.swap(false, Ordering::SeqCst) {
            config = custom_config.lock().unwrap().clone();
            if config.min_position <= config.max_position {
                h.detent.current_position = h
                    .detent
                    .current_position
                    .clamp(config.min_position, config.max_position);
            }
            h.click.prev_current_position = h.detent.current_position;
            tun.p_gain = finite_nonnegative(config.p_gain);
            tun.d_gain = finite_nonnegative(config.d_gain);
            tun.friction_compensation = finite_nonnegative(config.friction_compensation);
            tun.click_torque_nm = finite_nonnegative(config.click_torque_nm);
            if let Some(slot) = per_mode_tuning.lock().unwrap().get_mut(active_index) {
                slot.p_gain = tun.p_gain;
                slot.d_gain = tun.d_gain;
                slot.friction_compensation = tun.friction_compensation;
                slot.click_torque_nm = tun.click_torque_nm;
            }
            *tuning.lock().unwrap() = tun;
        }

        let shaft_angle = sensor.shaft_angle_rad;
        let velocity_rad_s = estimate_velocity_rad_s(
            &mut last_position_sample,
            shaft_angle,
            sensor.position_at,
            sensor.speed_rpm,
        );
        h.angle.shaft_angle = shaft_angle;

        let num_positions = position_count(&config);
        if num_positions != 1 {
            idle_recenter(&mut h.detent, shaft_angle, velocity_rad_s);
        }
        let (angle_to_center, dead_zone_adjustment, out_of_bounds) =
            snap_to_detent(&mut h.detent, shaft_angle, &config, num_positions);
        let haptic_component = compute_haptic_pid(
            &config,
            &tun,
            h.detent.current_position,
            angle_to_center,
            dead_zone_adjustment,
            velocity_rad_s,
            out_of_bounds,
        );
        let min_restoring = compute_min_restoring(
            angle_to_center,
            config.position_width_radians,
            velocity_rad_s,
            num_positions,
        );
        let friction_torque = compute_friction_coulomb(velocity_rad_s, tun.friction_compensation);
        let click_active =
            tun.click_torque_nm > 0.0 && !out_of_bounds && config.detent_positions.is_empty();
        if h.detent.current_position != h.click.prev_current_position {
            h.click.prev_current_position = h.detent.current_position;
            if click_active {
                h.click.started_at = Some(tick_at);
                h.click.dir = -h.click.dir;
            }
        }
        let click_torque =
            compute_click_torque(&mut h.click, tun.click_torque_nm, click_active, tick_at);
        let requested_current_a = if velocity_rad_s.abs() > MAX_VEL_RAD_S {
            0.0
        } else {
            (haptic_component + click_torque + min_restoring + friction_torque)
                .clamp(-tun.torque_limit_nm, tun.torque_limit_nm)
        };
        let current_x100 = effort_to_current_x100(requested_current_a, tun.max_torque_permille);
        let applied_current_a = current_x100 as f64 / MA_X100_PER_AMP;
        let data = param_frame(OD_CURRENT, current_x100);
        if let Err(e) = send_command(
            &bus,
            &send_lock,
            &state,
            t0,
            0x12,
            0,
            0,
            target_id,
            data,
            "haptic current",
        )
        .await
        {
            log::warn!("RollerCAN SmartKnob: current send failed: {e}");
        }
        telemetry_phase = telemetry_phase.wrapping_add(1);
        if let Err(e) =
            request_realtime_sample(&bus, &send_lock, &state, t0, target_id, telemetry_phase).await
        {
            log::warn!("RollerCAN SmartKnob: realtime read failed: {e}");
        }

        let enabled = feedback.state == 1;
        let error = if feedback.state == 2 || feedback.fault_raw != 0 {
            Some(format!("fault 0b{:03b}", feedback.fault_raw))
        } else {
            None
        };
        let mut st = state.lock().unwrap();
        st.knob.running = true;
        st.knob.config_index = active_index;
        st.knob.config = Some(config.clone());
        st.knob.current_position = h.detent.current_position;
        st.knob.min_position = config.min_position;
        st.knob.max_position = config.max_position;
        st.knob.num_positions = if num_positions > 0 { num_positions } else { 0 };
        st.knob.sub_position_unit = h.detent.latest_sub_position_unit;
        st.knob.shaft_angle_rad = shaft_angle;
        st.knob.shaft_velocity_rev_per_s = velocity_rad_s / std::f64::consts::TAU;
        st.knob.applied_torque_nm = applied_current_a;
        st.knob.measured_torque_nm = sensor.current_a.map(|a| a as f32);
        st.knob.at_endstop = out_of_bounds;
        st.knob.node_id = target_id;
        st.knob.online = feedback.age_ms < 500;
        st.knob.enabled = enabled;
        st.knob.driver_temp_c = None;
        st.knob.motor_temp_c = None;
        st.knob.error = error;
        st.knob.strength_scale = tun.strength_scale;
        st.knob.torque_limit_nm = tun.torque_limit_nm;
        st.knob.max_torque_permille = tun.max_torque_permille;
        st.knob.friction_compensation = tun.friction_compensation;
        st.knob.click_torque_nm = tun.click_torque_nm;
        st.knob.p_gain = tun.p_gain;
        st.knob.d_gain = tun.d_gain;
    }

    let _ = send_command(
        &bus,
        &send_lock,
        &state,
        t0,
        0x12,
        0,
        0,
        target_id,
        param_frame(OD_CURRENT, 0),
        "zero current",
    )
    .await;
    state.lock().unwrap().knob.running = false;
    log::info!("RollerCAN SmartKnob: haptic loop stopped");
}

async fn send_command(
    bus: &Arc<dyn CanBus>,
    send_lock: &Arc<tokio::sync::Mutex<()>>,
    state: &Arc<StdMutex<RollerCanState>>,
    t0: Instant,
    cmd: u8,
    param: u8,
    host_id: u8,
    target_id: u8,
    data: [u8; 8],
    note: &'static str,
) -> Result<()> {
    if cmd > 0x1F {
        return Err(anyhow!("RollerCAN command 0x{cmd:02X} exceeds 5 bits"));
    }
    let raw_id =
        ((cmd as u32) << 24) | ((param as u32) << 16) | ((host_id as u32) << 8) | target_id as u32;
    let id = CanId::new_extended(raw_id).map_err(|e| anyhow!("bad RollerCAN id: {e}"))?;
    let frame = CanFrame::new_data(id, &data).map_err(|e| anyhow!("build frame: {e}"))?;
    let _serialized = send_lock.lock().await;
    bus.send(frame)
        .await
        .map_err(|e| anyhow!("send RollerCAN frame: {e}"))?;
    let t_ms = t0.elapsed().as_millis() as u64;
    state
        .lock()
        .unwrap()
        .push_event(t_ms, "tx", raw_id, &data, note.to_string());
    Ok(())
}

async fn request_realtime_sample(
    bus: &Arc<dyn CanBus>,
    send_lock: &Arc<tokio::sync::Mutex<()>>,
    state: &Arc<StdMutex<RollerCanState>>,
    t0: Instant,
    target_id: u8,
    phase: u64,
) -> Result<()> {
    let index = match phase % 8 {
        0 => OD_CURRENT_READBACK,
        4 => OD_SPEED_READBACK,
        _ => OD_POSITION_READBACK,
    };
    send_command(
        bus,
        send_lock,
        state,
        t0,
        0x11,
        0,
        0,
        target_id,
        read_param_frame(index),
        "read realtime",
    )
    .await
}

async fn drain_loop(mut rx: Box<dyn CanRx>, state: Arc<StdMutex<RollerCanState>>, t0: Instant) {
    loop {
        match rx.recv().await {
            Ok(frame) => {
                if !matches!(frame.kind(), FrameKind::Data) {
                    continue;
                }
                let raw = frame.id().raw();
                let data = frame.data();
                let t_ms = t0.elapsed().as_millis() as u64;
                let cmd = ((raw >> 24) & 0x1F) as u8;
                let mut st = state.lock().unwrap();
                match cmd {
                    0x02 if data.len() >= 8 => {
                        let fault = ((raw >> 16) & 0x07) as u8;
                        let feedback = RollerCanFeedback {
                            node_id: ((raw >> 8) & 0xFF) as u8,
                            host_id: (raw & 0xFF) as u8,
                            speed_rpm: i16::from_le_bytes([data[0], data[1]]),
                            position_deg: i16::from_le_bytes([data[2], data[3]]),
                            current_ma: i16::from_le_bytes([data[4], data[5]]),
                            voltage_v: i16::from_le_bytes([data[6], data[7]]),
                            mode: ((raw >> 19) & 0x07) as u8,
                            state: ((raw >> 22) & 0x03) as u8,
                            fault_raw: fault,
                            fault_over_range: (fault & 0b100) != 0,
                            fault_stall: (fault & 0b010) != 0,
                            fault_over_voltage: (fault & 0b001) != 0,
                            age_ms: 0,
                        };
                        st.feedback = Some((feedback, Instant::now()));
                        st.push_event(t_ms, "rx", raw, data, "feedback".to_string());
                    }
                    0x11 | 0x13 if data.len() >= 8 => {
                        update_realtime_param(&mut st, data);
                        st.push_event(t_ms, "rx", raw, data, "param".to_string());
                    }
                    0x17 if data.len() >= 8 => {
                        update_firmware_state(&mut st, data, raw);
                        st.push_event(t_ms, "rx", raw, data, "SmartKnob state push".to_string());
                    }
                    0x18 if data.len() >= 8 => {
                        update_firmware_motion(&mut st, data);
                        st.push_event(t_ms, "rx", raw, data, "SmartKnob motion push".to_string());
                    }
                    0x00 => st.push_event(t_ms, "rx", raw, data, "id response".to_string()),
                    _ => st.push_event(t_ms, "rx", raw, data, format!("cmd 0x{cmd:02X}")),
                }
            }
            Err(CanIoError::Lagged { dropped }) => {
                log::warn!("RollerCAN rx lagged; dropped {dropped} frames");
            }
            Err(CanIoError::Disconnected) => break,
            Err(e) => log::warn!("RollerCAN rx: {e}"),
        }
    }
}

fn update_firmware_state(state: &mut RollerCanState, data: &[u8], raw_id: u32) {
    let mode = data[0] as usize;
    let flags = data[1];
    let position = i32::from_le_bytes([data[2], data[3], data[4], data[5]]);
    let sub_position = i16::from_le_bytes([data[6], data[7]]) as f64 / 10_000.0;
    if mode != 0 || state.knob.config.is_none() {
        state.knob.config = preset_configs().get(mode).cloned();
    }
    if let Some(config) = state.knob.config.as_ref() {
        state.knob.min_position = config.min_position;
        state.knob.max_position = config.max_position;
        state.knob.num_positions = position_count(config);
    }
    state.knob.running = (flags & (1 << 1)) != 0;
    state.knob.enabled = (flags & (1 << 1)) != 0;
    state.knob.at_endstop = (flags & (1 << 2)) != 0;
    state.knob.online = true;
    state.knob.config_index = mode;
    state.knob.current_position = position;
    state.knob.sub_position_unit = sub_position;
    state.knob.node_id = ((raw_id >> 8) & 0xff) as u8;
    state.knob.error = ((flags & (1 << 6)) != 0).then(|| "firmware fault".to_string());
}

fn update_firmware_motion(state: &mut RollerCanState, data: &[u8]) {
    let angle_cdeg = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let commanded_ma = i16::from_le_bytes([data[4], data[5]]);
    let measured_ma = i16::from_le_bytes([data[6], data[7]]);
    state.knob.shaft_angle_rad = (angle_cdeg as f64 / 100.0).to_radians();
    state.knob.applied_torque_nm = commanded_ma as f64 / 1000.0;
    state.knob.measured_torque_nm = Some(measured_ma as f32 / 1000.0);
    state.knob.online = true;
}

struct AngleTracker {
    shaft_angle: f64,
}

struct DetentState {
    detent_center: f64,
    current_position: i32,
    idle_velocity_ewma: f64,
    last_idle_start: Option<Instant>,
    latest_sub_position_unit: f64,
}

struct ClickState {
    prev_current_position: i32,
    started_at: Option<Instant>,
    dir: f64,
}

struct Haptic {
    angle: AngleTracker,
    detent: DetentState,
    click: ClickState,
}

impl Haptic {
    fn new(position: i32) -> Self {
        Self {
            angle: AngleTracker { shaft_angle: 0.0 },
            detent: DetentState {
                detent_center: 0.0,
                current_position: position,
                idle_velocity_ewma: 0.0,
                last_idle_start: None,
                latest_sub_position_unit: 0.0,
            },
            click: ClickState {
                prev_current_position: position,
                started_at: None,
                dir: 1.0,
            },
        }
    }
}

fn sanitize_custom_config(mut c: KnobConfig) -> KnobConfig {
    c.position_width_radians = finite_at_least(c.position_width_radians, 0.001);
    c.p_gain = finite_nonnegative(c.p_gain);
    c.d_gain = finite_nonnegative(c.d_gain);
    c.strength_scale = finite_nonnegative(c.strength_scale);
    c.endstop_strength_unit = finite_nonnegative(c.endstop_strength_unit);
    c.detent_strength_unit = finite_nonnegative(c.detent_strength_unit);
    c.friction_compensation = finite_nonnegative(c.friction_compensation);
    c.click_torque_nm = finite_nonnegative(c.click_torque_nm);
    c.snap_point = finite_or(c.snap_point, 0.55).clamp(0.1, 2.0);
    c.snap_point_bias = finite_or(c.snap_point_bias, 0.0).clamp(-1.0, 1.0);
    if c.min_position <= c.max_position {
        c.position = c.position.clamp(c.min_position, c.max_position);
    }
    c
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn finite_nonnegative(value: f64) -> f64 {
    finite_at_least(value, 0.0)
}

fn finite_at_least(value: f64, min: f64) -> f64 {
    finite_or(value, min).max(min)
}

fn position_count(config: &KnobConfig) -> i32 {
    config
        .max_position
        .checked_sub(config.min_position)
        .and_then(|delta| delta.checked_add(1))
        .filter(|count| *count > 0)
        .unwrap_or(0)
}

fn idle_recenter(detent: &mut DetentState, shaft_angle: f64, velocity_rad_s: f64) {
    detent.idle_velocity_ewma = velocity_rad_s.abs() * IDLE_VELOCITY_EWMA_ALPHA
        + detent.idle_velocity_ewma * (1.0 - IDLE_VELOCITY_EWMA_ALPHA);
    if detent.idle_velocity_ewma > IDLE_VELOCITY_RAD_PER_SEC {
        detent.last_idle_start = None;
    } else if detent.last_idle_start.is_none() {
        detent.last_idle_start = Some(Instant::now());
    }
    if let Some(start) = detent.last_idle_start {
        if start.elapsed() > IDLE_CORRECTION_DELAY
            && (shaft_angle - detent.detent_center).abs() < IDLE_CORRECTION_MAX_ANGLE_RAD
        {
            detent.detent_center = shaft_angle * IDLE_CORRECTION_RATE_ALPHA
                + detent.detent_center * (1.0 - IDLE_CORRECTION_RATE_ALPHA);
        }
    }
}

fn snap_to_detent(
    detent: &mut DetentState,
    shaft_angle: f64,
    config: &KnobConfig,
    num_positions: i32,
) -> (f64, f64, bool) {
    let width = config.position_width_radians;
    let mut angle_to_detent_center = shaft_angle - detent.detent_center;
    let snap_point_radians = width * config.snap_point;
    let bias_radians = width * config.snap_point_bias;
    let snap_dec = snap_point_radians
        + if detent.current_position <= 0 {
            bias_radians
        } else {
            -bias_radians
        };
    let snap_inc = -snap_point_radians
        + if detent.current_position >= 0 {
            -bias_radians
        } else {
            bias_radians
        };

    if angle_to_detent_center > snap_dec
        && (num_positions <= 0 || detent.current_position > config.min_position)
    {
        detent.detent_center += width;
        angle_to_detent_center -= width;
        detent.current_position -= 1;
    } else if angle_to_detent_center < snap_inc
        && (num_positions <= 0 || detent.current_position < config.max_position)
    {
        detent.detent_center -= width;
        angle_to_detent_center += width;
        detent.current_position += 1;
    }

    detent.latest_sub_position_unit = -angle_to_detent_center / width;
    let dead_zone_adjustment = angle_to_detent_center.clamp(
        (-width * DEAD_ZONE_DETENT_PERCENT).max(-DEAD_ZONE_RAD),
        (width * DEAD_ZONE_DETENT_PERCENT).min(DEAD_ZONE_RAD),
    );
    let out_of_bounds = num_positions > 0
        && ((angle_to_detent_center > 0.0 && detent.current_position == config.min_position)
            || (angle_to_detent_center < 0.0 && detent.current_position == config.max_position));
    (angle_to_detent_center, dead_zone_adjustment, out_of_bounds)
}

fn compute_haptic_pid(
    config: &KnobConfig,
    tun: &Tuning,
    current_position: i32,
    angle_to_detent_center: f64,
    dead_zone_adjustment: f64,
    velocity_rad_s: f64,
    out_of_bounds: bool,
) -> f64 {
    if velocity_rad_s.abs() > MAX_VEL_RAD_S {
        return 0.0;
    }
    let mut input = -angle_to_detent_center + dead_zone_adjustment;
    if !out_of_bounds
        && !config.detent_positions.is_empty()
        && !config.detent_positions.contains(&current_position)
    {
        input = 0.0;
    }
    let p_gain = if out_of_bounds {
        config.endstop_strength_unit * 4.0
    } else {
        tun.p_gain
    };
    let pid = (p_gain * input - tun.d_gain * velocity_rad_s).clamp(-PID_LIMIT, PID_LIMIT);
    tun.strength_scale * pid
}

fn compute_min_restoring(
    angle_to_detent_center: f64,
    width: f64,
    velocity_rad_s: f64,
    num_positions: i32,
) -> f64 {
    if num_positions != 1 {
        return 0.0;
    }
    let abs_angle = angle_to_detent_center.abs();
    let dead_zone = (width * DEAD_ZONE_DETENT_PERCENT).min(DEAD_ZONE_RAD);
    if abs_angle > 0.0005
        && abs_angle < dead_zone
        && velocity_rad_s.abs() < IDLE_VELOCITY_RAD_PER_SEC
    {
        (-angle_to_detent_center).signum() * 0.00
    } else {
        0.0
    }
}

fn compute_friction_coulomb(velocity_rad_s: f64, compensation: f64) -> f64 {
    if velocity_rad_s.abs() > IDLE_VELOCITY_RAD_PER_SEC {
        let taper = (velocity_rad_s.abs() / (IDLE_VELOCITY_RAD_PER_SEC * 10.0)).atan()
            / std::f64::consts::FRAC_PI_2;
        compensation * velocity_rad_s.signum() * taper
    } else {
        0.0
    }
}

fn compute_click_torque(
    click: &mut ClickState,
    click_torque_nm: f64,
    click_active: bool,
    now: Instant,
) -> f64 {
    let Some(started_at) = click.started_at else {
        return 0.0;
    };
    if !click_active {
        return 0.0;
    }
    let elapsed = now.duration_since(started_at);
    if elapsed >= CLICK_TOTAL_DURATION {
        click.started_at = None;
        return 0.0;
    }
    let sign = if elapsed < CLICK_PHASE_DURATION {
        click.dir
    } else {
        -click.dir
    };
    sign * click_torque_nm
}

fn update_realtime_param(state: &mut RollerCanState, data: &[u8]) {
    if data.len() < 8 {
        return;
    }
    let index = u16::from_le_bytes([data[0], data[1]]);
    let raw = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let now = Instant::now();
    match index {
        OD_POSITION_READBACK => state.realtime.position_deg = Some((raw as f64 / 100.0, now)),
        OD_SPEED_READBACK => state.realtime.speed_rpm = Some((raw as f64 / 100.0, now)),
        OD_CURRENT_READBACK => state.realtime.current_a = Some((raw as f64 / MA_X100_PER_AMP, now)),
        _ => {}
    }
}

fn estimate_velocity_rad_s(
    last_sample: &mut Option<(f64, Instant)>,
    shaft_angle: f64,
    sample_at: Instant,
    fallback_speed_rpm: Option<f64>,
) -> f64 {
    let fallback = fallback_speed_rpm.unwrap_or(0.0) * std::f64::consts::TAU / 60.0;
    let Some((last_angle, last_at)) = *last_sample else {
        *last_sample = Some((shaft_angle, sample_at));
        return fallback;
    };
    if sample_at <= last_at {
        return fallback;
    }
    let dt = sample_at.duration_since(last_at).as_secs_f64();
    if dt <= 0.0 {
        return fallback;
    }
    let velocity = (shaft_angle - last_angle) / dt;
    *last_sample = Some((shaft_angle, sample_at));
    if velocity.is_finite() {
        velocity
    } else {
        fallback
    }
}

fn effort_to_current_x100(current_a: f64, max_torque_permille: u16) -> i32 {
    if !current_a.is_finite() || current_a.abs() < ROLLER_OUTPUT_DEADBAND_A {
        return 0;
    }
    let safety = (CURRENT_X100_LIMIT as i64 * max_torque_permille.min(1000) as i64 / 1000) as i32;
    (ROLLER_CURRENT_DIRECTION * current_a * MA_X100_PER_AMP)
        .round()
        .clamp(-(safety as f64), safety as f64) as i32
}

fn param_frame(index: u16, value: i32) -> [u8; 8] {
    let mut data = [0u8; 8];
    data[0..2].copy_from_slice(&index.to_le_bytes());
    data[4..8].copy_from_slice(&value.to_le_bytes());
    data
}

fn read_param_frame(index: u16) -> [u8; 8] {
    let mut data = [0u8; 8];
    data[0..2].copy_from_slice(&index.to_le_bytes());
    data
}

fn scaled(value: i32) -> f64 {
    value as f64 / SCALE
}

fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 3);
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roller_presets_are_separate_from_native_smartknob_presets() {
        let roller = preset_configs();
        let native = crate::smartknob::preset_configs();

        assert_eq!(roller.len(), native.len());
        assert!(roller[0].is_custom);
        assert!(roller[0].strength_scale < native[0].strength_scale);
        assert!(roller[0].friction_compensation < native[0].friction_compensation);
    }

    #[test]
    fn tuning_uses_rollercan_config_values_without_extra_scaling() {
        let cfg = preset_configs()
            .into_iter()
            .find(|cfg| cfg.text == "On/off\nStrong detent")
            .expect("rollercan on/off preset");
        let tuning = Tuning::from_config(&cfg);

        assert_eq!(tuning.p_gain, cfg.p_gain);
        assert_eq!(tuning.d_gain, cfg.d_gain);
        assert_eq!(tuning.strength_scale, cfg.strength_scale);
        assert_eq!(tuning.friction_compensation, cfg.friction_compensation);
        assert_eq!(tuning.click_torque_nm, cfg.click_torque_nm);
    }

    #[test]
    fn output_deadband_suppresses_small_current_commands() {
        assert_eq!(
            effort_to_current_x100(ROLLER_OUTPUT_DEADBAND_A * 0.5, 1000),
            0
        );
        assert_eq!(
            effort_to_current_x100(-ROLLER_OUTPUT_DEADBAND_A * 0.5, 1000),
            0
        );
        assert_ne!(
            effort_to_current_x100(ROLLER_OUTPUT_DEADBAND_A * 1.5, 1000),
            0
        );
    }

    #[test]
    fn click_pulse_uses_two_millisecond_phases() {
        let now = Instant::now();
        let mut click = ClickState {
            prev_current_position: 0,
            started_at: Some(now),
            dir: 1.0,
        };

        assert_eq!(
            compute_click_torque(&mut click, 0.5, true, now + Duration::from_millis(1)),
            0.5
        );
        assert_eq!(
            compute_click_torque(&mut click, 0.5, true, now + Duration::from_millis(3)),
            -0.5
        );
        assert_eq!(
            compute_click_torque(&mut click, 0.5, true, now + Duration::from_millis(4)),
            0.0
        );
    }
}
