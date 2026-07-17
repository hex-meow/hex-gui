//! SmartKnob — a haptic rotary-input **Robot Application** (single motor).
//!
//! Port of [scottbez1/smartknob](https://github.com/scottbez1/smartknob)'s
//! firmware feel to a HEX 4310/4342 actuator. SmartKnob turns a brushless
//! gimbal motor into a software-configurable knob: virtual detents, endstops,
//! return-to-center, fine/coarse value dials, etc. The "feel" is pure torque
//! feedback computed from the shaft angle relative to the nearest *detent
//! center*.
//!
//! ## How it maps onto a HEX motor
//!
//! The original firmware runs a torque loop on the motor's own MCU
//! (`motor.move(torque)` in SimpleFOC). Our actuator instead exposes an
//! **uncompressed-MIT** control object (`0x2003`) where, with KP=0, the torque
//! law is `τ = TFF + KD·(VDES − v)`. So we keep smartknob's algorithm **on the
//! host** (it owns the detent state machine and computes the torque exactly as
//! the firmware does) and stream the result as the **torque feed-forward**
//! `0x2003:03` over **RPDO1** at [`CONTROL_HZ`]. The motor just applies the
//! torque we send — no dependence on the motor's internal position frame, which
//! makes multi-turn modes robust. VDES/KD are left at 0 (all damping is done in
//! software, faithfully to the firmware's PID D-term).
//!
//! This reuses the exact PDO plumbing HopeA3 uses (RPDO remap + a high-rate
//! control task streaming one CAN-FD frame), see [`crate::hopea3`].
//!
//! Unlike HopeA3 (fixed 3-motor chassis) the knob is a *single* motor whose
//! node-id the user picks at runtime from the discovered devices.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use can_transport::CanFrame;
use hex_motor::canopen::rpdo_config::{build_rpdo_config_writes, RpdoRecipe};
use hex_motor::canopen::sdo;
use hex_motor::canopen::tpdo_config::TpdoEntry;
use hex_motor::cia402::{Cia402Manager, Logic};
use hex_motor::types::MotorMode;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

// ─────────────────────────── tunables / constants ───────────────────────────

/// Control + haptic loop rate. A knob wants this as high as the TPDO feedback
/// allows; 1 kHz gives crisp detents.
const CONTROL_HZ: u64 = 1000;

/// RPDO1 COB-ID the motor listens on. The motor's default RPDO1 is `0x200+nid`;
/// we keep that (the recipe just rewrites the *mapping*, not the id space).
fn rpdo_cob_id(nid: u8) -> u16 {
    0x200 + nid as u16
}

/// Bytes streamed each tick: TFF `0x2003:03`(f32,4) + KD `0x2003:05`(u16,2) +
/// max torque `0x6072`(u16,2) = 8.
const FRAME_LEN: usize = 8;

/// Uncompressed-MIT control object (`0x2003`). See module docs for the law.
const OD_MIT: u16 = 0x2003;
const MIT_SUB_PDES: u8 = 0x01; // f32 Rev   (position target, unused → 0)
const MIT_SUB_VDES: u8 = 0x02; // f32 Rev/s (velocity target, unused → 0)
const MIT_SUB_TFF: u8 = 0x03; // f32 Nm    (torque feed-forward, streamed)
const MIT_SUB_KP: u8 = 0x04; // u16        (position gain, → 0)
const MIT_SUB_KD: u8 = 0x05; // u16        (velocity gain, streamed; default 0)
const MIT_SUB_FACTOR: u8 = 0x07; // f32     (kp/kd phys→int divisor)
const OD_MAX_TORQUE: u16 = 0x6072; // u16 ‰ of peak

/// Direction sign (the firmware's `SK_INVERT_ROTATION`). Applied to both the
/// read angle and the output torque so the haptic spring stays *stable* either
/// way; flipping it only reverses **which way you turn to increase the value**.
/// (Spring stability itself relies on the motor's FOC calibration aligning
/// torque sign with the sensor sign — flip the motor's zero/direction if it
/// feels anti-stable.)
const DIRECTION: f64 = 1.0;

// Haptic constants, lifted verbatim from the firmware's `motor_task.cpp`.
const DEAD_ZONE_DETENT_PERCENT: f64 = 0.2;
const DEAD_ZONE_RAD: f64 = std::f64::consts::PI / 180.0; // 1°
const IDLE_VELOCITY_EWMA_ALPHA: f64 = 0.001;
const IDLE_VELOCITY_RAD_PER_SEC: f64 = 0.05;
const IDLE_CORRECTION_DELAY: Duration = Duration::from_millis(500);
const IDLE_CORRECTION_MAX_ANGLE_RAD: f64 = 5.0 * std::f64::consts::PI / 180.0;
const IDLE_CORRECTION_RATE_ALPHA: f64 = 0.0005;
/// Above this shaft speed (rad/s) we command zero torque, to avoid a runaway
/// positive-feedback loop (firmware's `fabsf(shaft_velocity) > 60`).
const MAX_VEL_RAD_S: f64 = 60.0;
/// PID output limit in firmware torque units (`PID_velocity.limit = 10`).
const PID_LIMIT: f64 = 10.0;

// ── Haptic click ──
//
// For modes with [`KnobConfig::click_torque_nm`] > 0, we inject a short
// alternating torque burst — a "click" — every time the logical position
// changes.  Direction alternates so clockwise and counter-clockwise
// transitions both feel crisp.  Works for any detent width, from fine (≤3°)
// to coarse.
//
//   Reference: scottbez1/smartknob firmware, motor_task.cpp:
//   "consider eliminating this D factor entirely and just 'play' a
//    hardcoded haptic 'click' (e.g. a quick burst of torque in each
//    direction) whenever the position changes when the detent width is
//    too small for the P factor to work well."

/// Click duration per direction. The loop targets 1 kHz, but Windows/USB
/// scheduling can miss ticks, so the pulse is timed from `Instant` instead of
/// counting loop iterations.
const CLICK_PHASE_DURATION: Duration = Duration::from_millis(5);
const CLICK_TOTAL_DURATION: Duration = Duration::from_millis(10);
const HAPTIC_TIMING_WARN_THRESHOLD: Duration = Duration::from_millis(3);

/// Default live-tunables.
pub const DEFAULT_STRENGTH_SCALE: f64 = 0.15; // Nm per firmware PID unit
pub const DEFAULT_TORQUE_LIMIT_NM: f64 = 2.0; // hard host-side clamp
pub const DEFAULT_MAX_TORQUE_PERMILLE: u16 = 700; // motor-side safety clamp
/// Coulomb friction compensation (Nm). A small torque applied in the
/// direction of motion to cancel the motor's mechanical drag.
pub const DEFAULT_FRICTION_COMPENSATION: f64 = 0.03;

// ───────────────────────────── presets (modes) ──────────────────────────────

const DEG: f64 = std::f64::consts::PI / 180.0;

/// One haptic preset — the equivalent of the firmware's `PB_SmartKnobConfig`.
/// Serialized to the UI so the mode buttons + dial stay in sync with the
/// backend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnobConfig {
    /// Initial logical position when this mode is selected.
    pub position: i32,
    pub min_position: i32,
    /// `max < min` ⇒ unbounded (free spin, no endstops).
    pub max_position: i32,
    /// Angular spacing between detents (radians).
    pub position_width_radians: f64,
    pub detent_strength_unit: f64,
    pub endstop_strength_unit: f64,
    /// Fraction of `position_width` you must pass before snapping (≥0.5).
    pub snap_point: f64,
    pub snap_point_bias: f64,
    /// If non-empty, only these positions have a detent (magnetic detents).
    pub detent_positions: Vec<i32>,
    /// Per-mode default click torque (Nm). When > 0, haptic clicks (biphasic
    /// torque pulses) fire on each detent transition instead of the classic
    /// D-gain damper.  Live-tunable per mode via [`Tuning::click_torque_nm`];
    /// this field seeds the initial value on first mode visit.
    pub click_torque_nm: f64,
    /// Coulomb friction compensation (Nm). A fixed torque in the direction
    /// of motion that helps cancel mechanical drag. Default per-mode;
    /// overridable live via tuning.
    pub friction_compensation: f64,
    /// Overall haptic strength (Nm per firmware PID-output unit). Per-mode
    /// default; overridable live via tuning. Higher = stronger detents /
    /// endstops.
    pub strength_scale: f64,
    /// Per-mode default proportional gain (firmware PID units). This seeds the
    /// live [`Tuning::p_gain`] value; users can then override it per mode.
    pub p_gain: f64,
    /// Per-mode default derivative gain (firmware PID units). This seeds the
    /// live [`Tuning::d_gain`] value; users can then override it per mode.
    pub d_gain: f64,
    /// Two-line label shown on the dial / mode button.
    pub text: String,
    /// Hue (0..255) for the dial accent — mirrors the firmware's LED hue.
    pub led_hue: i32,
    /// `true` for the user-editable custom mode (index 0). Preset modes are
    /// `false`. The frontend uses this to show/hide the custom-config editor.
    #[serde(default)]
    pub is_custom: bool,
}

/// Helper for declaring haptic presets.
///
/// Keep `p_gain` and `d_gain` explicit in each call. They are part of the
/// mode's feel, so downstream users can customize any preset by editing one
/// line instead of reverse-engineering a detent-strength formula.
impl KnobConfig {
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
    ) -> Self {
        Self {
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
}

/// The full demo set, ported 1:1 from `interface_task.cpp`.
pub fn preset_configs() -> Vec<KnobConfig> {
    let p = KnobConfig::preset;
    vec![
        // ── Custom: fully user-editable mode ──
        KnobConfig {
            is_custom: true,
            text: "Custom\nEdit me".into(),
            led_hue: 120,
            max_position: -1,
            position_width_radians: 10.0 * DEG,
            snap_point: 0.55,
            friction_compensation: DEFAULT_FRICTION_COMPENSATION,
            strength_scale: DEFAULT_STRENGTH_SCALE,
            p_gain: 0.0,
            d_gain: 0.0,
            ..p(
                "", 0, 0, -1, 10.0, 0.0, 1.0, 0.55, 0.0, 0.03, 0.35, 0.0, 0.0, 120,
            )
        },
        // ── classic presets ──
        // The two arguments before `led_hue` are this mode's explicit
        // `p_gain` and `d_gain`; edit them per preset to customize feel.
        p(
            "Unbounded\nNo detents",
            0,
            0,
            -1,
            10.0,
            0.0,
            1.0,
            1.1,
            0.0,
            0.09,
            0.15,
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
            0.05,
            0.25,
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
            1.0,
            1.1,
            0.0,
            0.08,
            DEFAULT_STRENGTH_SCALE,
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
            1.0,
            1.0,
            0.55,
            0.0,
            0.05,
            0.25,
            4.0,
            0.11,
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
            DEFAULT_FRICTION_COMPENSATION,
            0.05,
            0.04,
            0.0002,
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
            0.02,
            0.3,
            0.0,
            0.0,
            219,
        ),
        KnobConfig {
            click_torque_nm: 0.37,
            ..p(
                "Fine values\nWith detents",
                127,
                0,
                255,
                1.0,
                1.0,
                1.0,
                1.1,
                0.0,
                DEFAULT_FRICTION_COMPENSATION,
                0.25,
                4.0,
                0.0,
                25,
            )
        },
        p(
            "Coarse values\nStrong detents",
            0,
            0,
            31,
            8.225806452,
            2.5,
            1.0,
            1.1,
            0.0,
            0.08,
            0.75,
            10.0,
            0.05,
            200,
        ),
        KnobConfig {
            click_torque_nm: 1.20,
            ..p(
                "Coarse values\nWeak detents",
                0,
                0,
                31,
                8.225806452,
                0.2,
                1.0,
                1.1,
                0.0,
                0.02,
                1.5,
                0.8,
                0.0,
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
                0.01,
                0.8,
                10.0,
                0.0,
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
            0.02,
            0.15,
            4.0,
            0.02,
            157,
        ),
    ]
}

// ───────────────────────────── shared state ─────────────────────────────────

/// Live, host-tunable parameters (independent of the selected mode).
#[derive(Clone, Copy)]
struct Tuning {
    /// Proportional gain (firmware PID units). Seeded from
    /// `KnobConfig::p_gain`, then live-tunable per mode.
    p_gain: f64,
    /// Derivative gain (firmware PID units). Seeded from
    /// `KnobConfig::d_gain`, then live-tunable per mode.
    d_gain: f64,
    /// Nm per firmware PID-output unit (overall haptic strength).
    strength_scale: f64,
    /// Hard host-side torque clamp (Nm).
    torque_limit_nm: f64,
    /// Motor-side `0x6072` safety clamp (‰ of peak).
    max_torque_permille: u16,
    /// Coulomb friction compensation (Nm). Added in the direction of motion
    /// to cancel mechanical drag.
    friction_compensation: f64,
    /// Haptic click torque (Nm). When > 0 and the active config has
    /// `use_click = true`, a biphasic torque pulse fires on each detent
    /// transition.  Live-tunable; seeded from [`DEFAULT_CLICK_TORQUE_NM`]
    /// when the config first enables clicks.
    click_torque_nm: f64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            p_gain: 0.0,
            d_gain: 0.0,
            strength_scale: DEFAULT_STRENGTH_SCALE,
            torque_limit_nm: DEFAULT_TORQUE_LIMIT_NM,
            max_torque_permille: DEFAULT_MAX_TORQUE_PERMILLE,
            friction_compensation: 0.0,
            click_torque_nm: 0.0,
        }
    }
}

impl Tuning {
    fn from_config(config: &KnobConfig) -> Self {
        Self {
            p_gain: config.p_gain,
            d_gain: config.d_gain,
            strength_scale: config.strength_scale,
            torque_limit_nm: DEFAULT_TORQUE_LIMIT_NM,
            max_torque_permille: DEFAULT_MAX_TORQUE_PERMILLE,
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

/// Smallest detent width we accept from a user-supplied config. A
/// non-positive width would invert the dead-zone clamp bounds in
/// [`snap_to_detent`] (`f64::clamp` panics when min > max — killing the
/// haptic loop) and divide by zero in the sub-position math.
const MIN_POSITION_WIDTH_RAD: f64 = 0.001;

/// Clamp a user-supplied custom config to values the 1 kHz loop can safely
/// consume. Negative gains would flip feedback signs (positive velocity
/// feedback → self-accelerating knob). `max_position < min_position` is left
/// alone — that is the documented "unbounded" convention.
fn sanitize_custom_config(mut c: KnobConfig) -> KnobConfig {
    c.position_width_radians = finite_at_least(c.position_width_radians, MIN_POSITION_WIDTH_RAD);
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

/// Snapshot handed to the frontend each poll.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SmartKnobState {
    pub running: bool,
    /// Index into [`preset_configs`] currently active.
    pub config_index: usize,
    /// The active config (so the UI dial can draw detents/bounds).
    pub config: Option<KnobConfig>,
    /// Current logical position (detent index).
    pub current_position: i32,
    pub min_position: i32,
    pub max_position: i32,
    /// `0` = unbounded.
    pub num_positions: i32,
    /// Smooth pointer between detents: `-angle_to_detent_center / width`,
    /// in (−snap..+snap). Add to `current_position` for a continuous value.
    pub sub_position_unit: f64,
    /// Continuous shaft angle since start (radians) and its rev equivalent.
    pub shaft_angle_rad: f64,
    pub shaft_velocity_rev_per_s: f64,
    /// Torque we are commanding this tick (Nm) and what the motor reports.
    pub applied_torque_nm: f64,
    pub measured_torque_nm: Option<f32>,
    pub at_endstop: bool,
    // Motor health.
    pub node_id: u8,
    pub online: bool,
    pub enabled: bool,
    pub driver_temp_c: Option<f32>,
    pub motor_temp_c: Option<f32>,
    pub error: Option<String>,
    // Tuning echo.
    pub strength_scale: f64,
    pub torque_limit_nm: f64,
    pub max_torque_permille: u16,
    pub friction_compensation: f64,
    pub click_torque_nm: f64,
    pub p_gain: f64,
    pub d_gain: f64,
}

// ───────────────────────────── the driver ───────────────────────────────────

/// A running SmartKnob: owns the high-rate haptic loop for one motor.
pub struct SmartKnob {
    node_id: u8,
    /// Index of the requested config; the loop picks it up and applies it.
    requested_config: Arc<StdMutex<usize>>,
    tuning: Arc<StdMutex<Tuning>>,
    /// Per-mode tuning overrides — one entry per preset.  When the user adjusts
    /// the sliders we write into this slot; on mode switch we restore from it.
    /// Initialised from each preset's defaults, so a never-touched mode keeps
    /// its stock feel.
    per_mode_tuning: Arc<StdMutex<Vec<Tuning>>>,
    state: Arc<StdMutex<SmartKnobState>>,
    /// Live-editable KnobConfig for the custom mode (index 0). The haptic loop
    /// reads from this mutex when custom mode is active; the dirty flag triggers
    /// on-the-fly re-apply without a full mode switch.
    custom_config: Arc<StdMutex<KnobConfig>>,
    custom_config_dirty: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

/// How many times to attempt motor init before giving up (init can be flaky).
const INIT_ATTEMPTS: u8 = 3;

impl SmartKnob {
    /// Initialize the chosen motor for MIT torque-stream control and start the
    /// haptic loop. The manager must already be connected with heartbeat on.
    pub async fn start(
        mgr: Arc<Cia402Manager>,
        nid: u8,
        config_index: usize,
        shutdown_requested: &AtomicBool,
    ) -> anyhow::Result<Self> {
        let configs = preset_configs();
        let config_index = config_index.min(configs.len() - 1);
        let bus = mgr.bus();
        let sdo_timeout = Some(mgr.options().sdo_timeout);
        // Seed live tunables from the selected preset so the sliders show the
        // preset's defaults on start.
        let tuning = Tuning::from_config(&configs[config_index]);

        // Per-motor init, retried — same recovery dance as HopeA3.
        let mut last_err = None;
        for attempt in 1..=INIT_ATTEMPTS {
            ensure_startup_allowed(&mgr, nid, shutdown_requested).await?;
            match init_motor(
                &mgr,
                &bus,
                sdo_timeout,
                nid,
                tuning.max_torque_permille,
                shutdown_requested,
            )
            .await
            {
                Ok(()) => {
                    log::info!("SmartKnob: motor 0x{nid:02X} ready (attempt {attempt})");
                    last_err = None;
                    break;
                }
                Err(e) => {
                    log::warn!(
                        "SmartKnob: init 0x{nid:02X} attempt {attempt}/{INIT_ATTEMPTS}: {e}"
                    );
                    if shutdown_requested.load(Ordering::SeqCst) {
                        let _ = mgr.disable(nid).await;
                        return Err(anyhow::anyhow!(
                            "SmartKnob startup cancelled by application shutdown while initializing 0x{nid:02X}: {e:#}"
                        ));
                    }
                    last_err = Some(e);
                    let _ = mgr.clear_error(nid).await;
                    ensure_startup_allowed(&mgr, nid, shutdown_requested).await?;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    ensure_startup_allowed(&mgr, nid, shutdown_requested).await?;
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e.context(format!(
                "motor 0x{nid:02X} failed after {INIT_ATTEMPTS} attempts"
            )));
        }
        ensure_startup_allowed(&mgr, nid, shutdown_requested).await?;

        // Per-mode tuning, seeded from each preset's defaults.
        let per_mode_tuning: Vec<Tuning> = configs.iter().map(Tuning::from_config).collect();

        let per_mode_tuning = Arc::new(StdMutex::new(per_mode_tuning));

        // Custom mode config: seed from the placeholder at index 0.
        let custom_config = Arc::new(StdMutex::new(configs[0].clone()));
        let custom_config_dirty = Arc::new(AtomicBool::new(false));

        let requested_config = Arc::new(StdMutex::new(config_index));
        let tuning = Arc::new(StdMutex::new(tuning));
        let state = Arc::new(StdMutex::new(SmartKnobState {
            node_id: nid,
            config_index,
            ..Default::default()
        }));
        let running = Arc::new(AtomicBool::new(true));

        let task = {
            let mgr = mgr.clone();
            let bus = bus.clone();
            let requested_config = requested_config.clone();
            let tuning = tuning.clone();
            let per_mode_tuning = per_mode_tuning.clone();
            let state = state.clone();
            let running = running.clone();
            let custom_config = custom_config.clone();
            let custom_config_dirty = custom_config_dirty.clone();
            tokio::spawn(async move {
                haptic_loop(
                    mgr,
                    bus,
                    nid,
                    requested_config,
                    tuning,
                    per_mode_tuning,
                    state,
                    running,
                    custom_config,
                    custom_config_dirty,
                )
                .await;
            })
        };

        Ok(Self {
            node_id: nid,
            requested_config,
            tuning,
            per_mode_tuning,
            state,
            custom_config,
            custom_config_dirty,
            running,
            task,
        })
    }

    /// Switch haptic mode (the front-panel "mode" button that stands in for the
    /// missing press sensor). Clamped to the preset range.
    pub fn set_config(&self, index: usize) {
        let max = preset_configs().len().saturating_sub(1);
        *self
            .requested_config
            .lock()
            .expect("requested_config poisoned") = index.min(max);
    }

    /// Replace the custom mode config (index 0) with a new one. The haptic
    /// loop picks it up on its next tick and re-applies it on the fly if
    /// custom mode is currently active. The config is sanitized first — the
    /// 1 kHz loop must never see values that invert clamp bounds or feedback
    /// signs (see [`sanitize_custom_config`]).
    pub fn set_custom_config(&self, config: KnobConfig) {
        *self.custom_config.lock().expect("custom_config poisoned") =
            sanitize_custom_config(config);
        self.custom_config_dirty.store(true, Ordering::SeqCst);
    }

    /// Update live haptic tunables.  Persists into the per-mode slot for the
    /// currently-active config so the tuned values survive a mode round-trip.
    pub fn set_tuning(
        &self,
        p_gain: f64,
        d_gain: f64,
        strength_scale: f64,
        torque_limit_nm: f64,
        max_torque_permille: u16,
        friction_compensation: f64,
        click_torque_nm: f64,
    ) {
        let clamped = Tuning {
            p_gain,
            d_gain,
            strength_scale,
            torque_limit_nm,
            max_torque_permille: max_torque_permille.min(1000),
            friction_compensation,
            click_torque_nm,
        }
        .sanitized();
        *self.tuning.lock().expect("tuning poisoned") = clamped;
        // Persist into the per-mode slot for the current config.
        let idx = *self
            .requested_config
            .lock()
            .expect("requested_config poisoned");
        if let Some(slot) = self
            .per_mode_tuning
            .lock()
            .expect("per_mode_tuning poisoned")
            .get_mut(idx)
        {
            *slot = clamped;
        }
    }

    pub fn state(&self) -> SmartKnobState {
        self.state.lock().expect("state poisoned").clone()
    }

    pub fn node_id(&self) -> u8 {
        self.node_id
    }

    /// Stop the loop, zero torque and disable the motor.
    pub async fn stop(self, mgr: &Cia402Manager) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.task.await;
        if let Err(e) = mgr.disable(self.node_id).await {
            log::warn!("SmartKnob: disable 0x{:02X} on stop: {e}", self.node_id);
        }
    }
}

/// Best-effort fault clear (so the user can recover without leaving the panel).
pub async fn clear_error(mgr: &Cia402Manager, nid: u8) {
    if let Err(e) = mgr.clear_error(nid).await {
        log::warn!("SmartKnob: clear_error 0x{nid:02X}: {e}");
    }
}

/// Initialize one motor: CiA402 init, remap RPDO1 to the MIT torque-stream
/// frame, zero the static MIT params (PDES/VDES/KP — we only stream TFF), set
/// max torque, switch to MIT mode (which enables).
async fn init_motor(
    mgr: &Cia402Manager,
    bus: &Arc<dyn can_transport::CanBus>,
    sdo_timeout: Option<Duration>,
    nid: u8,
    max_torque: u16,
    shutdown_requested: &AtomicBool,
) -> anyhow::Result<()> {
    mgr.initialize(nid)
        .await
        .map_err(|e| anyhow::anyhow!("initialize: {e}"))?;
    ensure_startup_allowed(mgr, nid, shutdown_requested).await?;

    let recipe = RpdoRecipe {
        rpdo_index: 0,
        cob_id: rpdo_cob_id(nid),
        entries: vec![
            TpdoEntry {
                index: OD_MIT,
                subindex: MIT_SUB_TFF,
                bit_len: 32,
            }, // torque FF
            TpdoEntry {
                index: OD_MIT,
                subindex: MIT_SUB_KD,
                bit_len: 16,
            }, // KD
            TpdoEntry {
                index: OD_MAX_TORQUE,
                subindex: 0,
                bit_len: 16,
            }, // max torque
        ],
        transmission_type: 255,
    };
    let writes =
        build_rpdo_config_writes(&recipe).map_err(|e| anyhow::anyhow!("rpdo recipe: {e}"))?;
    for w in &writes {
        sdo::download(&**bus, nid, w.index, w.subindex, &w.data, sdo_timeout)
            .await
            .map_err(|e| anyhow::anyhow!("rpdo write {:04X}:{}: {e}", w.index, w.subindex))?;
        ensure_startup_allowed(mgr, nid, shutdown_requested).await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        ensure_startup_allowed(mgr, nid, shutdown_requested).await?;
    }

    // Zero everything but TFF: PDES, VDES, KP. (KD is streamed, default 0.)
    sdo::download_f32(&**bus, nid, OD_MIT, MIT_SUB_PDES, 0.0, sdo_timeout)
        .await
        .map_err(|e| anyhow::anyhow!("zero PDES: {e}"))?;
    ensure_startup_allowed(mgr, nid, shutdown_requested).await?;
    sdo::download_f32(&**bus, nid, OD_MIT, MIT_SUB_VDES, 0.0, sdo_timeout)
        .await
        .map_err(|e| anyhow::anyhow!("zero VDES: {e}"))?;
    ensure_startup_allowed(mgr, nid, shutdown_requested).await?;
    sdo::download_u16(&**bus, nid, OD_MIT, MIT_SUB_KP, 0, sdo_timeout)
        .await
        .map_err(|e| anyhow::anyhow!("zero KP: {e}"))?;
    ensure_startup_allowed(mgr, nid, shutdown_requested).await?;

    mgr.set_max_torque(nid, max_torque)
        .await
        .map_err(|e| anyhow::anyhow!("set_max_torque: {e}"))?;
    ensure_startup_allowed(mgr, nid, shutdown_requested).await?;
    mgr.set_mode(nid, MotorMode::Mit)
        .await
        .map_err(|e| anyhow::anyhow!("set_mode MIT: {e}"))?;
    ensure_startup_allowed(mgr, nid, shutdown_requested).await?;
    Ok(())
}

/// Abort a slow CANopen startup as soon as the native close handler asks for
/// shutdown. A disable is attempted here because any completed initialization
/// step may already have transitioned the drive toward operation enabled.
async fn ensure_startup_allowed(
    mgr: &Cia402Manager,
    nid: u8,
    shutdown_requested: &AtomicBool,
) -> anyhow::Result<()> {
    if !shutdown_requested.load(Ordering::SeqCst) {
        return Ok(());
    }
    if let Err(error) = mgr.disable(nid).await {
        log::warn!("SmartKnob: disable 0x{nid:02X} while cancelling startup: {error}");
    }
    Err(anyhow::anyhow!(
        "SmartKnob startup cancelled by application shutdown"
    ))
}

// ───────────────────────────── the haptic loop ──────────────────────────────

// ─────────────────────── haptic sub-state structs ─────────────────────────

/// Continuous shaft-angle tracking (unwrapping across revolutions).
#[derive(Clone)]
struct AngleTracker {
    /// Continuous (unwrapped) shaft angle, radians.
    shaft_angle: f64,
    /// Continuous (unwrapped) revolution accumulator.
    accum_rev: f64,
    /// Last *wrapped* sensor reading (revolutions), for delta unwrapping.
    prev_raw_rev: Option<f64>,
}

/// Detent snap state — which detent centre the knob is currently latched to.
#[derive(Clone)]
struct DetentState {
    /// Detent center the knob is currently snapped to, radians.
    detent_center: f64,
    current_position: i32,
    /// Smoothed |velocity| for idle detection (rad/s).
    idle_velocity_ewma: f64,
    last_idle_start: Option<Instant>,
    latest_sub_position_unit: f64,
}

/// Haptic click pulse state (biphasic torque burst on fine-detent transitions).
#[derive(Clone)]
struct ClickState {
    /// Logical position at the *previous* tick, used to detect detent
    /// transitions and trigger a click.
    prev_current_position: i32,
    /// Wall-clock start of the current click sequence.
    started_at: Option<Instant>,
    /// Sign of the first phase of the *next* click (±1).  Flips after each
    /// triggered click so alternating detent transitions feel symmetric.
    dir: f64,
}

/// Mutable per-tick haptic state (the firmware's locals, hoisted into a struct).
struct Haptic {
    angle: AngleTracker,
    detent: DetentState,
    click: ClickState,
}

// ──────────────────── haptic pure-computation helpers ──────────────────────

/// Unwrap a wrapped sensor revolution reading into a continuous shaft angle.
/// Returns `(new_accum_rev, new_prev_raw_rev, shaft_angle_rad)`.
fn unwrap_shaft_angle(
    prev_raw_rev: Option<f64>,
    accum_rev: f64,
    raw_rev: f64,
) -> (f64, Option<f64>, f64) {
    let new_accum = match prev_raw_rev {
        None => raw_rev,
        Some(prev) => {
            let mut d = raw_rev - prev;
            if d > 0.5 {
                d -= 1.0;
            } else if d < -0.5 {
                d += 1.0;
            }
            accum_rev + d
        }
    };
    let shaft_angle = DIRECTION * new_accum * std::f64::consts::TAU;
    (new_accum, Some(raw_rev), shaft_angle)
}

/// Idle re-centering: slowly drift the detent centre toward the current shaft
/// angle when the knob is stationary so it doesn't feel "stuck" off-centre.
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

/// Snap-to-detent state machine (firmware logic, verbatim).
/// Returns `(angle_to_detent_center, dead_zone_adjustment, out_of_bounds)`.
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

/// Compute the haptic (PID) torque component.
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
        return 0.0; // runaway guard
    }
    let mut input = -angle_to_detent_center + dead_zone_adjustment;
    // Magnetic detents: no spring unless at a listed position.
    if !out_of_bounds && !config.detent_positions.is_empty() {
        if !config.detent_positions.contains(&current_position) {
            input = 0.0;
        }
    }
    let p_gain = if out_of_bounds {
        config.endstop_strength_unit * 4.0
    } else {
        tun.p_gain
    };
    let pid = (p_gain * input - tun.d_gain * velocity_rad_s).clamp(-PID_LIMIT, PID_LIMIT);
    tun.strength_scale * pid
}

/// Minimum restoring torque for single-detent (return-to-center) modes.
/// Pushes through stiction to bring the knob to true centre.
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

/// Fixed Coulomb friction compensation with smooth `atan` taper.
fn compute_friction_coulomb(velocity_rad_s: f64, compensation: f64) -> f64 {
    if velocity_rad_s.abs() > IDLE_VELOCITY_RAD_PER_SEC {
        let taper = (velocity_rad_s.abs() / (IDLE_VELOCITY_RAD_PER_SEC * 10.0)).atan()
            / std::f64::consts::FRAC_PI_2;
        compensation * velocity_rad_s.signum() * taper
    } else {
        0.0
    }
}

/// Compute haptic click torque pulse.
///
/// A biphasic (alternating-direction) torque burst that fires on each detent
/// transition. Direction flips so clockwise and counter-clockwise transitions
/// both feel crisp.
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

/// Build the 8-byte RPDO1 frame: `TFF(f32) + KD(u16=0) + max torque(u16)`.
fn build_rpdo_frame(torque_nm: f64, max_torque_permille: u16) -> [u8; FRAME_LEN] {
    let torque_cmd = (DIRECTION * torque_nm) as f32;
    let mut data = [0u8; FRAME_LEN];
    data[0..4].copy_from_slice(&torque_cmd.to_le_bytes());
    data[4..6].copy_from_slice(&0u16.to_le_bytes());
    data[6..8].copy_from_slice(&max_torque_permille.to_le_bytes());
    data
}

async fn haptic_loop(
    mgr: Arc<Cia402Manager>,
    bus: Arc<dyn can_transport::CanBus>,
    nid: u8,
    requested_config: Arc<StdMutex<usize>>,
    tuning: Arc<StdMutex<Tuning>>,
    per_mode_tuning: Arc<StdMutex<Vec<Tuning>>>,
    state: Arc<StdMutex<SmartKnobState>>,
    running: Arc<AtomicBool>,
    custom_config: Arc<StdMutex<KnobConfig>>,
    custom_config_dirty: Arc<AtomicBool>,
) {
    let configs = preset_configs();
    let period = Duration::from_micros(1_000_000 / CONTROL_HZ);
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut active_index = usize::MAX; // force first-tick config apply
    let mut config = configs[0].clone();
    let mut h = Haptic {
        angle: AngleTracker {
            shaft_angle: 0.0,
            accum_rev: 0.0,
            prev_raw_rev: None,
        },
        detent: DetentState {
            detent_center: 0.0,
            current_position: 0,
            idle_velocity_ewma: 0.0,
            last_idle_start: None,
            latest_sub_position_unit: 0.0,
        },
        click: ClickState {
            prev_current_position: configs[0].position,
            started_at: None,
            dir: 1.0,
        },
    };

    // Rate-limit RPDO send warnings to once per second (avoids log spam at 1 kHz).
    let mut last_rpdo_warn: Option<Instant> = None;
    let mut last_timing_warn: Option<Instant> = None;
    let mut last_tick_at = Instant::now();

    while running.load(Ordering::SeqCst) {
        tick.tick().await;
        let tick_at = Instant::now();
        let loop_dt = tick_at.duration_since(last_tick_at);
        last_tick_at = tick_at;
        if loop_dt > HAPTIC_TIMING_WARN_THRESHOLD {
            let should_warn = last_timing_warn
                .map(|t| tick_at.duration_since(t) >= Duration::from_secs(1))
                .unwrap_or(true);
            if should_warn {
                log::warn!(
                    "SmartKnob: haptic loop tick took {:.2} ms",
                    loop_dt.as_secs_f64() * 1000.0
                );
                last_timing_warn = Some(tick_at);
            }
        }
        let mut tun = *tuning.lock().expect("tuning poisoned");

        // ── 1. read motor feedback & unwrap to a continuous shaft angle ──
        let ls = mgr.status(nid);
        let m = &ls.measurements;
        let raw_rev = m.position_rev.unwrap_or(0.0) as f64;
        let (new_accum, new_prev, shaft_angle) =
            unwrap_shaft_angle(h.angle.prev_raw_rev, h.angle.accum_rev, raw_rev);
        h.angle.accum_rev = new_accum;
        h.angle.prev_raw_rev = new_prev;
        h.angle.shaft_angle = shaft_angle;
        let velocity_rad_s =
            DIRECTION * m.velocity_rev_per_s.unwrap_or(0.0) as f64 * std::f64::consts::TAU;

        let (enabled, error) = match ls.logic.as_ref() {
            Some(Logic::Enabled(_)) => (true, None),
            Some(Logic::Error { kind, raw_code }) => {
                (false, Some(format!("{kind:?} (0x{raw_code:04X})")))
            }
            _ => (false, None),
        };

        // ── 2. apply a pending mode switch ──
        let wanted =
            (*requested_config.lock().expect("requested_config poisoned")).min(configs.len() - 1);
        if wanted != active_index {
            // Custom mode reads from the live-editable custom_config; presets
            // read from the static preset list.
            config = if configs[wanted].is_custom {
                custom_config
                    .lock()
                    .expect("custom_config poisoned")
                    .clone()
            } else {
                configs[wanted].clone()
            };
            active_index = wanted;
            // Recenter on the new mode (firmware: position change + detent recenter).
            h.detent.current_position = config.position;
            if config.min_position <= config.max_position {
                h.detent.current_position = h
                    .detent
                    .current_position
                    .clamp(config.min_position, config.max_position);
            }
            // Place the detent centre at the current shaft angle so the knob
            // doesn't jump, biased by the configured sub-position (0 here).
            h.detent.detent_center = h.angle.shaft_angle;
            h.detent.last_idle_start = None;
            h.click.prev_current_position = h.detent.current_position;
            h.click.started_at = None;
            h.click.dir = 1.0;
            // Restore per-mode tuning (user-tweaked values, or preset defaults on
            // first visit).  Also write them back into the shared Tuning so the
            // frontend sees the restored values on the next poll.
            let saved = {
                let pmt = per_mode_tuning.lock().expect("per_mode_tuning poisoned");
                pmt[wanted]
            };
            tun.strength_scale = saved.strength_scale;
            tun.torque_limit_nm = saved.torque_limit_nm;
            tun.max_torque_permille = saved.max_torque_permille;
            tun.friction_compensation = saved.friction_compensation;
            tun.click_torque_nm = saved.click_torque_nm;
            tun.p_gain = saved.p_gain;
            tun.d_gain = saved.d_gain;
            *tuning.lock().expect("tuning poisoned") = saved;
        }

        // ── 2b. pick up custom-config edits on the fly (no mode switch, no
        //        detent recentering — avoids a knob jump while editing live).
        if config.is_custom && custom_config_dirty.swap(false, Ordering::SeqCst) {
            config = custom_config
                .lock()
                .expect("custom_config poisoned")
                .clone();
            // Keep the logical position inside the (possibly narrowed) new
            // range — the mode-switch path above clamps too. Without this,
            // shrinking max_position leaves current_position out of range
            // with no endstop torque anywhere in between (out_of_bounds only
            // tests equality with the bounds).
            if config.min_position <= config.max_position {
                h.detent.current_position = h
                    .detent
                    .current_position
                    .clamp(config.min_position, config.max_position);
            }
            h.click.prev_current_position = h.detent.current_position;
            // Propagate explicit config fields to the active tuning so the
            // haptic feel changes immediately, with the same non-negative
            // clamps as set_tuning (a negative d_gain would be positive
            // velocity feedback). strength_scale is deliberately NOT
            // propagated — the user controls it independently via the
            // Tuning — Feel slider.
            tun.p_gain = finite_nonnegative(config.p_gain);
            tun.d_gain = finite_nonnegative(config.d_gain);
            tun.friction_compensation = finite_nonnegative(config.friction_compensation);
            tun.click_torque_nm = finite_nonnegative(config.click_torque_nm);
            // Persist into per-mode tuning + shared mutex so the frontend
            // sees the updated values on the next poll.
            if let Some(slot) = per_mode_tuning
                .lock()
                .expect("per_mode_tuning poisoned")
                .get_mut(active_index)
            {
                slot.p_gain = tun.p_gain;
                slot.d_gain = tun.d_gain;
                slot.friction_compensation = tun.friction_compensation;
                slot.click_torque_nm = tun.click_torque_nm;
            }
            *tuning.lock().expect("tuning poisoned") = tun;
        }

        let num_positions = position_count(&config);

        // ── 3. idle re-centering (skip for single-detent / return-to-center modes) ──
        if num_positions != 1 {
            idle_recenter(&mut h.detent, h.angle.shaft_angle, velocity_rad_s);
        }

        // ── 4. snap-to-detent state machine ──
        let (angle_to_detent_center, dead_zone_adjustment, out_of_bounds) =
            snap_to_detent(&mut h.detent, h.angle.shaft_angle, &config, num_positions);

        // ── 5. haptic PID torque ──
        let haptic_component = compute_haptic_pid(
            &config,
            &tun,
            h.detent.current_position,
            angle_to_detent_center,
            dead_zone_adjustment,
            velocity_rad_s,
            out_of_bounds,
        );

        // ── 6. minimum restoring torque (return-to-center single-detent) ──
        let min_restoring = compute_min_restoring(
            angle_to_detent_center,
            config.position_width_radians,
            velocity_rad_s,
            num_positions,
        );

        // ── 7. friction compensation (Coulomb) ──
        let friction_torque = compute_friction_coulomb(velocity_rad_s, tun.friction_compensation);

        // ── 8. haptic click pulse ──
        let click_active =
            tun.click_torque_nm > 0.0 && !out_of_bounds && config.detent_positions.is_empty();
        // Track detent transitions unconditionally — if `prev` were only
        // updated while clicks are active, raising the click slider after
        // rotating in a click-less mode would fire a burst into a stationary
        // knob for a transition that happened long ago. Only *arm* the burst
        // when clicks are active.
        if h.detent.current_position != h.click.prev_current_position {
            h.click.prev_current_position = h.detent.current_position;
            if click_active {
                h.click.started_at = Some(tick_at);
                h.click.dir = -h.click.dir;
            }
        }
        let click_torque =
            compute_click_torque(&mut h.click, tun.click_torque_nm, click_active, tick_at);

        // ── 9. clamp total torque ──
        // Runaway guard on the TOTAL command, not just the PID term: above
        // MAX_VEL_RAD_S every component must go silent. Friction compensation
        // points along the direction of motion and click bursts keep firing
        // as detents fly past — left unguarded they would actively sustain
        // the very runaway this guard exists to stop.
        let torque_nm = if velocity_rad_s.abs() > MAX_VEL_RAD_S {
            0.0
        } else {
            (haptic_component + click_torque + min_restoring + friction_torque)
                .clamp(-tun.torque_limit_nm, tun.torque_limit_nm)
        };

        // ── 10. stream RPDO frame ──
        let data = build_rpdo_frame(torque_nm, tun.max_torque_permille);
        match CanFrame::new_fd(rpdo_cob_id(nid), &data, true) {
            Ok(frame) => {
                let send_started = Instant::now();
                if let Err(e) = bus.send(frame).await {
                    // Rate-limit: warn at most once per second to avoid log spam.
                    let now = Instant::now();
                    let should_warn = last_rpdo_warn
                        .map(|t| now.duration_since(t) >= Duration::from_secs(1))
                        .unwrap_or(true);
                    if should_warn {
                        log::warn!("SmartKnob: RPDO send failed: {e}");
                        last_rpdo_warn = Some(now);
                    }
                }
                let send_dt = send_started.elapsed();
                if send_dt > HAPTIC_TIMING_WARN_THRESHOLD {
                    let now = Instant::now();
                    let should_warn = last_timing_warn
                        .map(|t| now.duration_since(t) >= Duration::from_secs(1))
                        .unwrap_or(true);
                    if should_warn {
                        log::warn!(
                            "SmartKnob: RPDO send took {:.2} ms",
                            send_dt.as_secs_f64() * 1000.0
                        );
                        last_timing_warn = Some(now);
                    }
                }
            }
            Err(e) => log::error!("SmartKnob: build RPDO frame: {e}"),
        }

        // ── 11. publish state snapshot for the frontend ──
        {
            let mut s = state.lock().expect("state poisoned");
            s.running = true;
            s.config_index = active_index;
            s.config = Some(config.clone());
            s.current_position = h.detent.current_position;
            s.min_position = config.min_position;
            s.max_position = config.max_position;
            s.num_positions = if num_positions > 0 { num_positions } else { 0 };
            s.sub_position_unit = h.detent.latest_sub_position_unit;
            s.shaft_angle_rad = h.angle.shaft_angle;
            s.shaft_velocity_rev_per_s = velocity_rad_s / std::f64::consts::TAU;
            s.applied_torque_nm = torque_nm;
            s.measured_torque_nm = m.torque_nm;
            s.at_endstop = out_of_bounds;
            s.node_id = nid;
            s.online = ls.connection.online;
            s.enabled = enabled;
            s.driver_temp_c = m.driver_temp_c;
            s.motor_temp_c = m.motor_temp_c;
            s.error = error;
            s.strength_scale = tun.strength_scale;
            s.torque_limit_nm = tun.torque_limit_nm;
            s.max_torque_permille = tun.max_torque_permille;
            s.friction_compensation = tun.friction_compensation;
            s.click_torque_nm = tun.click_torque_nm;
            s.p_gain = tun.p_gain;
            s.d_gain = tun.d_gain;
        }
    }

    state.lock().expect("state poisoned").running = false;
    log::info!("SmartKnob: haptic loop stopped");
}

// ─────────────────────────────── unit tests ─────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── unwrap_shaft_angle ──

    #[test]
    fn unwrap_first_reading_sets_accum_to_raw() {
        let (accum, prev, angle) = unwrap_shaft_angle(None, 0.0, 0.25);
        assert_eq!(accum, 0.25);
        assert_eq!(prev, Some(0.25));
        assert!((angle - DIRECTION * 0.25 * std::f64::consts::TAU).abs() < 1e-10);
    }

    #[test]
    fn unwrap_no_wrap_around() {
        let (accum, prev, _) = unwrap_shaft_angle(Some(0.1), 0.1, 0.15);
        assert!((accum - 0.15).abs() < 1e-10);
        assert_eq!(prev, Some(0.15));
    }

    #[test]
    fn unwrap_forward_wrap() {
        // raw goes from 0.9 → 0.1: forward crossing the ±0.5 boundary
        let (accum, _, _) = unwrap_shaft_angle(Some(0.9), 10.9, 0.1);
        // d = 0.1 - 0.9 = -0.8 < -0.5 → d += 1.0 → 0.2
        // new_accum = 10.9 + 0.2 = 11.1
        assert!((accum - 11.1).abs() < 1e-10);
    }

    #[test]
    fn unwrap_backward_wrap() {
        // raw goes from 0.1 → 0.9: backward crossing the ±0.5 boundary
        let (accum, _, _) = unwrap_shaft_angle(Some(0.1), 10.1, 0.9);
        // d = 0.9 - 0.1 = 0.8 > 0.5 → d -= 1.0 → -0.2
        // new_accum = 10.1 + (-0.2) = 9.9
        assert!((accum - 9.9).abs() < 1e-10);
    }

    // ── snap_to_detent ──

    fn test_config() -> KnobConfig {
        KnobConfig {
            position: 0,
            min_position: 0,
            max_position: 10,
            position_width_radians: 10.0 * DEG,
            detent_strength_unit: 1.0,
            endstop_strength_unit: 1.0,
            snap_point: 0.55,
            snap_point_bias: 0.0,
            ..Default::default()
        }
    }

    #[test]
    fn snap_no_movement_near_center() {
        let mut d = DetentState {
            detent_center: 0.0,
            current_position: 5,
            idle_velocity_ewma: 0.0,
            last_idle_start: None,
            latest_sub_position_unit: 0.0,
        };
        let (angle_to_center, _dz, out_of_bounds) =
            snap_to_detent(&mut d, 0.01, &test_config(), 11);
        // Small angle, within snap — no transition should occur.
        assert_eq!(d.current_position, 5);
        assert!(!out_of_bounds);
        // angle_to_center should be small
        assert!(angle_to_center.abs() < 0.02);
    }

    #[test]
    fn snap_forward_one_detent() {
        let width = 10.0 * DEG;
        let mut d = DetentState {
            detent_center: 0.0,
            current_position: 5,
            idle_velocity_ewma: 0.0,
            last_idle_start: None,
            latest_sub_position_unit: 0.0,
        };
        // Shaft angle beyond snap_inc threshold
        let shaft = d.detent_center - width * 0.6; // past snap_inc = -0.55*width
        let (_angle_to_center, _dz, _out_of_bounds) =
            snap_to_detent(&mut d, shaft, &test_config(), 11);
        assert_eq!(d.current_position, 6); // moved one detent forward
    }

    #[test]
    fn snap_at_min_boundary_no_transition() {
        let width = 10.0 * DEG;
        let mut d = DetentState {
            detent_center: 0.0,
            current_position: 0, // at min
            idle_velocity_ewma: 0.0,
            last_idle_start: None,
            latest_sub_position_unit: 0.0,
        };
        // Try to go below min (snap_dec direction)
        let shaft = d.detent_center + width * 0.6; // past snap_dec = 0.55*width
        let (_angle_to_center, _dz, out_of_bounds) =
            snap_to_detent(&mut d, shaft, &test_config(), 11);
        // Should not change position (at boundary), and should be out_of_bounds.
        assert_eq!(d.current_position, 0);
        assert!(out_of_bounds);
    }

    // ── compute_haptic_pid ──

    #[test]
    fn pid_runaway_guard() {
        let cfg = test_config();
        let tun = Tuning {
            p_gain: 1.0,
            d_gain: 0.1,
            strength_scale: 1.0,
            ..Default::default()
        };
        let result = compute_haptic_pid(&cfg, &tun, 5, 0.0, 0.0, MAX_VEL_RAD_S + 1.0, false);
        assert_eq!(result, 0.0);
    }

    #[test]
    fn pid_zero_at_center() {
        let cfg = test_config();
        let tun = Tuning {
            p_gain: 1.0,
            d_gain: 0.0,
            strength_scale: 1.0,
            ..Default::default()
        };
        // At centre with no velocity → input is 0 (angle 0 + dead_zone 0)
        let result = compute_haptic_pid(&cfg, &tun, 5, 0.0, 0.0, 0.0, false);
        assert_eq!(result, 0.0);
    }

    #[test]
    fn pid_spring_restoring() {
        let width = 10.0 * DEG;
        let cfg = KnobConfig {
            position_width_radians: width,
            detent_strength_unit: 1.0,
            endstop_strength_unit: 2.0,
            ..test_config()
        };
        let tun = Tuning {
            p_gain: 4.0,
            d_gain: 0.0,
            strength_scale: 0.5,
            ..Default::default()
        };
        // Off centre: angle_to_center = 0.01, dead_zone should clamp small values
        let result = compute_haptic_pid(&cfg, &tun, 5, 0.01, 0.0, 0.0, false);
        // input = -0.01, pid = 4.0 * (-0.01) = -0.04, scaled: 0.5 * (-0.04) = -0.02
        assert!((result - (-0.02)).abs() < 1e-10);
    }

    #[test]
    fn pid_endstop_uses_config_strength() {
        let cfg = test_config();
        // Even if tun.p_gain is small, endstop uses config.endstop_strength_unit * 4.
        let tun = Tuning {
            p_gain: 0.1,
            d_gain: 0.0,
            strength_scale: 1.0,
            ..Default::default()
        };
        let result = compute_haptic_pid(&cfg, &tun, 5, 0.1, 0.0, 0.0, true);
        // out_of_bounds → p_gain = config.endstop_strength_unit * 4.0 = 4.0
        // input = -0.1, pid = 4.0 * (-0.1) = -0.4
        assert!((result - (-0.4)).abs() < 1e-10);
    }

    #[test]
    fn pid_magnetic_detent_no_spring_off_position() {
        let cfg = KnobConfig {
            detent_positions: vec![2, 10, 21],
            ..test_config()
        };
        let tun = Tuning {
            p_gain: 4.0,
            d_gain: 0.0,
            strength_scale: 1.0,
            ..Default::default()
        };
        // current_position=5 is NOT in the magnetic list → input becomes 0.
        let result = compute_haptic_pid(&cfg, &tun, 5, 0.01, 0.0, 0.0, false);
        assert_eq!(result, 0.0);
    }

    #[test]
    fn pid_magnetic_detent_spring_on_position() {
        let cfg = KnobConfig {
            detent_positions: vec![2, 10, 21],
            ..test_config()
        };
        let tun = Tuning {
            p_gain: 4.0,
            d_gain: 0.0,
            strength_scale: 1.0,
            ..Default::default()
        };
        // current_position=10 IS in the magnetic list → normal spring.
        let result = compute_haptic_pid(&cfg, &tun, 10, 0.01, 0.0, 0.0, false);
        assert!((result - (-0.04)).abs() < 1e-10);
    }

    // ── compute_friction_coulomb ──

    #[test]
    fn friction_below_idle_is_zero() {
        let result = compute_friction_coulomb(IDLE_VELOCITY_RAD_PER_SEC * 0.5, 0.1);
        assert_eq!(result, 0.0);
    }

    #[test]
    fn friction_positive_direction() {
        let result = compute_friction_coulomb(1.0, 0.1);
        assert!(result > 0.0); // positive velocity → positive friction
    }

    #[test]
    fn friction_negative_direction() {
        let result = compute_friction_coulomb(-1.0, 0.1);
        assert!(result < 0.0); // negative velocity → negative friction
    }

    // ── compute_click_torque ──

    #[test]
    fn click_inactive_returns_zero() {
        let now = Instant::now();
        let mut c = ClickState {
            prev_current_position: 0,
            started_at: Some(now),
            dir: 1.0,
        };
        let result = compute_click_torque(&mut c, 0.5, false, now);
        assert_eq!(result, 0.0);
        // State should not change when inactive.
        assert!(c.started_at.is_some());
    }

    #[test]
    fn click_first_phase_returns_dir_torque() {
        let now = Instant::now();
        let mut c = ClickState {
            prev_current_position: 0,
            started_at: Some(now),
            dir: 1.0,
        };
        // ticks: 6 → 5, phase = 5/5 = 1, sign = +dir = +1.0
        let result = compute_click_torque(&mut c, 0.5, true, now + Duration::from_millis(2));
        assert_eq!(result, 0.5);
        assert!(c.started_at.is_some());
    }

    #[test]
    fn click_second_phase_reverses_sign() {
        // Phase 0: dir=-1.0, ticks_remaining=4 → 3, phase = 3/5 = 0, sign = -(-1.0) = +1.0
        let now = Instant::now();
        let mut c = ClickState {
            prev_current_position: 0,
            started_at: Some(now),
            dir: -1.0,
        };
        let result = compute_click_torque(&mut c, 0.3, true, now + Duration::from_millis(7));
        assert_eq!(result, 0.3); // +0.3 (reversed from dir=-1.0)
    }

    #[test]
    fn click_exhausted_returns_zero() {
        let now = Instant::now();
        let mut c = ClickState {
            prev_current_position: 0,
            started_at: Some(now),
            dir: 1.0,
        };
        let result = compute_click_torque(&mut c, 0.5, true, now + Duration::from_millis(10));
        assert_eq!(result, 0.0);
        assert!(c.started_at.is_none());
    }

    // ── build_rpdo_frame ──

    #[test]
    fn rpdo_frame_structure() {
        let frame = build_rpdo_frame(1.5, 700);
        assert_eq!(frame.len(), 8);
        // Bytes 0-3: torque_cmd f32 LE = DIRECTION * 1.5
        let torque_bytes: [u8; 4] = frame[0..4].try_into().unwrap();
        let torque: f32 = f32::from_le_bytes(torque_bytes);
        assert!((torque - DIRECTION as f32 * 1.5).abs() < 1e-6);
        // Bytes 4-5: KD = 0u16
        assert_eq!(frame[4], 0);
        assert_eq!(frame[5], 0);
        // Bytes 6-7: max_torque_permille = 700u16 LE
        assert_eq!(u16::from_le_bytes([frame[6], frame[7]]), 700);
    }

    // ── preset_configs ──

    #[test]
    fn presets_are_well_formed() {
        let configs = preset_configs();
        assert!(!configs.is_empty());
        for (i, c) in configs.iter().enumerate() {
            assert!(!c.text.is_empty(), "preset {i} has empty text");
            assert!(
                c.position_width_radians > 0.0,
                "preset {i} has non-positive width"
            );
            assert!(
                c.strength_scale >= 0.0,
                "preset {i} has negative strength_scale"
            );
        }
    }

    #[test]
    fn custom_config_sanitizer_rejects_non_finite_and_negative_values() {
        let cfg = KnobConfig {
            position_width_radians: f64::NAN,
            p_gain: -1.0,
            d_gain: f64::INFINITY,
            strength_scale: -0.5,
            detent_strength_unit: -2.0,
            endstop_strength_unit: -3.0,
            friction_compensation: f64::NEG_INFINITY,
            click_torque_nm: -0.1,
            snap_point: f64::NAN,
            snap_point_bias: f64::INFINITY,
            ..test_config()
        };

        let sanitized = sanitize_custom_config(cfg);

        assert_eq!(sanitized.position_width_radians, MIN_POSITION_WIDTH_RAD);
        assert_eq!(sanitized.p_gain, 0.0);
        assert_eq!(sanitized.d_gain, 0.0);
        assert_eq!(sanitized.strength_scale, 0.0);
        assert_eq!(sanitized.detent_strength_unit, 0.0);
        assert_eq!(sanitized.endstop_strength_unit, 0.0);
        assert_eq!(sanitized.friction_compensation, 0.0);
        assert_eq!(sanitized.click_torque_nm, 0.0);
        assert_eq!(sanitized.snap_point, 0.55);
        assert_eq!(sanitized.snap_point_bias, 0.0);
    }

    #[test]
    fn position_count_handles_unbounded_and_extreme_ranges() {
        let unbounded = KnobConfig {
            min_position: 10,
            max_position: 0,
            ..test_config()
        };
        assert_eq!(position_count(&unbounded), 0);

        let extreme = KnobConfig {
            min_position: i32::MIN,
            max_position: i32::MAX,
            ..test_config()
        };
        assert_eq!(position_count(&extreme), 0);
    }
}
