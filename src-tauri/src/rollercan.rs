//! RollerCAN firmware-owned SmartKnob session.
//!
//! Unit RollerCAN is not a HEX/CiA402 motor. The default device speaks a
//! proprietary CAN 2.0 29-bit extended-frame protocol at 1 Mbps, with default
//! node id `0xA8`. The STM32 owns the 1 kHz haptic loop; this module sends mode
//! and tuning parameters and decodes the firmware's unsolicited telemetry.
//!
//! There is deliberately no host-side haptic control loop here. The remaining
//! code handles discovery, protocol I/O, verified configuration transactions,
//! telemetry pairing, and fail-safe enable/disable around the firmware loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use can_transport::{CanBus, CanFilter, CanFrame, CanId, CanIoError, CanRx, FrameKind};
use tokio::task::JoinHandle;

pub use crate::smartknob::KnobConfig;

// Firmware presets use 0.45 A by default; 1.2 A is only the hard capability
// ceiling exposed by the profile. Keep the host mirror aligned so switching
// to an as-yet-untuned mode does not make the UI report a looser limit.
const ROLLER_DEFAULT_CURRENT_LIMIT_A: f64 = 0.45;
pub(crate) const ROLLER_HARD_CURRENT_LIMIT_A: f64 = 1.2;
const DEG: f64 = std::f64::consts::PI / 180.0;

const OD_RELEASE_PROTECTION: u16 = 0x7003;
const OD_ENABLE: u16 = 0x7004;
const OD_RUN_MODE: u16 = 0x7005;
const OD_CURRENT: u16 = 0x7006;

const RC_CMD_SET_CONFIG: u16 = 0x8001;
const RC_TELEMETRY_ENABLE: u16 = 0x8002;
const RC_TELEMETRY_RATE_HZ: u16 = 0x8003;
const RC_TELEMETRY_HOST_ID: u16 = 0x8004;
const RC_MODE_COUNT: u16 = 0x8005;
const RC_PROTOCOL_VERSION: u16 = 0x8006;
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
const ROLLER_DEFAULT_NODE_ID: u8 = 0xA8;
const ROLLER_MODE_COUNT: i32 = 12;
const ROLLER_PROTOCOL_VERSION: i32 = 1;
const ROLLER_DEFAULT_TELEMETRY_RATE_HZ: u16 = 50;
const ROLLER_MAX_TELEMETRY_RATE_HZ: u16 = 100;
const DISCOVERY_STEP: Duration = Duration::from_micros(15_625); // 64 ids/s
const KNOWN_PING_PERIOD: Duration = Duration::from_secs(1);
const IDENTITY_PROBE_PERIOD: Duration = Duration::from_millis(200);
const PAIR_TTL: Duration = Duration::from_millis(500);
const PARAM_READ_TIMEOUT: Duration = Duration::from_millis(120);
const PARAM_READ_ATTEMPTS: usize = 3;
const ENABLE_STATUS_TIMEOUT: Duration = Duration::from_millis(200);
const VERIFY_SCALED_TOLERANCE: i32 = 2;

fn ensure_start_not_cancelled(shutdown_requested: Option<&AtomicBool>) -> Result<()> {
    if shutdown_requested.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err(anyhow!(
            "RollerCAN SmartKnob startup cancelled by application shutdown"
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct PendingTelemetry {
    first_seen: Instant,
    state: Option<[u8; 8]>,
    motion: Option<[u8; 8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RollerCanStatus {
    fault: u8,
    mode: u8,
    state: u8,
}

impl PendingTelemetry {
    fn new(now: Instant) -> Self {
        Self {
            first_seen: now,
            state: None,
            motion: None,
        }
    }
}

struct RollerCanNode {
    knob: crate::smartknob::SmartKnobState,
    last_seen: Instant,
    last_telemetry: Option<Instant>,
    /// Start of the current period in which firmware telemetry is expected.
    /// This distinguishes a short first-sample grace period from a lost
    /// stream; once expected, ping responses cannot keep the node online.
    telemetry_expected_since: Option<Instant>,
    mode_count: Option<i32>,
    protocol_version: Option<i32>,
    telemetry_enabled: bool,
    telemetry_rate_hz: u16,
    /// True once the host has explicitly configured/read telemetry state.
    /// A final in-flight telemetry pair after disabling must not flip the
    /// setting back on and make an otherwise healthy pinged node look stale.
    telemetry_configured: bool,
    missed_pings: u8,
    /// Prevent repeatedly discarding a freshly re-read identity while the
    /// same telemetry-stale presence remains offline.
    identity_invalidated_while_offline: bool,
    pending: HashMap<u8, PendingTelemetry>,
}

impl RollerCanNode {
    fn new(node_id: u8, now: Instant) -> Self {
        Self {
            knob: crate::smartknob::SmartKnobState {
                node_id,
                ..Default::default()
            },
            last_seen: now,
            last_telemetry: None,
            telemetry_expected_since: None,
            mode_count: None,
            protocol_version: None,
            telemetry_enabled: true,
            telemetry_rate_hz: ROLLER_DEFAULT_TELEMETRY_RATE_HZ,
            telemetry_configured: false,
            missed_pings: 0,
            identity_invalidated_while_offline: false,
            pending: HashMap::new(),
        }
    }

    fn confirmed(&self) -> bool {
        self.mode_count == Some(ROLLER_MODE_COUNT)
            && self.protocol_version == Some(ROLLER_PROTOCOL_VERSION)
    }

    fn online_at(&self, now: Instant) -> bool {
        if self.telemetry_enabled {
            let period_ms = 1000_u64 / u64::from(self.telemetry_rate_hz.max(1));
            let timeout = Duration::from_millis((period_ms * 3).max(500));
            if let Some(expected_since) = self.telemetry_expected_since {
                let freshness_anchor = self
                    .last_telemetry
                    .filter(|received_at| *received_at >= expected_since)
                    .unwrap_or(expected_since);
                now.saturating_duration_since(freshness_anchor) <= timeout
            } else if self.telemetry_configured {
                // Configured-on telemetry always establishes an expectation;
                // retain a conservative fail-closed fallback if state is ever
                // observed between the two field updates.
                false
            } else {
                // Before 0x8002 has been read, allow identity probing/ping a
                // bounded discovery grace while waiting for first telemetry.
                self.missed_pings < 3
                    && now.saturating_duration_since(self.last_seen) <= Duration::from_secs(3)
            }
        } else {
            self.missed_pings < 3
                && now.saturating_duration_since(self.last_seen) <= Duration::from_secs(3)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollerCanDiscoveredDevice {
    pub node_id: u8,
    pub online: bool,
}

#[derive(Default)]
struct RollerCanState {
    knob: crate::smartknob::SmartKnobState,
    nodes: HashMap<u8, RollerCanNode>,
    selected_node: Option<u8>,
    /// One-shot readback waiters registered before a 0x11 request is sent.
    /// The protocol has no request sequence, so all operations for one target
    /// are issued serially and a matching `(source,index)` response completes
    /// the current waiter.
    pending_reads: HashMap<(u8, u8, u16), Vec<tokio::sync::oneshot::Sender<i32>>>,
    /// The firmware answers each 0x12 write with generic cmd=0x02 feedback.
    /// It is not a parameter ACK, but the final enable transaction uses it to
    /// prove the driver entered MODE_DIAL/RUNNING with no fault bits set.
    pending_status: HashMap<(u8, u8), Vec<tokio::sync::oneshot::Sender<RollerCanStatus>>>,
}

impl RollerCanState {
    fn devices(&self, now: Instant) -> Vec<RollerCanDiscoveredDevice> {
        let mut devices: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.confirmed())
            .map(|(&node_id, node)| RollerCanDiscoveredDevice {
                node_id,
                online: node.online_at(now),
            })
            .collect();
        devices.sort_by_key(|device| device.node_id);
        devices
    }

    fn knob_for(&self, node_id: u8) -> crate::smartknob::SmartKnobState {
        let mut knob = self
            .nodes
            .get(&node_id)
            .map(|node| node.knob.clone())
            .unwrap_or_else(|| crate::smartknob::SmartKnobState {
                node_id,
                ..Default::default()
            });
        knob.online = self
            .nodes
            .get(&node_id)
            .map(|node| node.confirmed() && node.online_at(Instant::now()))
            .unwrap_or(false);
        knob
    }

    fn telemetry_for(&self, node_id: u8) -> (bool, u16) {
        self.nodes
            .get(&node_id)
            .map(|node| (node.telemetry_enabled, node.telemetry_rate_hz))
            .unwrap_or((true, ROLLER_DEFAULT_TELEMETRY_RATE_HZ))
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
            torque_limit_nm: finite_nonnegative(self.torque_limit_nm)
                .min(ROLLER_HARD_CURRENT_LIMIT_A),
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
    discovery_task: JoinHandle<()>,
    running: Arc<AtomicBool>,
    requested_config: Arc<StdMutex<usize>>,
    tuning: Arc<StdMutex<Tuning>>,
    per_mode_tuning: Arc<StdMutex<Vec<Tuning>>>,
    custom_config: Arc<StdMutex<KnobConfig>>,
    target_id: StdMutex<Option<u8>>,
    /// Non-zero response destination tag for correlating 0x11 readbacks and
    /// the final enable's cmd=0x02 status with their exact requests.
    next_response_host_id: AtomicU8,
    send_lock: Arc<tokio::sync::Mutex<()>>,
}

impl RollerCanSession {
    /// Attach the RollerCAN monitor to the manager-owned bus. This is the
    /// normal product path: a physical adapter is opened only once.
    pub async fn attach(bus: Arc<dyn CanBus>) -> Result<Self> {
        let rx = bus
            .subscribe(CanFilter::pass_all_extended())
            .await
            .map_err(|e| anyhow!("subscribe RollerCAN extended frames: {e}"))?;
        let state = Arc::new(StdMutex::new(RollerCanState::default()));
        let rx_task = tokio::spawn(drain_loop(rx, state.clone()));
        let send_lock = Arc::new(tokio::sync::Mutex::new(()));
        let discovery_task = tokio::spawn(discovery_loop(
            bus.clone(),
            state.clone(),
            send_lock.clone(),
        ));
        let configs = preset_configs();
        let per_mode_tuning = configs.iter().map(Tuning::from_config).collect();
        let tuning = Tuning::from_config(&configs[0]);
        Ok(Self {
            bus,
            state,
            rx_task,
            discovery_task,
            running: Arc::new(AtomicBool::new(false)),
            requested_config: Arc::new(StdMutex::new(0)),
            tuning: Arc::new(StdMutex::new(tuning)),
            per_mode_tuning: Arc::new(StdMutex::new(per_mode_tuning)),
            custom_config: Arc::new(StdMutex::new(configs[0].clone())),
            target_id: StdMutex::new(None),
            next_response_host_id: AtomicU8::new(1),
            send_lock,
        })
    }

    pub fn devices(&self) -> Vec<RollerCanDiscoveredDevice> {
        self.state.lock().unwrap().devices(Instant::now())
    }

    pub fn has_online_device(&self) -> bool {
        self.devices().iter().any(|device| device.online)
    }

    pub fn may_be_active(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn knob_state(&self, node_id: u8) -> crate::smartknob::SmartKnobState {
        self.state.lock().unwrap().knob_for(node_id)
    }

    pub fn telemetry_settings(&self, node_id: u8) -> (bool, u16) {
        self.state.lock().unwrap().telemetry_for(node_id)
    }

    /// Immediately probe one node (manual-ID fallback). Discovery continues
    /// in the background even if this call returns before the replies arrive.
    pub async fn probe(&self, node_id: u8) -> Result<()> {
        probe_node(&self.bus, &self.send_lock, node_id).await
    }

    /// Stop background transmitters before a shutdown disable transaction.
    /// The receive task deliberately stays alive so nonce-correlated status
    /// and parameter readback can still confirm that output is disabled.
    pub fn begin_shutdown(&self) {
        self.discovery_task.abort();
    }

    pub async fn stop(self) {
        self.discovery_task.abort();
        let target = *self.target_id.lock().unwrap();
        if let Some(target) = target {
            if !self.best_effort_disable(target).await {
                log::error!(
                    "RollerCAN 0x{target:02X}: disable could not be confirmed during session teardown; disconnect device power before handling it"
                );
            }
        }
        let _ = self.discovery_task.await;
        self.rx_task.abort();
        let _ = self.rx_task.await;
        log::info!("RollerCAN SmartKnob disconnected");
    }

    /// Execute firmware-owned SmartKnob startup as one ordered transaction.
    /// Every setting is read back while output remains disabled; final enable
    /// additionally requires fault-free MODE_DIAL/RUNNING feedback. Any send,
    /// timeout, or mismatch triggers zero-current + disable rollback.
    pub async fn start_knob(
        &self,
        index: usize,
        target_id: u8,
        custom: Option<KnobConfig>,
        requested_tuning: Option<crate::unified_smartknob::SmartKnobTuning>,
        telemetry: crate::unified_smartknob::SmartKnobTelemetry,
        shutdown_requested: Option<&AtomicBool>,
    ) -> Result<()> {
        let configs = preset_configs();
        let config = configs
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("invalid RollerCAN SmartKnob mode {index}"))?;
        let custom = custom.map(sanitize_custom_config);
        let requested_tuning = requested_tuning.unwrap_or_else(|| {
            let t = Tuning::from_config(custom.as_ref().unwrap_or(&config));
            crate::unified_smartknob::SmartKnobTuning {
                p_gain: t.p_gain,
                d_gain: t.d_gain,
                strength_scale: t.strength_scale,
                effort_limit: t.torque_limit_nm,
                max_output_permille: t.max_torque_permille,
                friction_compensation: t.friction_compensation,
                click_effort: t.click_torque_nm,
            }
        });
        let internal_tuning = Tuning {
            p_gain: requested_tuning.p_gain,
            d_gain: requested_tuning.d_gain,
            strength_scale: requested_tuning.strength_scale,
            torque_limit_nm: requested_tuning.effort_limit,
            max_torque_permille: requested_tuning.max_output_permille,
            friction_compensation: requested_tuning.friction_compensation,
            click_torque_nm: requested_tuning.click_effort,
        }
        .sanitized();
        let tuning = crate::unified_smartknob::SmartKnobTuning {
            p_gain: internal_tuning.p_gain,
            d_gain: internal_tuning.d_gain,
            strength_scale: internal_tuning.strength_scale,
            effort_limit: internal_tuning.torque_limit_nm,
            max_output_permille: internal_tuning.max_torque_permille,
            friction_compensation: internal_tuning.friction_compensation,
            click_effort: internal_tuning.click_torque_nm,
        };
        let telemetry = crate::unified_smartknob::SmartKnobTelemetry {
            rate_hz: telemetry.rate_hz.clamp(1, ROLLER_MAX_TELEMETRY_RATE_HZ),
            ..telemetry
        };

        *self.requested_config.lock().unwrap() = index;
        *self.target_id.lock().unwrap() = Some(target_id);
        self.state.lock().unwrap().selected_node = Some(target_id);

        let result: Result<()> = async {
            ensure_start_not_cancelled(shutdown_requested)?;
            self.write_param_verified(
                target_id,
                OD_ENABLE,
                0,
                0,
                "disable before configure",
                shutdown_requested,
            )
            .await?;
            self.write_param_verified(
                target_id,
                OD_CURRENT,
                0,
                0,
                "zero current",
                shutdown_requested,
            )
            .await?;
            self.write_param_verified(
                target_id,
                RC_CMD_SET_CONFIG,
                index as i32,
                0,
                "select firmware preset",
                shutdown_requested,
            )
            .await?;
            if let Some(config) = custom.as_ref() {
                self.write_custom_config_verified(target_id, config, shutdown_requested)
                    .await?;
            }
            self.write_tuning_verified(target_id, tuning, shutdown_requested)
                .await?;
            self.write_param_verified(
                target_id,
                RC_TELEMETRY_HOST_ID,
                0,
                0,
                "telemetry host",
                shutdown_requested,
            )
            .await?;
            self.write_param_verified(
                target_id,
                RC_TELEMETRY_RATE_HZ,
                i32::from(telemetry.rate_hz),
                0,
                "telemetry rate",
                shutdown_requested,
            )
            .await?;
            self.write_param_verified(
                target_id,
                RC_TELEMETRY_ENABLE,
                i32::from(telemetry.enabled),
                0,
                if telemetry.enabled {
                    "telemetry on"
                } else {
                    "telemetry off"
                },
                shutdown_requested,
            )
            .await?;
            self.write_param_verified(
                target_id,
                OD_RUN_MODE,
                4,
                0,
                "firmware SmartKnob mode",
                shutdown_requested,
            )
            .await?;
            let enable_confirmed_at = self
                .write_enable_verified(target_id, shutdown_requested)
                .await?;
            if telemetry.enabled {
                self.wait_for_enabled_telemetry(
                    target_id,
                    enable_confirmed_at,
                    telemetry.rate_hz,
                    shutdown_requested,
                )
                .await?;
            }
            ensure_start_not_cancelled(shutdown_requested)?;
            Ok(())
        }
        .await;

        if let Err(error) = result {
            let disabled = self.best_effort_disable(target_id).await;
            if disabled {
                return Err(anyhow!(
                    "RollerCAN SmartKnob start failed on 0x{target_id:02X}; motor rolled back disabled: {error:#}"
                ));
            }
            return Err(anyhow!(
                "RollerCAN SmartKnob start failed on 0x{target_id:02X}; rollback disable also failed and the motor may still be active. Retry Stop or disconnect power: {error:#}"
            ));
        }

        self.running.store(true, Ordering::SeqCst);
        let config = custom.unwrap_or(config);
        *self.tuning.lock().unwrap() = internal_tuning;
        if let Some(slot) = self.per_mode_tuning.lock().unwrap().get_mut(index) {
            *slot = internal_tuning;
        }
        if index == 0 {
            *self.custom_config.lock().unwrap() = config.clone();
        }
        let mut state = self.state.lock().unwrap();
        let node = state
            .nodes
            .entry(target_id)
            .or_insert_with(|| RollerCanNode::new(target_id, Instant::now()));
        node.telemetry_enabled = telemetry.enabled;
        node.telemetry_rate_hz = telemetry.rate_hz;
        node.telemetry_configured = true;
        if telemetry.enabled {
            node.telemetry_expected_since
                .get_or_insert_with(Instant::now);
        } else {
            node.telemetry_expected_since = None;
        }
        node.knob.running = true;
        node.knob.enabled = true;
        node.knob.config_index = index;
        node.knob.config = Some(config.clone());
        node.knob.current_position = config.position;
        node.knob.min_position = config.min_position;
        node.knob.max_position = config.max_position;
        node.knob.num_positions = position_count(&config);
        node.knob.node_id = target_id;
        node.knob.strength_scale = internal_tuning.strength_scale;
        node.knob.torque_limit_nm = internal_tuning.torque_limit_nm;
        node.knob.max_torque_permille = internal_tuning.max_torque_permille;
        node.knob.friction_compensation = internal_tuning.friction_compensation;
        node.knob.click_torque_nm = internal_tuning.click_torque_nm;
        node.knob.p_gain = internal_tuning.p_gain;
        node.knob.d_gain = internal_tuning.d_gain;
        state.knob = node.knob.clone();
        drop(state);
        log::info!("RollerCAN firmware SmartKnob started on 0x{target_id:02X}");
        Ok(())
    }

    /// Attempt the safety rollback and report whether firmware confirmed the
    /// disable. A successful transport send alone is insufficient: it only
    /// proves the adapter accepted the frame, not that the motor processed it.
    /// On any status/readback failure, keep the session visibly active so the
    /// unified command layer can retain a Stop target for retries.
    async fn best_effort_disable(&self, target_id: u8) -> bool {
        if let Err(error) = self
            .write_param_raw(0, target_id, OD_CURRENT, 0, "rollback zero current")
            .await
        {
            log::warn!("RollerCAN 0x{target_id:02X}: zero-current rollback failed: {error}");
        }
        let disable = self
            .write_disable_verified(target_id, "rollback disable")
            .await;
        if let Err(error) = &disable {
            log::warn!("RollerCAN 0x{target_id:02X}: disable rollback failed: {error}");
        }
        let safely_disabled = disable.is_ok();
        if safely_disabled {
            *self.target_id.lock().unwrap() = None;
        }
        self.running.store(!safely_disabled, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        if let Some(node) = state.nodes.get_mut(&target_id) {
            node.knob.running = !safely_disabled;
            node.knob.enabled = !safely_disabled;
            if !safely_disabled {
                node.knob.error = Some("rollback disable failed; motor may still be active".into());
            }
        }
        state.knob.running = !safely_disabled;
        state.knob.enabled = !safely_disabled;
        if !safely_disabled {
            state.knob.error = Some("rollback disable failed; motor may still be active".into());
        }
        safely_disabled
    }

    pub async fn stop_motor(&self, _host_id: u8, target_id: u8) -> Result<()> {
        let target = self.target_id.lock().unwrap().unwrap_or(target_id);
        let zero = self
            .write_param_raw(0, target, OD_CURRENT, 0, "zero current")
            .await;
        let disable = self.write_disable_verified(target, "disable").await;
        match disable {
            Ok(()) => {
                if let Err(error) = zero {
                    // Disable is the safety boundary: once output is disabled,
                    // a failed preceding zero-current frame is non-fatal.
                    log::warn!(
                        "RollerCAN 0x{target:02X}: zero-current send failed before successful disable: {error}"
                    );
                }
                self.running.store(false, Ordering::SeqCst);
                *self.target_id.lock().unwrap() = None;
                let mut state = self.state.lock().unwrap();
                if let Some(node) = state.nodes.get_mut(&target) {
                    node.knob.running = false;
                    node.knob.enabled = false;
                }
                state.knob.running = false;
                state.knob.enabled = false;
                Ok(())
            }
            Err(error) => {
                // Firmware may resume its haptic output on the next tick even
                // if zero-current succeeded, so keep the session visibly
                // active and allow Stop to be retried.
                self.running.store(true, Ordering::SeqCst);
                let mut state = self.state.lock().unwrap();
                if let Some(node) = state.nodes.get_mut(&target) {
                    node.knob.running = true;
                    node.knob.enabled = true;
                    node.knob.error =
                        Some("disable could not be confirmed; motor may still be active".into());
                }
                state.knob.running = true;
                state.knob.enabled = true;
                state.knob.error =
                    Some("disable could not be confirmed; motor may still be active".into());
                Err(error.context(format!(
                    "RollerCAN 0x{target:02X} disable could not be confirmed; SmartKnob may still be active"
                )))
            }
        }
    }

    pub async fn set_config(&self, target_id: u8, index: usize) -> Result<()> {
        let preset = preset_configs()
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("invalid RollerCAN SmartKnob mode {index}"))?;
        let config = if index == 0 {
            self.custom_config.lock().unwrap().clone()
        } else {
            preset.clone()
        };
        let tuning = self
            .per_mode_tuning
            .lock()
            .unwrap()
            .get(index)
            .copied()
            .unwrap_or_else(|| Tuning::from_config(&preset));
        self.write_param_raw(
            0,
            target_id,
            RC_CMD_SET_CONFIG,
            index as i32,
            "select firmware preset",
        )
        .await?;
        *self.requested_config.lock().unwrap() = index;
        *self.tuning.lock().unwrap() = tuning;
        let mut state = self.state.lock().unwrap();
        if let Some(node) = state.nodes.get_mut(&target_id) {
            node.knob.config_index = index;
            node.knob.config = Some(config.clone());
            node.knob.min_position = config.min_position;
            node.knob.max_position = config.max_position;
            node.knob.num_positions = position_count(&config);
            node.knob.p_gain = tuning.p_gain;
            node.knob.d_gain = tuning.d_gain;
            node.knob.strength_scale = tuning.strength_scale;
            node.knob.torque_limit_nm = tuning.torque_limit_nm;
            node.knob.max_torque_permille = tuning.max_torque_permille;
            node.knob.friction_compensation = tuning.friction_compensation;
            node.knob.click_torque_nm = tuning.click_torque_nm;
            state.knob = node.knob.clone();
        }
        Ok(())
    }

    pub async fn set_tuning_config(
        &self,
        target_id: u8,
        tuning: crate::unified_smartknob::SmartKnobTuning,
    ) -> Result<()> {
        self.write_tuning_raw(target_id, tuning).await?;
        let internal = Tuning {
            p_gain: tuning.p_gain,
            d_gain: tuning.d_gain,
            strength_scale: tuning.strength_scale,
            torque_limit_nm: tuning.effort_limit,
            max_torque_permille: tuning.max_output_permille,
            friction_compensation: tuning.friction_compensation,
            click_torque_nm: tuning.click_effort,
        }
        .sanitized();
        *self.tuning.lock().unwrap() = internal;
        let index = *self.requested_config.lock().unwrap();
        if let Some(slot) = self.per_mode_tuning.lock().unwrap().get_mut(index) {
            *slot = internal;
        }
        if let Some(node) = self.state.lock().unwrap().nodes.get_mut(&target_id) {
            node.knob.p_gain = internal.p_gain;
            node.knob.d_gain = internal.d_gain;
            node.knob.strength_scale = internal.strength_scale;
            node.knob.torque_limit_nm = internal.torque_limit_nm;
            node.knob.max_torque_permille = internal.max_torque_permille;
            node.knob.friction_compensation = internal.friction_compensation;
            node.knob.click_torque_nm = internal.click_torque_nm;
        }
        Ok(())
    }

    pub async fn set_custom_config(&self, target_id: u8, config: KnobConfig) -> Result<()> {
        let config = sanitize_custom_config(config);
        self.write_custom_config_raw(target_id, &config).await?;
        *self.custom_config.lock().unwrap() = config.clone();
        let mut state = self.state.lock().unwrap();
        if let Some(node) = state.nodes.get_mut(&target_id) {
            if node.knob.config_index == 0 {
                node.knob.config = Some(config.clone());
                node.knob.current_position = config.position;
                node.knob.min_position = config.min_position;
                node.knob.max_position = config.max_position;
                node.knob.num_positions = position_count(&config);
                state.knob = node.knob.clone();
            }
        }
        Ok(())
    }

    pub async fn set_telemetry(
        &self,
        target_id: u8,
        telemetry: crate::unified_smartknob::SmartKnobTelemetry,
    ) -> Result<()> {
        let rate_hz = telemetry.rate_hz.clamp(1, ROLLER_MAX_TELEMETRY_RATE_HZ);
        self.write_param_raw(
            0,
            target_id,
            RC_TELEMETRY_RATE_HZ,
            i32::from(rate_hz),
            "telemetry rate",
        )
        .await?;
        self.write_param_raw(
            0,
            target_id,
            RC_TELEMETRY_ENABLE,
            i32::from(telemetry.enabled),
            if telemetry.enabled {
                "telemetry on"
            } else {
                "telemetry off"
            },
        )
        .await?;
        let mut state = self.state.lock().unwrap();
        let node = state
            .nodes
            .entry(target_id)
            .or_insert_with(|| RollerCanNode::new(target_id, Instant::now()));
        node.telemetry_enabled = telemetry.enabled;
        node.telemetry_rate_hz = rate_hz;
        node.telemetry_configured = true;
        node.telemetry_expected_since = telemetry.enabled.then(Instant::now);
        Ok(())
    }

    async fn write_custom_config_raw(&self, target_id: u8, config: &KnobConfig) -> Result<()> {
        // Bounds before position: firmware sanitizes after every write and
        // would otherwise clamp the new position against stale bounds.
        for (index, value) in custom_parameter_values(config) {
            self.write_param_raw(0, target_id, index, value, "firmware custom config")
                .await?;
        }
        Ok(())
    }

    async fn write_custom_config_verified(
        &self,
        target_id: u8,
        config: &KnobConfig,
        shutdown_requested: Option<&AtomicBool>,
    ) -> Result<()> {
        for (index, value) in custom_parameter_values(config) {
            self.write_param_verified(
                target_id,
                index,
                value,
                verification_tolerance(index),
                "firmware custom config",
                shutdown_requested,
            )
            .await?;
        }
        Ok(())
    }

    async fn write_tuning_raw(
        &self,
        target_id: u8,
        tuning: crate::unified_smartknob::SmartKnobTuning,
    ) -> Result<()> {
        for (index, value) in tuning_parameter_values(tuning) {
            self.write_param_raw(0, target_id, index, value, "firmware tuning")
                .await?;
        }
        Ok(())
    }

    async fn write_tuning_verified(
        &self,
        target_id: u8,
        tuning: crate::unified_smartknob::SmartKnobTuning,
        shutdown_requested: Option<&AtomicBool>,
    ) -> Result<()> {
        for (index, value) in tuning_parameter_values(tuning) {
            self.write_param_verified(
                target_id,
                index,
                value,
                verification_tolerance(index),
                "firmware tuning",
                shutdown_requested,
            )
            .await?;
        }
        Ok(())
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

    fn register_param_read(
        &self,
        target_id: u8,
        host_id: u8,
        index: u16,
    ) -> tokio::sync::oneshot::Receiver<i32> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut state = self.state.lock().unwrap();
        let waiters = state
            .pending_reads
            .entry((target_id, host_id, index))
            .or_default();
        waiters.retain(|waiter| !waiter.is_closed());
        waiters.push(tx);
        rx
    }

    fn register_status_read(
        &self,
        target_id: u8,
        host_id: u8,
    ) -> tokio::sync::oneshot::Receiver<RollerCanStatus> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut state = self.state.lock().unwrap();
        let waiters = state
            .pending_status
            .entry((target_id, host_id))
            .or_default();
        waiters.retain(|waiter| !waiter.is_closed());
        waiters.push(tx);
        rx
    }

    async fn read_param_value(&self, target_id: u8, index: u16) -> Result<i32> {
        let mut last_error = None;
        for attempt in 1..=PARAM_READ_ATTEMPTS {
            let host_id = self.next_response_host_id();
            let response = self.register_param_read(target_id, host_id, index);
            if let Err(error) = self
                .send_command(
                    0x11,
                    0,
                    host_id,
                    target_id,
                    read_param_frame(index),
                    "verify parameter",
                )
                .await
            {
                return Err(error.context(format!(
                    "request readback 0x{index:04X} from RollerCAN 0x{target_id:02X}"
                )));
            }
            match tokio::time::timeout(PARAM_READ_TIMEOUT, response).await {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(_)) => {
                    last_error = Some("readback waiter was cancelled".to_string());
                }
                Err(_) => {
                    last_error = Some(format!(
                        "readback timed out after {} ms (attempt {attempt}/{PARAM_READ_ATTEMPTS})",
                        PARAM_READ_TIMEOUT.as_millis()
                    ));
                }
            }
        }
        Err(anyhow!(
            "RollerCAN 0x{target_id:02X} parameter 0x{index:04X}: {}",
            last_error.unwrap_or_else(|| "readback failed".to_string())
        ))
    }

    async fn write_param_verified(
        &self,
        target_id: u8,
        index: u16,
        value: i32,
        tolerance: i32,
        note: &'static str,
        shutdown_requested: Option<&AtomicBool>,
    ) -> Result<()> {
        ensure_start_not_cancelled(shutdown_requested)?;
        self.write_param_raw(0, target_id, index, value, note)
            .await?;
        ensure_start_not_cancelled(shutdown_requested)?;
        let actual = self.read_param_value(target_id, index).await?;
        ensure_start_not_cancelled(shutdown_requested)?;
        if i64::from(actual).abs_diff(i64::from(value)) > tolerance.max(0) as u64 {
            return Err(anyhow!(
                "RollerCAN 0x{target_id:02X} rejected {note}: parameter 0x{index:04X} expected {value}, read back {actual}"
            ));
        }
        Ok(())
    }

    async fn write_enable_verified(
        &self,
        target_id: u8,
        shutdown_requested: Option<&AtomicBool>,
    ) -> Result<Instant> {
        ensure_start_not_cancelled(shutdown_requested)?;
        let host_id = self.next_response_host_id();
        let status_response = self.register_status_read(target_id, host_id);
        self.write_param_raw(host_id, target_id, OD_ENABLE, 1, "enable")
            .await?;
        ensure_start_not_cancelled(shutdown_requested)?;
        let status = tokio::time::timeout(ENABLE_STATUS_TIMEOUT, status_response)
            .await
            .map_err(|_| {
                anyhow!(
                    "RollerCAN 0x{target_id:02X} enable status timed out after {} ms",
                    ENABLE_STATUS_TIMEOUT.as_millis()
                )
            })?
            .map_err(|_| anyhow!("RollerCAN 0x{target_id:02X} enable status was cancelled"))?;
        ensure_start_not_cancelled(shutdown_requested)?;
        if status.fault != 0 || status.mode != 4 || status.state != 1 {
            return Err(anyhow!(
                "RollerCAN 0x{target_id:02X} did not enter MODE_DIAL/RUNNING after enable (fault=0b{:03b}, mode={}, state={})",
                status.fault,
                status.mode,
                status.state
            ));
        }
        // A telemetry sample latched before this exact, nonce-correlated
        // status cannot be used as enable confirmation.
        let status_confirmed_at = Instant::now();
        let enabled = self.read_param_value(target_id, OD_ENABLE).await?;
        ensure_start_not_cancelled(shutdown_requested)?;
        if enabled != 1 {
            return Err(anyhow!(
                "RollerCAN 0x{target_id:02X} enable readback expected 1, got {enabled}"
            ));
        }
        Ok(status_confirmed_at)
    }

    /// Disable output and prove that this exact write was processed. The
    /// nonce-correlated cmd=0x02 status establishes command causality, while
    /// the subsequent 0x7004 readback confirms firmware's output latch is 0.
    async fn write_disable_verified(&self, target_id: u8, note: &'static str) -> Result<()> {
        let host_id = self.next_response_host_id();
        let status_response = self.register_status_read(target_id, host_id);
        self.write_param_raw(host_id, target_id, OD_ENABLE, 0, note)
            .await?;
        let status = tokio::time::timeout(ENABLE_STATUS_TIMEOUT, status_response)
            .await
            .map_err(|_| {
                anyhow!(
                    "RollerCAN 0x{target_id:02X} disable status timed out after {} ms",
                    ENABLE_STATUS_TIMEOUT.as_millis()
                )
            })?
            .map_err(|_| anyhow!("RollerCAN 0x{target_id:02X} disable status was cancelled"))?;
        if !matches!(status.state, 0 | 2) {
            return Err(anyhow!(
                "RollerCAN 0x{target_id:02X} still reported RUNNING after disable (fault=0b{:03b}, mode={}, state={})",
                status.fault,
                status.mode,
                status.state
            ));
        }
        let enabled = self.read_param_value(target_id, OD_ENABLE).await?;
        if enabled != 0 {
            return Err(anyhow!(
                "RollerCAN 0x{target_id:02X} disable readback expected 0, got {enabled}"
            ));
        }
        Ok(())
    }

    fn next_response_host_id(&self) -> u8 {
        self.next_response_host_id
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(if value == u8::MAX { 1 } else { value + 1 })
            })
            .unwrap_or(1)
    }

    async fn wait_for_enabled_telemetry(
        &self,
        target_id: u8,
        after: Instant,
        rate_hz: u16,
        shutdown_requested: Option<&AtomicBool>,
    ) -> Result<()> {
        let period_ms = 1000_u64 / u64::from(rate_hz.max(1));
        let timeout = Duration::from_millis((period_ms * 2 + 100).clamp(250, 2_100));
        tokio::time::timeout(timeout, async {
            loop {
                ensure_start_not_cancelled(shutdown_requested)?;
                let sample = {
                    let state = self.state.lock().unwrap();
                    state.nodes.get(&target_id).and_then(|node| {
                        node.last_telemetry
                            .filter(|received_at| *received_at >= after)
                            .map(|_| {
                                (
                                    node.knob.running,
                                    node.knob.enabled,
                                    node.knob.error.clone(),
                                )
                            })
                    })
                };
                if let Some((running, enabled, error)) = sample {
                    if let Some(error) = error {
                        return Err(anyhow!(
                            "RollerCAN 0x{target_id:02X} telemetry reported {error} after enable"
                        ));
                    }
                    if running && enabled {
                        return Ok(());
                    }
                    // Telemetry was enabled while the motor was deliberately
                    // still disabled, so an older latched pair can legally
                    // arrive around the enable boundary. Keep waiting for a
                    // newer pair instead of treating that sample as failure.
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow!(
                "RollerCAN 0x{target_id:02X} enabled telemetry timed out after {} ms",
                timeout.as_millis()
            )
        })?
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
            cmd,
            param,
            host_id,
            target_id,
            data,
            note,
        )
        .await
    }
}

async fn send_command(
    bus: &Arc<dyn CanBus>,
    send_lock: &Arc<tokio::sync::Mutex<()>>,
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
        .map_err(|e| anyhow!("{note}: send RollerCAN frame: {e}"))?;
    Ok(())
}

async fn probe_node(
    bus: &Arc<dyn CanBus>,
    send_lock: &Arc<tokio::sync::Mutex<()>>,
    node_id: u8,
) -> Result<()> {
    send_command(
        bus,
        send_lock,
        0x00,
        0,
        0,
        node_id,
        [0; 8],
        "discovery ping",
    )
    .await?;
    probe_identity(bus, send_lock, node_id).await
}

async fn probe_identity(
    bus: &Arc<dyn CanBus>,
    send_lock: &Arc<tokio::sync::Mutex<()>>,
    node_id: u8,
) -> Result<()> {
    for index in [
        RC_MODE_COUNT,
        RC_PROTOCOL_VERSION,
        RC_TELEMETRY_ENABLE,
        RC_TELEMETRY_RATE_HZ,
    ] {
        send_command(
            bus,
            send_lock,
            0x11,
            0,
            0,
            node_id,
            read_param_frame(index),
            "identity probe",
        )
        .await?;
    }
    Ok(())
}

async fn discovery_loop(
    bus: Arc<dyn CanBus>,
    state: Arc<StdMutex<RollerCanState>>,
    send_lock: Arc<tokio::sync::Mutex<()>>,
) {
    if let Err(error) = probe_node(&bus, &send_lock, ROLLER_DEFAULT_NODE_ID).await {
        log::warn!("RollerCAN default-node probe failed: {error}");
    }

    let mut scan = tokio::time::interval(DISCOVERY_STEP);
    scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut known = tokio::time::interval(KNOWN_PING_PERIOD);
    known.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut identify = tokio::time::interval(IDENTITY_PROBE_PERIOD);
    identify.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first known tick; the default probe above already
    // did the useful startup work.
    known.tick().await;
    identify.tick().await;
    let mut cursor = 0_u8;

    loop {
        tokio::select! {
            _ = scan.tick() => {
                let target = cursor;
                cursor = cursor.wrapping_add(1);
                if let Err(error) = send_command(
                    &bus, &send_lock, 0x00, 0, 0, target, [0; 8], "background scan"
                ).await {
                    log::warn!("RollerCAN background scan send failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            _ = known.tick() => {
                let candidates: Vec<u8> = {
                    let mut registry = state.lock().unwrap();
                    registry.nodes.iter_mut().filter_map(|(&node_id, node)| {
                        node.confirmed().then(|| {
                            node.missed_pings = node.missed_pings.saturating_add(1);
                            node_id
                        })
                    }).collect()
                };
                for node_id in candidates {
                    if let Err(error) = send_command(
                        &bus, &send_lock, 0x00, 0, 0, node_id, [0; 8], "known-node ping"
                    ).await {
                        log::warn!("RollerCAN known-node ping 0x{node_id:02X} failed: {error}");
                    }
                }
            }
            _ = identify.tick() => {
                let candidates: Vec<u8> = {
                    let registry = state.lock().unwrap();
                    registry.nodes.iter().filter_map(|(&node_id, node)| {
                        (!node.confirmed()).then_some(node_id)
                    }).collect()
                };
                for node_id in candidates {
                    if let Err(error) = probe_identity(
                        &bus, &send_lock, node_id
                    ).await {
                        log::warn!("RollerCAN identity probe 0x{node_id:02X} failed: {error}");
                    }
                }
            }
        }
    }
}

fn observe_node(state: &mut RollerCanState, node_id: u8, now: Instant) -> &mut RollerCanNode {
    let node = state
        .nodes
        .entry(node_id)
        .or_insert_with(|| RollerCanNode::new(node_id, now));
    // A protocol identity belongs to one physical device presence, not to a
    // numeric CAN ID forever. Once the old presence is offline, the first new
    // response invalidates the cached identity and forces 0x8005/0x8006 to be
    // confirmed again before the node is offered as a RollerCAN target.
    let was_online = node.online_at(now);
    if was_online {
        node.identity_invalidated_while_offline = false;
    } else if node.confirmed() && !node.identity_invalidated_while_offline {
        node.mode_count = None;
        node.protocol_version = None;
        node.telemetry_configured = false;
        node.identity_invalidated_while_offline = true;
        node.pending.clear();
    }
    node.last_seen = now;
    node.missed_pings = 0;
    node
}

fn fulfill_param_read(
    state: &mut RollerCanState,
    node_id: u8,
    host_id: u8,
    index: u16,
    value: i32,
) {
    if let Some(waiters) = state.pending_reads.remove(&(node_id, host_id, index)) {
        for waiter in waiters {
            let _ = waiter.send(value);
        }
    }
}

fn fulfill_status(state: &mut RollerCanState, node_id: u8, host_id: u8, status: RollerCanStatus) {
    if let Some(waiters) = state.pending_status.remove(&(node_id, host_id)) {
        for waiter in waiters {
            let _ = waiter.send(status);
        }
    }
}

fn update_identity_param(
    state: &mut RollerCanState,
    node_id: u8,
    host_id: u8,
    data: &[u8],
    now: Instant,
) {
    if data.len() < 8 {
        return;
    }
    let index = u16::from_le_bytes([data[0], data[1]]);
    let value = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let node = observe_node(state, node_id, now);
    match index {
        RC_MODE_COUNT => node.mode_count = Some(value),
        RC_PROTOCOL_VERSION => node.protocol_version = Some(value),
        RC_TELEMETRY_ENABLE => {
            node.telemetry_enabled = value != 0;
            node.telemetry_configured = true;
            if node.telemetry_enabled {
                // Preserve an existing expectation across identity
                // revalidation. Resetting it here would let periodic pings
                // grant an endless new first-sample grace after a lost stream.
                node.telemetry_expected_since.get_or_insert(now);
            } else {
                node.telemetry_expected_since = None;
            }
        }
        RC_TELEMETRY_RATE_HZ => {
            node.telemetry_rate_hz =
                (value.clamp(1, i32::from(ROLLER_MAX_TELEMETRY_RATE_HZ))) as u16
        }
        _ => {}
    }
    fulfill_param_read(state, node_id, host_id, index, value);
}

fn ingest_telemetry(state: &mut RollerCanState, cmd: u8, raw_id: u32, data: &[u8], now: Instant) {
    if data.len() < 8 {
        return;
    }
    let (source, sequence) = telemetry_source_sequence(raw_id);
    let selected = state.selected_node;
    let node = observe_node(state, source, now);
    node.pending
        .retain(|_, pair| now.saturating_duration_since(pair.first_seen) <= PAIR_TTL);
    let pair = node
        .pending
        .entry(sequence)
        .or_insert_with(|| PendingTelemetry::new(now));
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&data[..8]);
    match cmd {
        0x17 => pair.state = Some(bytes),
        0x18 => pair.motion = Some(bytes),
        _ => return,
    }
    let Some(state_frame) = pair.state else {
        return;
    };
    let Some(motion_frame) = pair.motion else {
        return;
    };
    node.pending.remove(&sequence);
    if node.telemetry_enabled {
        node.telemetry_expected_since.get_or_insert(now);
    }
    node.last_telemetry = Some(now);
    if !node.telemetry_configured {
        node.telemetry_enabled = true;
    }
    update_firmware_state(&mut node.knob, &state_frame, source);
    update_firmware_motion(&mut node.knob, &motion_frame);
    node.knob.online = true;
    node.identity_invalidated_while_offline = false;
    if selected == Some(source) {
        state.knob = node.knob.clone();
    }
}

fn ping_response_source(raw_id: u32) -> Option<u8> {
    ((raw_id & 0xff) == 0xfe).then_some(((raw_id >> 8) & 0xff) as u8)
}

fn function_read_source(raw_id: u32) -> u8 {
    (raw_id & 0xff) as u8
}

fn function_read_host(raw_id: u32) -> u8 {
    ((raw_id >> 8) & 0xff) as u8
}

fn telemetry_source_sequence(raw_id: u32) -> (u8, u8) {
    (((raw_id >> 8) & 0xff) as u8, ((raw_id >> 16) & 0xff) as u8)
}

async fn drain_loop(mut rx: Box<dyn CanRx>, state: Arc<StdMutex<RollerCanState>>) {
    loop {
        match rx.recv().await {
            Ok(frame) => {
                if !matches!(frame.kind(), FrameKind::Data) {
                    continue;
                }
                let raw = frame.id().raw();
                let data = frame.data();
                let cmd = ((raw >> 24) & 0x1F) as u8;
                let now = Instant::now();
                let mut st = state.lock().unwrap();
                match cmd {
                    0x02 if data.len() >= 8 => {
                        let fault = ((raw >> 16) & 0x07) as u8;
                        let node_id = ((raw >> 8) & 0xFF) as u8;
                        let host_id = (raw & 0xFF) as u8;
                        let status = RollerCanStatus {
                            fault,
                            mode: ((raw >> 19) & 0x07) as u8,
                            state: ((raw >> 22) & 0x03) as u8,
                        };
                        observe_node(&mut st, node_id, now);
                        fulfill_status(&mut st, node_id, host_id, status);
                    }
                    0x11 | 0x13 if data.len() >= 8 => {
                        // Function-read replies put the device source in the
                        // low byte (unlike telemetry, whose low byte is host).
                        let source = function_read_source(raw);
                        let host = function_read_host(raw);
                        update_identity_param(&mut st, source, host, data, now);
                    }
                    0x17 if data.len() >= 8 => {
                        ingest_telemetry(&mut st, cmd, raw, data, now);
                    }
                    0x18 if data.len() >= 8 => {
                        ingest_telemetry(&mut st, cmd, raw, data, now);
                    }
                    0x00 => {
                        // Ping response is 0x0000SSFE: source lives in bits
                        // 15..8 and FE is the fixed destination marker.
                        if let Some(source) = ping_response_source(raw) {
                            observe_node(&mut st, source, now);
                        }
                    }
                    _ => {}
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

fn update_firmware_state(state: &mut crate::smartknob::SmartKnobState, data: &[u8], node_id: u8) {
    let mode = data[0] as usize;
    let flags = data[1];
    let position = i32::from_le_bytes([data[2], data[3], data[4], data[5]]);
    let sub_position = i16::from_le_bytes([data[6], data[7]]) as f64 / 10_000.0;
    if mode != 0 || state.config.is_none() {
        state.config = preset_configs().get(mode).cloned();
    }
    if let Some(config) = state.config.as_ref() {
        state.min_position = config.min_position;
        state.max_position = config.max_position;
        state.num_positions = position_count(config);
    }
    state.running = (flags & (1 << 0)) != 0;
    state.enabled = (flags & (1 << 1)) != 0;
    state.at_endstop = (flags & (1 << 2)) != 0;
    state.online = true;
    state.config_index = mode;
    state.current_position = position;
    state.sub_position_unit = sub_position;
    state.node_id = node_id;
    state.error = if (flags & (1 << 6)) != 0 {
        Some("firmware fault".to_string())
    } else if (flags & (1 << 7)) != 0 {
        Some("firmware over-voltage".to_string())
    } else {
        None
    };
}

fn update_firmware_motion(state: &mut crate::smartknob::SmartKnobState, data: &[u8]) {
    let angle_cdeg = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let commanded_ma = i16::from_le_bytes([data[4], data[5]]);
    let measured_ma = i16::from_le_bytes([data[6], data[7]]);
    state.shaft_angle_rad = (angle_cdeg as f64 / 100.0).to_radians();
    state.applied_torque_nm = commanded_ma as f64 / 1000.0;
    state.measured_torque_nm = Some(measured_ma as f32 / 1000.0);
    state.online = true;
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
    // Match firmware `sanitize_mode`; readback verification must compare the
    // value the device can actually retain.
    c.snap_point = finite_or(c.snap_point, 0.55).clamp(0.5, 1.5);
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

fn custom_parameter_values(config: &KnobConfig) -> [(u16, i32); 14] {
    [
        (RC_CUSTOM_MIN_POSITION, config.min_position),
        (RC_CUSTOM_MAX_POSITION, config.max_position),
        (RC_CUSTOM_POSITION, config.position),
        (
            RC_CUSTOM_WIDTH_DEG,
            to_scaled(config.position_width_radians.to_degrees()),
        ),
        (
            RC_CUSTOM_DETENT_STRENGTH,
            to_scaled(config.detent_strength_unit),
        ),
        (
            RC_CUSTOM_ENDSTOP_STRENGTH,
            to_scaled(config.endstop_strength_unit),
        ),
        (RC_CUSTOM_SNAP_POINT, to_scaled(config.snap_point)),
        (RC_CUSTOM_SNAP_BIAS, to_scaled(config.snap_point_bias)),
        (RC_CUSTOM_CLICK, to_scaled(config.click_torque_nm)),
        (RC_CUSTOM_FRICTION, to_scaled(config.friction_compensation)),
        (RC_CUSTOM_STRENGTH, to_scaled(config.strength_scale)),
        (RC_CUSTOM_P_GAIN, to_scaled(config.p_gain)),
        (RC_CUSTOM_D_GAIN, to_scaled(config.d_gain)),
        (RC_CUSTOM_LED_HUE, config.led_hue),
    ]
}

fn tuning_parameter_values(tuning: crate::unified_smartknob::SmartKnobTuning) -> [(u16, i32); 7] {
    [
        (RC_TUNING_P_GAIN, to_scaled(tuning.p_gain)),
        (RC_TUNING_D_GAIN, to_scaled(tuning.d_gain)),
        (RC_TUNING_STRENGTH, to_scaled(tuning.strength_scale)),
        (RC_TUNING_TORQUE_LIMIT, to_scaled(tuning.effort_limit)),
        (
            RC_TUNING_MAX_TORQUE,
            i32::from(tuning.max_output_permille.min(1000)),
        ),
        (RC_TUNING_FRICTION, to_scaled(tuning.friction_compensation)),
        (RC_TUNING_CLICK, to_scaled(tuning.click_effort)),
    ]
}

fn verification_tolerance(index: u16) -> i32 {
    match index {
        RC_TUNING_P_GAIN
        | RC_TUNING_D_GAIN
        | RC_TUNING_STRENGTH
        | RC_TUNING_TORQUE_LIMIT
        | RC_TUNING_FRICTION
        | RC_TUNING_CLICK
        | RC_CUSTOM_WIDTH_DEG
        | RC_CUSTOM_DETENT_STRENGTH
        | RC_CUSTOM_ENDSTOP_STRENGTH
        | RC_CUSTOM_SNAP_POINT
        | RC_CUSTOM_SNAP_BIAS
        | RC_CUSTOM_CLICK
        | RC_CUSTOM_FRICTION
        | RC_CUSTOM_STRENGTH
        | RC_CUSTOM_P_GAIN
        | RC_CUSTOM_D_GAIN => VERIFY_SCALED_TOLERANCE,
        _ => 0,
    }
}

fn read_param_frame(index: u16) -> [u8; 8] {
    let mut data = [0u8; 8];
    data[0..2].copy_from_slice(&index.to_le_bytes());
    data
}

fn to_scaled(value: f64) -> i32 {
    (finite_or(value, 0.0) * SCALE)
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct PendingRx;

    #[async_trait::async_trait]
    impl CanRx for PendingRx {
        async fn recv(&mut self) -> std::result::Result<CanFrame, CanIoError> {
            std::future::pending().await
        }

        fn try_recv(&mut self) -> std::result::Result<Option<CanFrame>, CanIoError> {
            Ok(None)
        }
    }

    #[derive(Default)]
    struct FakeBus {
        attempts: StdMutex<Vec<CanFrame>>,
        send_count: AtomicUsize,
        fail_on: AtomicUsize,
        suppress_disable_status: AtomicBool,
        state: StdMutex<Option<Arc<StdMutex<RollerCanState>>>>,
        parameter_values: StdMutex<HashMap<(u8, u16), i32>>,
        readback_overrides: StdMutex<HashMap<u16, i32>>,
    }

    impl FakeBus {
        fn fail_on(&self, attempt: usize) {
            self.fail_on.store(attempt, AtomicOrdering::SeqCst);
        }

        fn suppress_disable_status(&self) {
            self.suppress_disable_status
                .store(true, AtomicOrdering::SeqCst);
        }

        fn bind_state(&self, state: Arc<StdMutex<RollerCanState>>) {
            *self.state.lock().unwrap() = Some(state);
        }

        fn override_readback(&self, index: u16, value: i32) {
            self.readback_overrides.lock().unwrap().insert(index, value);
        }

        fn params(&self) -> Vec<(u16, i32)> {
            self.attempts
                .lock()
                .unwrap()
                .iter()
                .filter(|frame| ((frame.id().raw() >> 24) & 0x1f) == 0x12)
                .map(|frame| {
                    let data = frame.data();
                    (
                        u16::from_le_bytes([data[0], data[1]]),
                        i32::from_le_bytes([data[4], data[5], data[6], data[7]]),
                    )
                })
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl CanBus for FakeBus {
        async fn send(&self, frame: CanFrame) -> std::result::Result<(), CanIoError> {
            let attempt = self.send_count.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let raw_id = frame.id().raw();
            let cmd = ((raw_id >> 24) & 0x1f) as u8;
            let host_id = ((raw_id >> 8) & 0xff) as u8;
            let target_id = (raw_id & 0xff) as u8;
            let data = frame.data().to_vec();
            self.attempts.lock().unwrap().push(frame);
            if self.fail_on.load(AtomicOrdering::SeqCst) == attempt {
                return Err(CanIoError::Disconnected);
            }

            if cmd == 0x12 && data.len() >= 8 {
                let index = u16::from_le_bytes([data[0], data[1]]);
                let value = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                let stored_value = self
                    .readback_overrides
                    .lock()
                    .unwrap()
                    .get(&index)
                    .copied()
                    .unwrap_or(value);
                self.parameter_values
                    .lock()
                    .unwrap()
                    .insert((target_id, index), stored_value);

                if let Some(state) = self.state.lock().unwrap().clone() {
                    let values = self.parameter_values.lock().unwrap();
                    let mode = *values.get(&(target_id, OD_RUN_MODE)).unwrap_or(&0) as u8;
                    let enabled = *values.get(&(target_id, OD_ENABLE)).unwrap_or(&0) != 0;
                    let telemetry =
                        *values.get(&(target_id, RC_TELEMETRY_ENABLE)).unwrap_or(&0) != 0;
                    let config_index =
                        *values.get(&(target_id, RC_CMD_SET_CONFIG)).unwrap_or(&0) as usize;
                    drop(values);

                    if !(index == OD_ENABLE
                        && value == 0
                        && self.suppress_disable_status.load(AtomicOrdering::SeqCst))
                    {
                        let mut registry = state.lock().unwrap();
                        fulfill_status(
                            &mut registry,
                            target_id,
                            host_id,
                            RollerCanStatus {
                                fault: 0,
                                mode,
                                state: u8::from(enabled && mode == 4),
                            },
                        );
                    }
                    if index == OD_ENABLE && enabled && mode == 4 && telemetry {
                        // Real telemetry is produced on a later firmware tick,
                        // after the enable status. Model that boundary so the
                        // startup test cannot pass using a pre-enable sample.
                        let telemetry_state = state.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(2)).await;
                            let now = Instant::now();
                            let mut registry = telemetry_state.lock().unwrap();
                            let node = observe_node(&mut registry, target_id, now);
                            node.last_telemetry = Some(now);
                            node.knob.running = true;
                            node.knob.enabled = true;
                            node.knob.config_index = config_index;
                            node.knob.error = None;
                        });
                    }
                }
            } else if cmd == 0x11 && data.len() >= 2 {
                let index = u16::from_le_bytes([data[0], data[1]]);
                let value = *self
                    .parameter_values
                    .lock()
                    .unwrap()
                    .get(&(target_id, index))
                    .unwrap_or(&0);
                if let Some(state) = self.state.lock().unwrap().clone() {
                    update_identity_param(
                        &mut state.lock().unwrap(),
                        target_id,
                        host_id,
                        &identity_payload(index, value),
                        Instant::now(),
                    );
                }
            }
            Ok(())
        }

        async fn subscribe(
            &self,
            _filter: CanFilter,
        ) -> std::result::Result<Box<dyn CanRx>, CanIoError> {
            Ok(Box::new(PendingRx))
        }

        fn capabilities(&self) -> can_transport::CanCapabilities {
            can_transport::CanCapabilities {
                fd: true,
                max_dlen: 64,
            }
        }
    }

    fn test_session(bus: Arc<FakeBus>) -> RollerCanSession {
        let configs = preset_configs();
        let state = Arc::new(StdMutex::new(RollerCanState::default()));
        bus.bind_state(state.clone());
        RollerCanSession {
            bus,
            state,
            rx_task: tokio::spawn(async {}),
            discovery_task: tokio::spawn(async {}),
            running: Arc::new(AtomicBool::new(false)),
            requested_config: Arc::new(StdMutex::new(0)),
            tuning: Arc::new(StdMutex::new(Tuning::from_config(&configs[0]))),
            per_mode_tuning: Arc::new(StdMutex::new(
                configs.iter().map(Tuning::from_config).collect(),
            )),
            custom_config: Arc::new(StdMutex::new(configs[0].clone())),
            target_id: StdMutex::new(None),
            next_response_host_id: AtomicU8::new(1),
            send_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn state_payload(position: i32) -> [u8; 8] {
        let mut data = [0_u8; 8];
        data[0] = 1;
        data[1] = (1 << 0) | (1 << 1);
        data[2..6].copy_from_slice(&position.to_le_bytes());
        data
    }

    fn motion_payload(angle_cdeg: i32, commanded_ma: i16, measured_ma: i16) -> [u8; 8] {
        let mut data = [0_u8; 8];
        data[..4].copy_from_slice(&angle_cdeg.to_le_bytes());
        data[4..6].copy_from_slice(&commanded_ma.to_le_bytes());
        data[6..8].copy_from_slice(&measured_ma.to_le_bytes());
        data
    }

    fn telemetry_id(cmd: u8, sequence: u8, source: u8) -> u32 {
        ((cmd as u32) << 24) | ((sequence as u32) << 16) | ((source as u32) << 8)
    }

    fn identity_payload(index: u16, value: i32) -> [u8; 8] {
        let mut data = [0_u8; 8];
        data[..2].copy_from_slice(&index.to_le_bytes());
        data[4..].copy_from_slice(&value.to_le_bytes());
        data
    }

    #[test]
    fn command_specific_source_fields_are_not_confused() {
        assert_eq!(ping_response_source(0x0000_A8FE), Some(0xA8));
        assert_eq!(ping_response_source(0x0000_A800), None);
        assert_eq!(function_read_source(0x1100_00A8), 0xA8);
        assert_eq!(function_read_host(0x1100_5AA8), 0x5A);
        assert_eq!(telemetry_source_sequence(0x172A_A800), (0xA8, 0x2A));
    }

    #[tokio::test]
    async fn parameter_readback_requires_matching_response_host_tag() {
        let bus = Arc::new(FakeBus::default());
        let session = test_session(bus);
        let mut response = session.register_param_read(0xA8, 0x5A, RC_MODE_COUNT);
        let now = Instant::now();

        update_identity_param(
            &mut session.state.lock().unwrap(),
            0xA8,
            0,
            &identity_payload(RC_MODE_COUNT, 99),
            now,
        );
        assert!(matches!(
            response.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        update_identity_param(
            &mut session.state.lock().unwrap(),
            0xA8,
            0x5A,
            &identity_payload(RC_MODE_COUNT, ROLLER_MODE_COUNT),
            now,
        );
        assert_eq!(response.try_recv().unwrap(), ROLLER_MODE_COUNT);
    }

    #[test]
    fn telemetry_updates_only_after_matching_source_and_sequence_pair() {
        let now = Instant::now();
        let mut registry = RollerCanState::default();
        ingest_telemetry(
            &mut registry,
            0x17,
            telemetry_id(0x17, 7, 0xA8),
            &state_payload(42),
            now,
        );
        let node = registry.nodes.get(&0xA8).unwrap();
        assert_eq!(node.knob.current_position, 0);
        assert!(node.last_telemetry.is_none());

        // A motion half from another source must not complete A8's pair.
        ingest_telemetry(
            &mut registry,
            0x18,
            telemetry_id(0x18, 7, 0xA9),
            &motion_payload(9000, 100, 90),
            now,
        );
        assert_eq!(registry.nodes.get(&0xA8).unwrap().knob.current_position, 0);

        // Completing A9 updates only A9.
        ingest_telemetry(
            &mut registry,
            0x17,
            telemetry_id(0x17, 7, 0xA9),
            &state_payload(9),
            now,
        );
        assert_eq!(registry.nodes.get(&0xA9).unwrap().knob.current_position, 9);
        assert_eq!(registry.nodes.get(&0xA8).unwrap().knob.current_position, 0);

        ingest_telemetry(
            &mut registry,
            0x18,
            telemetry_id(0x18, 7, 0xA8),
            &motion_payload(9000, 100, 90),
            now,
        );
        let node = registry.nodes.get(&0xA8).unwrap();
        assert_eq!(node.knob.current_position, 42);
        assert!(node.knob.running);
        assert!(node.knob.enabled);
        assert!((node.knob.shaft_angle_rad - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
        assert_eq!(node.knob.applied_torque_nm, 0.1);
        assert_eq!(node.last_telemetry, Some(now));
        assert_eq!(registry.nodes.get(&0xA9).unwrap().knob.current_position, 9);
    }

    #[tokio::test]
    async fn firmware_start_uses_safe_order_and_custom_bounds_before_position() {
        let bus = Arc::new(FakeBus::default());
        let session = test_session(bus.clone());
        let mut custom = preset_configs()[0].clone();
        custom.min_position = -3;
        custom.max_position = 7;
        custom.position = 6;
        session
            .start_knob(
                0,
                0xA8,
                Some(custom),
                None,
                crate::unified_smartknob::SmartKnobTelemetry::default(),
                None,
            )
            .await
            .unwrap();

        let params = bus.params();
        let indices: Vec<_> = params.iter().map(|(index, _)| *index).collect();
        assert_eq!(indices[0..3], [OD_ENABLE, OD_CURRENT, RC_CMD_SET_CONFIG]);
        let min_at = indices
            .iter()
            .position(|v| *v == RC_CUSTOM_MIN_POSITION)
            .unwrap();
        let max_at = indices
            .iter()
            .position(|v| *v == RC_CUSTOM_MAX_POSITION)
            .unwrap();
        let pos_at = indices
            .iter()
            .position(|v| *v == RC_CUSTOM_POSITION)
            .unwrap();
        assert!(min_at < max_at && max_at < pos_at);
        let tuning_at = indices.iter().position(|v| *v == RC_TUNING_P_GAIN).unwrap();
        let telemetry_at = indices
            .iter()
            .position(|v| *v == RC_TELEMETRY_HOST_ID)
            .unwrap();
        let dial_at = indices.iter().position(|v| *v == OD_RUN_MODE).unwrap();
        assert!(pos_at < tuning_at && tuning_at < telemetry_at && telemetry_at < dial_at);
        assert_eq!(params[dial_at], (OD_RUN_MODE, 4));
        assert_eq!(params.last().copied(), Some((OD_ENABLE, 1)));
    }

    #[tokio::test]
    async fn firmware_start_failure_always_ends_with_zero_and_disable_rollback() {
        let bus = Arc::new(FakeBus::default());
        bus.fail_on(5);
        let session = test_session(bus.clone());
        let result = session
            .start_knob(
                1,
                0xA8,
                None,
                None,
                crate::unified_smartknob::SmartKnobTelemetry::default(),
                None,
            )
            .await;
        assert!(result.is_err());
        let params = bus.params();
        assert_eq!(params[params.len() - 2], (OD_CURRENT, 0));
        assert_eq!(params[params.len() - 1], (OD_ENABLE, 0));
        assert!(!session.running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn firmware_start_readback_mismatch_rolls_back_disabled() {
        let bus = Arc::new(FakeBus::default());
        bus.override_readback(RC_TUNING_P_GAIN, -123);
        let session = test_session(bus.clone());
        let result = session
            .start_knob(
                1,
                0xA8,
                None,
                None,
                crate::unified_smartknob::SmartKnobTelemetry::default(),
                None,
            )
            .await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("read back -123"), "{message}");
        let params = bus.params();
        assert_eq!(params[params.len() - 2], (OD_CURRENT, 0));
        assert_eq!(params[params.len() - 1], (OD_ENABLE, 0));
        assert!(!session.running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn failed_disable_keeps_session_active_for_stop_retry() {
        let bus = Arc::new(FakeBus::default());
        bus.fail_on(2); // zero succeeds, first disable attempt fails
        let session = test_session(bus.clone());
        session.running.store(true, Ordering::SeqCst);
        *session.target_id.lock().unwrap() = Some(0xA8);
        session
            .state
            .lock()
            .unwrap()
            .nodes
            .insert(0xA8, RollerCanNode::new(0xA8, Instant::now()));
        session
            .state
            .lock()
            .unwrap()
            .nodes
            .get_mut(&0xA8)
            .unwrap()
            .knob
            .running = true;

        assert!(session.stop_motor(0, 0xA8).await.is_err());
        assert!(session.running.load(Ordering::SeqCst));
        assert!(session.knob_state(0xA8).running);
        assert_eq!(*session.target_id.lock().unwrap(), Some(0xA8));

        assert!(session.stop_motor(0, 0xA8).await.is_ok());
        assert!(!session.running.load(Ordering::SeqCst));
        assert!(!session.knob_state(0xA8).running);
        assert_eq!(*session.target_id.lock().unwrap(), None);

        let sent_after_confirmed_stop = bus.params();
        session.stop().await;
        assert_eq!(
            bus.params(),
            sent_after_confirmed_stop,
            "session teardown repeated an already-confirmed disable"
        );
    }

    #[tokio::test]
    async fn unconfirmed_disable_readback_keeps_session_active() {
        let bus = Arc::new(FakeBus::default());
        // Model a transport-level successful write that firmware did not
        // apply. Stop must not clear the active marker on send success alone.
        bus.override_readback(OD_ENABLE, 1);
        let session = test_session(bus);
        session.running.store(true, Ordering::SeqCst);
        let mut node = RollerCanNode::new(0xA8, Instant::now());
        node.knob.running = true;
        node.knob.enabled = true;
        session.state.lock().unwrap().nodes.insert(0xA8, node);

        let error = session.stop_motor(0, 0xA8).await.unwrap_err();
        assert!(format!("{error:#}").contains("readback expected 0"));
        assert!(session.may_be_active());
        let state = session.knob_state(0xA8);
        assert!(state.running);
        assert!(state.enabled);
    }

    #[tokio::test]
    async fn missing_disable_status_keeps_session_active() {
        let bus = Arc::new(FakeBus::default());
        bus.suppress_disable_status();
        let session = test_session(bus);
        session.running.store(true, Ordering::SeqCst);
        let mut node = RollerCanNode::new(0xA8, Instant::now());
        node.knob.running = true;
        node.knob.enabled = true;
        session.state.lock().unwrap().nodes.insert(0xA8, node);

        let error = session.stop_motor(0, 0xA8).await.unwrap_err();
        assert!(format!("{error:#}").contains("status timed out"));
        assert!(session.may_be_active());
        assert!(session.knob_state(0xA8).running);
    }

    #[tokio::test]
    async fn rollback_disable_failure_keeps_possible_output_visible() {
        let bus = Arc::new(FakeBus::default());
        bus.fail_on(2); // rollback zero succeeds, rollback disable fails
        let session = test_session(bus);
        session
            .state
            .lock()
            .unwrap()
            .nodes
            .insert(0xA8, RollerCanNode::new(0xA8, Instant::now()));

        assert!(!session.best_effort_disable(0xA8).await);
        assert!(session.may_be_active());
        let state = session.knob_state(0xA8);
        assert!(state.running);
        assert!(state.enabled);
        assert!(state
            .error
            .as_deref()
            .is_some_and(|message| { message.contains("rollback disable failed") }));
    }

    #[tokio::test]
    async fn enabled_telemetry_wait_ignores_early_disabled_sample() {
        let bus = Arc::new(FakeBus::default());
        let session = test_session(bus);
        let after = Instant::now();
        let mut node = RollerCanNode::new(0xA8, after);
        node.last_telemetry = Some(Instant::now());
        node.knob.running = false;
        node.knob.enabled = false;
        session.state.lock().unwrap().nodes.insert(0xA8, node);

        let state = session.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            let mut registry = state.lock().unwrap();
            let node = registry.nodes.get_mut(&0xA8).unwrap();
            node.last_telemetry = Some(Instant::now());
            node.knob.running = true;
            node.knob.enabled = true;
        });

        session
            .wait_for_enabled_telemetry(0xA8, after, 100, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancellation_rolls_back_before_enable() {
        let bus = Arc::new(FakeBus::default());
        let session = test_session(bus.clone());
        let shutdown_requested = AtomicBool::new(true);

        let result = session
            .start_knob(
                1,
                0xA8,
                None,
                None,
                crate::unified_smartknob::SmartKnobTelemetry::default(),
                Some(&shutdown_requested),
            )
            .await;

        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("cancelled"));
        assert_eq!(bus.params(), vec![(OD_CURRENT, 0), (OD_ENABLE, 0)]);
        assert!(!session.may_be_active());
    }

    #[test]
    fn telemetry_pairing_handles_out_of_order_wrap_and_dropped_halves() {
        let now = Instant::now();
        let mut registry = RollerCanState::default();
        // Motion-before-state is valid.
        ingest_telemetry(
            &mut registry,
            0x18,
            telemetry_id(0x18, 255, 0xA8),
            &motion_payload(100, 10, 9),
            now,
        );
        ingest_telemetry(
            &mut registry,
            0x17,
            telemetry_id(0x17, 255, 0xA8),
            &state_payload(1),
            now,
        );
        assert_eq!(registry.nodes.get(&0xA8).unwrap().knob.current_position, 1);

        // Sequence wrap creates a fresh pair.
        ingest_telemetry(
            &mut registry,
            0x17,
            telemetry_id(0x17, 0, 0xA8),
            &state_payload(2),
            now + Duration::from_millis(1),
        );
        ingest_telemetry(
            &mut registry,
            0x18,
            telemetry_id(0x18, 0, 0xA8),
            &motion_payload(200, 20, 19),
            now + Duration::from_millis(1),
        );
        assert_eq!(registry.nodes.get(&0xA8).unwrap().knob.current_position, 2);

        // A stale half is pruned and cannot combine with a much later half.
        ingest_telemetry(
            &mut registry,
            0x17,
            telemetry_id(0x17, 1, 0xA8),
            &state_payload(99),
            now + Duration::from_millis(2),
        );
        ingest_telemetry(
            &mut registry,
            0x18,
            telemetry_id(0x18, 1, 0xA8),
            &motion_payload(999, 99, 99),
            now + PAIR_TTL + Duration::from_millis(3),
        );
        assert_eq!(registry.nodes.get(&0xA8).unwrap().knob.current_position, 2);
    }

    #[test]
    fn in_flight_pair_does_not_reenable_host_disabled_telemetry() {
        let now = Instant::now();
        let mut registry = RollerCanState::default();
        let mut node = RollerCanNode::new(0xA8, now);
        node.telemetry_enabled = false;
        node.telemetry_configured = true;
        registry.nodes.insert(0xA8, node);

        ingest_telemetry(
            &mut registry,
            0x17,
            telemetry_id(0x17, 4, 0xA8),
            &state_payload(4),
            now,
        );
        ingest_telemetry(
            &mut registry,
            0x18,
            telemetry_id(0x18, 4, 0xA8),
            &motion_payload(400, 40, 39),
            now,
        );

        let node = registry.nodes.get(&0xA8).unwrap();
        assert!(!node.telemetry_enabled);
        assert_eq!(node.last_telemetry, Some(now));
    }

    #[test]
    fn identity_requires_both_expected_values_on_the_same_node() {
        let now = Instant::now();
        let mut registry = RollerCanState::default();
        update_identity_param(
            &mut registry,
            0xA8,
            0,
            &identity_payload(RC_MODE_COUNT, ROLLER_MODE_COUNT),
            now,
        );
        update_identity_param(
            &mut registry,
            0xA9,
            0,
            &identity_payload(RC_PROTOCOL_VERSION, ROLLER_PROTOCOL_VERSION),
            now,
        );
        assert!(!registry.nodes.get(&0xA8).unwrap().confirmed());
        assert!(!registry.nodes.get(&0xA9).unwrap().confirmed());

        update_identity_param(
            &mut registry,
            0xA8,
            0,
            &identity_payload(RC_PROTOCOL_VERSION, ROLLER_PROTOCOL_VERSION),
            now,
        );
        assert!(registry.nodes.get(&0xA8).unwrap().confirmed());

        update_identity_param(
            &mut registry,
            0xA8,
            0,
            &identity_payload(RC_MODE_COUNT, ROLLER_MODE_COUNT - 1),
            now,
        );
        assert!(!registry.nodes.get(&0xA8).unwrap().confirmed());
    }

    #[test]
    fn offline_node_identity_is_invalidated_and_must_be_reconfirmed() {
        let now = Instant::now();
        let mut registry = RollerCanState::default();
        let mut old = RollerCanNode::new(0xA8, now - Duration::from_secs(4));
        old.mode_count = Some(ROLLER_MODE_COUNT);
        old.protocol_version = Some(ROLLER_PROTOCOL_VERSION);
        old.telemetry_enabled = false;
        old.missed_pings = 3;
        registry.nodes.insert(0xA8, old);

        // A new response at the same numeric ID must not inherit the previous
        // physical device's RollerCAN classification.
        observe_node(&mut registry, 0xA8, now);
        assert!(!registry.nodes.get(&0xA8).unwrap().confirmed());

        update_identity_param(
            &mut registry,
            0xA8,
            0,
            &identity_payload(RC_MODE_COUNT, ROLLER_MODE_COUNT),
            now,
        );
        assert!(!registry.nodes.get(&0xA8).unwrap().confirmed());
        update_identity_param(
            &mut registry,
            0xA8,
            0,
            &identity_payload(RC_PROTOCOL_VERSION, ROLLER_PROTOCOL_VERSION),
            now,
        );
        assert!(registry.nodes.get(&0xA8).unwrap().confirmed());
    }

    #[test]
    fn stale_expected_telemetry_cannot_be_masked_by_ping_or_reidentity() {
        let now = Instant::now();
        let mut registry = RollerCanState::default();
        let mut node = RollerCanNode::new(0xA8, now);
        node.mode_count = Some(ROLLER_MODE_COUNT);
        node.protocol_version = Some(ROLLER_PROTOCOL_VERSION);
        node.telemetry_enabled = true;
        node.telemetry_configured = true;
        node.telemetry_expected_since = Some(now);
        node.last_telemetry = Some(now);
        registry.nodes.insert(0xA8, node);

        let stale_at = now + Duration::from_millis(501);
        observe_node(&mut registry, 0xA8, stale_at);
        let node = registry.nodes.get(&0xA8).unwrap();
        assert!(!node.confirmed());
        assert_eq!(node.last_telemetry, Some(now));

        update_identity_param(
            &mut registry,
            0xA8,
            0,
            &identity_payload(RC_MODE_COUNT, ROLLER_MODE_COUNT),
            stale_at + Duration::from_millis(1),
        );
        update_identity_param(
            &mut registry,
            0xA8,
            0,
            &identity_payload(RC_PROTOCOL_VERSION, ROLLER_PROTOCOL_VERSION),
            stale_at + Duration::from_millis(2),
        );
        update_identity_param(
            &mut registry,
            0xA8,
            0,
            &identity_payload(RC_TELEMETRY_ENABLE, 1),
            stale_at + Duration::from_millis(3),
        );
        observe_node(&mut registry, 0xA8, stale_at + Duration::from_secs(1));
        let node = registry.nodes.get(&0xA8).unwrap();
        assert!(node.confirmed());
        assert!(!node.online_at(stale_at + Duration::from_secs(1)));

        let resumed_at = stale_at + Duration::from_secs(1) + Duration::from_millis(1);
        ingest_telemetry(
            &mut registry,
            0x17,
            telemetry_id(0x17, 9, 0xA8),
            &state_payload(9),
            resumed_at,
        );
        ingest_telemetry(
            &mut registry,
            0x18,
            telemetry_id(0x18, 9, 0xA8),
            &motion_payload(900, 90, 88),
            resumed_at,
        );
        let node = registry.nodes.get(&0xA8).unwrap();
        assert!(node.confirmed());
        assert!(node.online_at(resumed_at));
    }

    #[test]
    fn online_timeout_uses_paired_telemetry_or_three_missed_pings() {
        let now = Instant::now();
        let mut node = RollerCanNode::new(0xA8, now);
        node.mode_count = Some(ROLLER_MODE_COUNT);
        node.protocol_version = Some(ROLLER_PROTOCOL_VERSION);
        node.telemetry_enabled = true;
        node.telemetry_configured = true;
        node.telemetry_rate_hz = 50;
        node.telemetry_expected_since = Some(now);
        node.last_telemetry = Some(now);
        assert!(node.online_at(now + Duration::from_millis(500)));
        assert!(!node.online_at(now + Duration::from_millis(501)));

        node.telemetry_enabled = false;
        node.last_seen = now;
        node.missed_pings = 2;
        assert!(node.online_at(now + Duration::from_secs(2)));
        node.missed_pings = 3;
        assert!(!node.online_at(now + Duration::from_secs(2)));
    }

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
        assert_eq!(tuning.torque_limit_nm, 0.45);
        assert_eq!(
            tuning.max_torque_permille,
            crate::smartknob::DEFAULT_MAX_TORQUE_PERMILLE
        );
        assert_eq!(tuning.friction_compensation, cfg.friction_compensation);
        assert_eq!(tuning.click_torque_nm, cfg.click_torque_nm);
    }
}
