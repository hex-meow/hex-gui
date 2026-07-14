//! Direct-CANopen debug session for one lift driver.
//!
//! The React view is intentionally not a real-time controller. This module owns
//! heartbeat/TPDO reception, SDO sequencing, velocity-watchdog refresh, and the
//! safe detach path. Opening the tool never makes the node Operational.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use can_transport::{CanBus, CanFilter, CanFrame, CanRx, FrameKind};
use hex_motor::canopen::sdo;
use hex_motor::cia402::Cia402Manager;
use serde::Serialize;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

const MODE_COMMAND: u16 = 0x4401;
const MODE_DISPLAY: u16 = 0x4402;
const STATUS_WORD: u16 = 0x4403;
const DETAILED_FAULT: u16 = 0x453F;
const ACTUAL_POSITION: u16 = 0x4564;
const ACTUAL_VELOCITY: u16 = 0x456C;
const TARGET_POSITION: u16 = 0x457A;
const TARGET_VELOCITY: u16 = 0x45FF;
const EFFECTIVE_PARAMS: u16 = 0x4600;
const DIAGNOSTICS: u16 = 0x4601;
const SAMPLE_TIMESTAMP: u16 = 0x4713;

const MODE_DISABLED: u8 = 0;
const MODE_POSITION: u8 = 1;
const MODE_VELOCITY: u8 = 2;
const MODE_HOMING: u8 = 5;
const MODE_CLEAR_FAULT: u8 = 0xFF;

const STATUS_CONFIG_VALID: u8 = 1 << 0;
const STATUS_HOMED: u8 = 1 << 1;
const STATUS_FAULT: u8 = 1 << 7;

const NMT_OPERATIONAL: u8 = 0x05;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(1_200);
const TPDO_TIMEOUT: Duration = Duration::from_millis(500);
const VELOCITY_LEASE_TIMEOUT: Duration = Duration::from_millis(250);
const HOST_MIN_VELOCITY_WATCHDOG_MS: u16 = 30;

#[derive(Debug, Clone, Serialize)]
pub struct LiftState {
    pub running: bool,
    pub node_id: u8,
    pub online: bool,
    pub tpdo1_fresh: bool,
    pub tpdo2_fresh: bool,
    pub nmt_state: u8,
    pub device_name: String,
    pub firmware_version: String,

    pub nameplate_kind: u8,
    pub model: String,
    pub layout_id: u32,
    pub nameplate_used: u8,
    pub nameplate_crc32: u32,
    pub nameplate_crc_ok: bool,

    pub mode_command: u8,
    pub mode_display: u8,
    pub status_word: u8,
    pub detailed_fault: u16,
    pub actual_position_m: f32,
    pub actual_velocity_mps: f32,
    pub sample_timestamp_us: u16,

    pub bus_voltage_v: f32,
    pub bus_current_a: f32,
    pub bus_power_w: f32,
    pub ina_temperature_c: f32,
    pub encoder_count: i32,
    pub duty_command_permille: i16,
    pub control_loop_hz: u16,

    pub counts_per_meter: f32,
    pub position_min_m: f32,
    pub position_max_m: f32,
    pub velocity_max_mps: f32,
    pub acceleration_limit_mps2: f32,
    pub homing_speed_mps: f32,
    pub homing_stall_bus_current_a: f32,
    pub homing_stall_speed_mps: f32,
    pub homing_arm_time_ms: u16,
    pub homing_confirm_time_ms: u16,
    pub homing_timeout_ms: u32,
    pub velocity_watchdog_time_ms: u16,
    pub position_tolerance_m: f32,
    pub max_bus_current_a: f32,
    pub bus_voltage_min_v: f32,
    pub bus_voltage_max_v: f32,

    pub last_error: Option<String>,
}

impl Default for LiftState {
    fn default() -> Self {
        Self {
            running: false,
            node_id: 0,
            online: false,
            tpdo1_fresh: false,
            tpdo2_fresh: false,
            nmt_state: 0,
            device_name: String::new(),
            firmware_version: String::new(),
            nameplate_kind: 0,
            model: String::new(),
            layout_id: 0,
            nameplate_used: 0,
            nameplate_crc32: 0,
            nameplate_crc_ok: false,
            mode_command: 0,
            mode_display: 0,
            status_word: 0,
            detailed_fault: 0,
            actual_position_m: 0.0,
            actual_velocity_mps: 0.0,
            sample_timestamp_us: 0,
            bus_voltage_v: 0.0,
            bus_current_a: 0.0,
            bus_power_w: 0.0,
            ina_temperature_c: 0.0,
            encoder_count: 0,
            duty_command_permille: 0,
            control_loop_hz: 0,
            counts_per_meter: 0.0,
            position_min_m: 0.0,
            position_max_m: 0.0,
            velocity_max_mps: 0.0,
            acceleration_limit_mps2: 0.0,
            homing_speed_mps: 0.0,
            homing_stall_bus_current_a: 0.0,
            homing_stall_speed_mps: 0.0,
            homing_arm_time_ms: 0,
            homing_confirm_time_ms: 0,
            homing_timeout_ms: 0,
            velocity_watchdog_time_ms: 0,
            position_tolerance_m: 0.0,
            max_bus_current_a: 0.0,
            bus_voltage_min_v: 0.0,
            bus_voltage_max_v: 0.0,
            last_error: None,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct VelocityDemand {
    generation: u64,
    active: bool,
    target_mps: f32,
    period_ms: u64,
    lease_deadline: Option<Instant>,
}

#[derive(Default)]
struct TelemetryFreshness {
    last_valid_heartbeat: Option<Instant>,
    last_valid_tpdo1: Option<Instant>,
    last_valid_tpdo2: Option<Instant>,
}

/// One attached lift node. Only one may own motion commands at a time.
pub struct LiftSession {
    node_id: u8,
    bus: Arc<dyn CanBus>,
    sdo_timeout: Option<Duration>,
    sdo_gate: Arc<AsyncMutex<()>>,
    state: Arc<StdMutex<LiftState>>,
    freshness: Arc<StdMutex<TelemetryFreshness>>,
    velocity: Arc<StdMutex<VelocityDemand>>,
    accepting_commands: AtomicBool,
    running: Arc<AtomicBool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl LiftSession {
    /// Attach to a node without changing its NMT state.
    pub async fn start(mgr: Arc<Cia402Manager>, node_id: u8) -> anyhow::Result<Self> {
        if !(1..=127).contains(&node_id) {
            anyhow::bail!("lift node-id must be in 1..=127");
        }

        // The shared manager automatically identifies a newly heartbeating
        // node over the same default SDO channel. Finish that manager-owned
        // operation before starting Lift's serialized direct-SDO session.
        // A successful identify changes the manager entry out of Unknown, so
        // discovery will not start another background identify every heartbeat.
        let identify_deadline = Instant::now() + mgr.options().sdo_timeout * 3;
        loop {
            match mgr.identify(node_id).await {
                Ok(()) => break,
                Err(error)
                    if error.to_string().contains("another exclusive op")
                        && Instant::now() < identify_deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => {
                    anyhow::bail!("settle manager identification for lift 0x{node_id:02X}: {error}")
                }
            }
        }

        let bus = mgr.bus();
        let heartbeat_rx = bus
            .subscribe(CanFilter::exact_standard(heartbeat_cob_id(node_id)))
            .await
            .map_err(|e| anyhow::anyhow!("subscribe lift heartbeat: {e}"))?;
        let tpdo1_rx = bus
            .subscribe(CanFilter::exact_standard(tpdo1_cob_id(node_id)))
            .await
            .map_err(|e| anyhow::anyhow!("subscribe lift TPDO1: {e}"))?;
        let tpdo2_rx = bus
            .subscribe(CanFilter::exact_standard(tpdo2_cob_id(node_id)))
            .await
            .map_err(|e| anyhow::anyhow!("subscribe lift TPDO2: {e}"))?;

        let state = Arc::new(StdMutex::new(LiftState {
            running: true,
            node_id,
            ..Default::default()
        }));
        let freshness = Arc::new(StdMutex::new(TelemetryFreshness::default()));
        let velocity = Arc::new(StdMutex::new(VelocityDemand::default()));
        let running = Arc::new(AtomicBool::new(true));
        let sdo_gate = Arc::new(AsyncMutex::new(()));

        let session = Self {
            node_id,
            bus,
            sdo_timeout: Some(mgr.options().sdo_timeout),
            sdo_gate,
            state,
            freshness,
            velocity,
            accepting_commands: AtomicBool::new(true),
            running,
            tasks: StdMutex::new(Vec::new()),
        };

        if let Err(error) = session.refresh_all().await {
            session.running.store(false, Ordering::SeqCst);
            let _ = send_nmt(&session.bus, 0x80, node_id).await;
            return Err(error);
        }

        *session.tasks.lock().unwrap() = vec![
            tokio::spawn(heartbeat_loop(
                heartbeat_rx,
                node_id,
                session.state.clone(),
                session.freshness.clone(),
                session.running.clone(),
            )),
            tokio::spawn(tpdo1_loop(
                tpdo1_rx,
                node_id,
                session.state.clone(),
                session.freshness.clone(),
                session.running.clone(),
            )),
            tokio::spawn(tpdo2_loop(
                tpdo2_rx,
                node_id,
                session.state.clone(),
                session.freshness.clone(),
                session.running.clone(),
            )),
            tokio::spawn(velocity_loop(
                session.bus.clone(),
                node_id,
                session.velocity.clone(),
                session.state.clone(),
                session.freshness.clone(),
                session.running.clone(),
            )),
        ];

        log::info!("Lift: attached node 0x{node_id:02X} without changing NMT state");
        Ok(session)
    }

    pub fn state(&self) -> LiftState {
        self.sync_freshness();
        self.state.lock().unwrap().clone()
    }

    pub async fn refresh(&self) -> anyhow::Result<LiftState> {
        let _guard = self.sdo_gate.lock().await;
        self.require_accepting_commands()?;
        match self.refresh_live_locked().await {
            Ok(()) => {
                self.sync_freshness();
                let mut state = self.state.lock().unwrap();
                state.last_error = None;
                Ok(state.clone())
            }
            Err(error) => {
                self.record_error(&error);
                Err(error)
            }
        }
    }

    pub async fn set_nmt(&self, command: &str) -> anyhow::Result<()> {
        let cs = match command {
            "operational" | "start" => 0x01,
            "pre_operational" | "preop" => 0x80,
            "stopped" | "stop" => 0x02,
            other => anyhow::bail!("unknown NMT command {other:?}"),
        };

        // Leaving Operational is a safety action, so send it before waiting
        // for any SDO/command serialization. Repeat it after taking the gate
        // so an older in-flight Start command cannot win the race.
        if cs != 0x01 {
            self.cancel_velocity();
            send_nmt(&self.bus, cs, self.node_id).await?;
            let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;
        }
        let _guard = self.sdo_gate.lock().await;
        self.require_accepting_commands()?;
        send_nmt(&self.bus, cs, self.node_id).await?;
        self.clear_error();
        Ok(())
    }

    /// Directed emergency-safe disable: NMT Stop first, then clear the mode.
    pub async fn disable(&self) -> anyhow::Result<()> {
        send_nmt(&self.bus, 0x02, self.node_id).await?;
        self.cancel_velocity();
        let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;
        let _guard = self.sdo_gate.lock().await;
        // Override any older Start/mode command which completed after the
        // first emergency Stop, then move to Pre-op where SDO is available.
        send_nmt(&self.bus, 0x02, self.node_id).await?;
        send_nmt(&self.bus, 0x80, self.node_id).await?;
        self.wait_for_nmt(0x7F, HEARTBEAT_TIMEOUT).await?;
        sdo::download_u8(
            &*self.bus,
            self.node_id,
            MODE_COMMAND,
            0,
            MODE_DISABLED,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("disable mode write after NMT Stop: {e}"))?;
        send_nmt(&self.bus, 0x02, self.node_id).await?;
        self.wait_for_nmt(0x04, HEARTBEAT_TIMEOUT).await?;
        self.clear_error();
        Ok(())
    }

    pub async fn home(&self) -> anyhow::Result<()> {
        self.cancel_velocity();
        let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;
        let _guard = self.sdo_gate.lock().await;
        self.require_accepting_commands()?;
        self.refresh_live_locked().await?;
        self.require_motion_gate(false)?;
        sdo::download_u8(
            &*self.bus,
            self.node_id,
            MODE_COMMAND,
            0,
            MODE_HOMING,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("start homing: {e}"))?;
        self.clear_error();
        Ok(())
    }

    pub async fn clear_fault(&self) -> anyhow::Result<()> {
        self.cancel_velocity();
        let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;
        let _guard = self.sdo_gate.lock().await;
        self.require_accepting_commands()?;
        self.refresh_live_locked().await?;
        if self.state.lock().unwrap().status_word & STATUS_FAULT == 0 {
            anyhow::bail!("lift has no latched fault to clear");
        }
        sdo::download_u8(
            &*self.bus,
            self.node_id,
            MODE_COMMAND,
            0,
            MODE_CLEAR_FAULT,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("clear lift fault: {e}"))?;
        self.clear_error();
        Ok(())
    }

    pub async fn set_position(&self, position_m: f32) -> anyhow::Result<()> {
        if !position_m.is_finite() {
            anyhow::bail!("position target must be finite");
        }
        self.cancel_velocity();
        let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;
        let _guard = self.sdo_gate.lock().await;
        self.require_accepting_commands()?;
        self.refresh_live_locked().await?;
        self.require_motion_gate(true)?;
        {
            let state = self.state.lock().unwrap();
            if position_m < state.position_min_m || position_m > state.position_max_m {
                anyhow::bail!(
                    "position {position_m} m is outside [{}, {}] m",
                    state.position_min_m,
                    state.position_max_m
                );
            }
        }

        // Position is an autonomous goal. Validate/store it first, then enter
        // Position. Detach/Disable/NMT loss explicitly cancels it.
        let already_in_position = self.state.lock().unwrap().mode_display == MODE_POSITION;
        sdo::download_f32(
            &*self.bus,
            self.node_id,
            TARGET_POSITION,
            0,
            position_m,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("write lift position target: {e}"))?;
        if !already_in_position {
            sdo::download_u8(
                &*self.bus,
                self.node_id,
                MODE_COMMAND,
                0,
                MODE_POSITION,
                self.sdo_timeout,
            )
            .await
            .map_err(|e| anyhow::anyhow!("enter lift Position mode: {e}"))?;
            self.wait_for_mode(MODE_POSITION, Duration::from_millis(150))
                .await?;
        }
        self.clear_error();
        Ok(())
    }

    pub async fn set_velocity(&self, velocity_mps: f32) -> anyhow::Result<()> {
        if !velocity_mps.is_finite() {
            anyhow::bail!("velocity target must be finite");
        }

        // A zero command is the deadman-release path: always permit it, stop
        // watchdog streaming, and return to Disabled without changing NMT.
        if velocity_mps == 0.0 {
            self.cancel_velocity();
            let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;
            let _guard = self.sdo_gate.lock().await;
            if let Err(error) = sdo::download_u8(
                &*self.bus,
                self.node_id,
                MODE_COMMAND,
                0,
                MODE_DISABLED,
                self.sdo_timeout,
            )
            .await
            {
                let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
                anyhow::bail!("deadman Disabled write failed; directed NMT Stop sent: {error}");
            }
            self.clear_error();
            return Ok(());
        }

        // Invalidate the previous stream before performing any mode/target
        // I/O. A concurrent deadman release increments this generation again,
        // so this request can no longer arm after the operator has let go.
        let generation = self.begin_velocity_request();
        let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;
        let _guard = self.sdo_gate.lock().await;
        self.require_accepting_commands()?;
        if !self.velocity_request_is_current(generation) {
            return Ok(());
        }
        self.refresh_live_locked().await?;
        self.require_motion_gate(true)?;
        let watchdog_ms = {
            let state = self.state.lock().unwrap();
            if velocity_mps.abs() > state.velocity_max_mps {
                anyhow::bail!(
                    "velocity magnitude {} m/s exceeds {} m/s",
                    velocity_mps.abs(),
                    state.velocity_max_mps
                );
            }
            if state.velocity_watchdog_time_ms < HOST_MIN_VELOCITY_WATCHDOG_MS {
                anyhow::bail!(
                    "velocity watchdog {} ms is too short for the desktop host (minimum {HOST_MIN_VELOCITY_WATCHDOG_MS} ms)",
                    state.velocity_watchdog_time_ms
                );
            }
            state.velocity_watchdog_time_ms
        };

        sdo::download_u8(
            &*self.bus,
            self.node_id,
            MODE_COMMAND,
            0,
            MODE_VELOCITY,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("enter lift Velocity mode: {e}"))?;

        // The firmware deliberately requires a target write after it observes
        // the mode transition. Confirm ModeDisplay instead of relying on a
        // fixed delay, and re-check the operator generation before the target.
        self.wait_for_mode(MODE_VELOCITY, Duration::from_millis(150))
            .await?;
        if !self.velocity_request_is_current(generation) {
            return Ok(());
        }
        sdo::download_f32(
            &*self.bus,
            self.node_id,
            TARGET_VELOCITY,
            0,
            velocity_mps,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("write lift velocity target: {e}"))?;

        let watchdog = u64::from(watchdog_ms);
        let period_ms = (watchdog / 3).clamp(10, 50);
        let canceled_after_target = {
            let mut demand = self.velocity.lock().unwrap();
            if demand.generation != generation {
                true
            } else {
                demand.active = true;
                demand.target_mps = velocity_mps;
                demand.period_ms = period_ms;
                demand.lease_deadline = Some(Instant::now() + VELOCITY_LEASE_TIMEOUT);
                false
            }
        };
        if canceled_after_target {
            let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
            return Ok(());
        }
        self.clear_error();
        Ok(())
    }

    /// Renew the short WebView/operator lease without performing SDO I/O.
    /// The Rust task remains the sole owner of firmware-watchdog RPDO timing.
    pub fn renew_velocity_lease(&self) -> anyhow::Result<()> {
        self.require_accepting_commands()?;
        self.require_motion_gate(true)?;
        let mut demand = self.velocity.lock().unwrap();
        if !demand.active {
            anyhow::bail!("velocity deadman is not active");
        }
        demand.lease_deadline = Some(Instant::now() + VELOCITY_LEASE_TIMEOUT);
        Ok(())
    }

    /// Stop motion, cancel autonomous goals, and leave the node safely Pre-op.
    /// Success means a Pre-operational heartbeat and a Disabled command
    /// readback were both observed; on failure the session remains available
    /// for a retry. ModeDisplay may legitimately remain CONFIG_INVALID/Fault.
    pub async fn stop(&self) -> anyhow::Result<()> {
        self.accepting_commands.store(false, Ordering::SeqCst);
        self.cancel_velocity();
        // NMT Stop is deliberately the first awaited bus action and does not
        // depend on SDO health or the normal command serialization gate.
        let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
        let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;

        let result = async {
            let _guard = self.sdo_gate.lock().await;
            // Override any command which was already in flight when closing
            // began, then enter Pre-op so the SDO server is available.
            send_nmt(&self.bus, 0x02, self.node_id).await?;
            let _ = send_rpdo_velocity(&self.bus, self.node_id, 0.0).await;
            send_nmt(&self.bus, 0x80, self.node_id).await?;
            self.wait_for_nmt(0x7F, HEARTBEAT_TIMEOUT).await?;
            sdo::download_u8(
                &*self.bus,
                self.node_id,
                MODE_COMMAND,
                0,
                MODE_DISABLED,
                self.sdo_timeout,
            )
            .await
            .map_err(|e| anyhow::anyhow!("detach Disabled write: {e}"))?;
            let mode_command =
                sdo::upload_u8(&*self.bus, self.node_id, MODE_COMMAND, 0, self.sdo_timeout)
                    .await
                    .map_err(|e| anyhow::anyhow!("detach Disabled readback: {e}"))?;
            if mode_command != MODE_DISABLED {
                anyhow::bail!("detach Disabled readback mismatch: 0x{mode_command:02X}");
            }
            self.state.lock().unwrap().mode_command = mode_command;
            anyhow::Ok(())
        }
        .await;

        if let Err(error) = result {
            let message = format!(
                "STOP UNCONFIRMED for lift 0x{:02X}; keep the session and use physical power removal if motion is possible: {error}",
                self.node_id
            );
            self.state.lock().unwrap().last_error = Some(message.clone());
            return Err(anyhow::anyhow!(message));
        }

        self.running.store(false, Ordering::SeqCst);
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        let mut state = self.state.lock().unwrap();
        state.running = false;
        state.last_error = None;
        log::info!(
            "Lift: detached node 0x{:02X} in Pre-operational",
            self.node_id
        );
        Ok(())
    }

    async fn refresh_all(&self) -> anyhow::Result<()> {
        let _guard = self.sdo_gate.lock().await;
        self.refresh_static_locked().await?;
        self.refresh_live_locked().await?;
        let mut state = self.state.lock().unwrap();
        state.last_error = None;
        Ok(())
    }

    async fn refresh_static_locked(&self) -> anyhow::Result<()> {
        let bus = &*self.bus;
        let nid = self.node_id;
        let timeout = self.sdo_timeout;

        let device_name = sdo::upload_string(bus, nid, 0x1008, 0, timeout).await?;
        let firmware_version = sdo::upload_string(bus, nid, 0x100A, 0, timeout).await?;
        let nameplate_kind = sdo::upload_u8(bus, nid, 0x5F00, 1, timeout).await?;
        let mut model_raw = Vec::with_capacity(32);
        for sub in 1..=4 {
            model_raw.extend(sdo::upload(bus, nid, 0x5F01, sub, timeout).await?);
        }
        model_raw.truncate(32);
        let model_end = model_raw
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(model_raw.len());
        let model = String::from_utf8_lossy(&model_raw[..model_end]).into_owned();
        let layout_id = sdo::upload_u32(bus, nid, 0x5F02, 1, timeout).await?;
        let nameplate_used = sdo::upload_u8(bus, nid, 0x5F02, 2, timeout).await?;
        let nameplate_crc32 = sdo::upload_u32(bus, nid, 0x5F02, 3, timeout).await?;

        let mut payload = [0u32; 64];
        let packed_count = usize::from(nameplate_used).min(payload.len()).div_ceil(2);
        for packed_index in 0..packed_count {
            let raw = sdo::upload(bus, nid, 0x5F03, (packed_index + 1) as u8, timeout).await?;
            if raw.len() < 8 {
                anyhow::bail!(
                    "nameplate payload 0x5F03:{:02X} returned {} bytes",
                    packed_index + 1,
                    raw.len()
                );
            }
            let packed = u64::from_le_bytes(raw[..8].try_into().unwrap());
            payload[packed_index * 2] = packed as u32;
            if packed_index * 2 + 1 < payload.len() {
                payload[packed_index * 2 + 1] = (packed >> 32) as u32;
            }
        }
        let nameplate_crc_ok = nameplate_used as usize <= payload.len()
            && lift_nameplate_crc(layout_id, nameplate_used, &payload) == nameplate_crc32;

        {
            let mut state = self.state.lock().unwrap();
            state.device_name = device_name;
            state.firmware_version = firmware_version;
            state.nameplate_kind = nameplate_kind;
            state.model = model;
            state.layout_id = layout_id;
            state.nameplate_used = nameplate_used;
            state.nameplate_crc32 = nameplate_crc32;
            state.nameplate_crc_ok = nameplate_crc_ok;
        }

        self.refresh_effective_locked().await
    }

    async fn refresh_effective_locked(&self) -> anyhow::Result<()> {
        let bus = &*self.bus;
        let nid = self.node_id;
        let timeout = self.sdo_timeout;
        let counts_per_meter = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 1, timeout).await?;
        let position_min_m = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 2, timeout).await?;
        let position_max_m = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 3, timeout).await?;
        let velocity_max_mps = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 4, timeout).await?;
        let acceleration_limit_mps2 =
            sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 5, timeout).await?;
        let homing_speed_mps = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 6, timeout).await?;
        let homing_stall_bus_current_a =
            sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 7, timeout).await?;
        let homing_stall_speed_mps =
            sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 8, timeout).await?;
        let homing_arm_time_ms = sdo::upload_u16(bus, nid, EFFECTIVE_PARAMS, 9, timeout).await?;
        let homing_confirm_time_ms =
            sdo::upload_u16(bus, nid, EFFECTIVE_PARAMS, 10, timeout).await?;
        let homing_timeout_ms = sdo::upload_u32(bus, nid, EFFECTIVE_PARAMS, 11, timeout).await?;
        let velocity_watchdog_time_ms =
            sdo::upload_u16(bus, nid, EFFECTIVE_PARAMS, 12, timeout).await?;
        let position_tolerance_m = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 13, timeout).await?;
        let max_bus_current_a = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 14, timeout).await?;
        let bus_voltage_min_v = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 15, timeout).await?;
        let bus_voltage_max_v = sdo::upload_f32(bus, nid, EFFECTIVE_PARAMS, 16, timeout).await?;

        let mut state = self.state.lock().unwrap();
        state.counts_per_meter = counts_per_meter;
        state.position_min_m = position_min_m;
        state.position_max_m = position_max_m;
        state.velocity_max_mps = velocity_max_mps;
        state.acceleration_limit_mps2 = acceleration_limit_mps2;
        state.homing_speed_mps = homing_speed_mps;
        state.homing_stall_bus_current_a = homing_stall_bus_current_a;
        state.homing_stall_speed_mps = homing_stall_speed_mps;
        state.homing_arm_time_ms = homing_arm_time_ms;
        state.homing_confirm_time_ms = homing_confirm_time_ms;
        state.homing_timeout_ms = homing_timeout_ms;
        state.velocity_watchdog_time_ms = velocity_watchdog_time_ms;
        state.position_tolerance_m = position_tolerance_m;
        state.max_bus_current_a = max_bus_current_a;
        state.bus_voltage_min_v = bus_voltage_min_v;
        state.bus_voltage_max_v = bus_voltage_max_v;
        Ok(())
    }

    async fn refresh_live_locked(&self) -> anyhow::Result<()> {
        let bus = &*self.bus;
        let nid = self.node_id;
        let timeout = self.sdo_timeout;
        let mode_command = sdo::upload_u8(bus, nid, MODE_COMMAND, 0, timeout).await?;
        let mode_display = sdo::upload_u8(bus, nid, MODE_DISPLAY, 0, timeout).await?;
        let status_word = sdo::upload_u8(bus, nid, STATUS_WORD, 0, timeout).await?;
        let detailed_fault = sdo::upload_u16(bus, nid, DETAILED_FAULT, 0, timeout).await?;
        let actual_position_m = sdo::upload_f32(bus, nid, ACTUAL_POSITION, 0, timeout).await?;
        let actual_velocity_mps = sdo::upload_f32(bus, nid, ACTUAL_VELOCITY, 0, timeout).await?;
        let sample_timestamp_us = sdo::upload_u16(bus, nid, SAMPLE_TIMESTAMP, 0, timeout).await?;
        let bus_voltage_v = sdo::upload_f32(bus, nid, DIAGNOSTICS, 1, timeout).await?;
        let bus_current_a = sdo::upload_f32(bus, nid, DIAGNOSTICS, 2, timeout).await?;
        let bus_power_w = sdo::upload_f32(bus, nid, DIAGNOSTICS, 3, timeout).await?;
        let ina_temperature_c = sdo::upload_f32(bus, nid, DIAGNOSTICS, 4, timeout).await?;
        let encoder_count = read_i32(bus, nid, DIAGNOSTICS, 5, timeout).await?;
        let duty_command_permille = read_i16(bus, nid, DIAGNOSTICS, 6, timeout).await?;
        let control_loop_hz = sdo::upload_u16(bus, nid, DIAGNOSTICS, 7, timeout).await?;

        let mut state = self.state.lock().unwrap();
        state.mode_command = mode_command;
        state.mode_display = mode_display;
        state.status_word = status_word;
        state.detailed_fault = detailed_fault;
        state.actual_position_m = actual_position_m;
        state.actual_velocity_mps = actual_velocity_mps;
        state.sample_timestamp_us = sample_timestamp_us;
        state.bus_voltage_v = bus_voltage_v;
        state.bus_current_a = bus_current_a;
        state.bus_power_w = bus_power_w;
        state.ina_temperature_c = ina_temperature_c;
        state.encoder_count = encoder_count;
        state.duty_command_permille = duty_command_permille;
        state.control_loop_hz = control_loop_hz;
        Ok(())
    }

    fn require_motion_gate(&self, needs_homed: bool) -> anyhow::Result<()> {
        self.sync_freshness();
        let state = self.state.lock().unwrap();
        if !state.online {
            anyhow::bail!("lift heartbeat is stale");
        }
        if !state.tpdo1_fresh || !state.tpdo2_fresh {
            anyhow::bail!(
                "lift telemetry is stale (TPDO1={}, TPDO2={})",
                state.tpdo1_fresh,
                state.tpdo2_fresh
            );
        }
        if state.nmt_state != NMT_OPERATIONAL {
            anyhow::bail!("lift is not NMT Operational");
        }
        if state.status_word & STATUS_CONFIG_VALID == 0 {
            anyhow::bail!("lift CONFIG_VALID is clear");
        }
        if state.status_word & STATUS_FAULT != 0 || state.detailed_fault != 0 {
            anyhow::bail!("lift has fault 0x{:04X}", state.detailed_fault);
        }
        if needs_homed && state.status_word & STATUS_HOMED == 0 {
            anyhow::bail!("lift is not homed");
        }
        Ok(())
    }

    fn require_accepting_commands(&self) -> anyhow::Result<()> {
        if !self.accepting_commands.load(Ordering::SeqCst) {
            anyhow::bail!("lift session is closing after a Stop request");
        }
        Ok(())
    }

    async fn wait_for_nmt(&self, expected: u8, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            self.sync_freshness();
            let (online, actual) = {
                let state = self.state.lock().unwrap();
                (state.online, state.nmt_state)
            };
            if online && actual == expected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "NMT confirmation timed out: expected 0x{expected:02X}, latest 0x{actual:02X}, online={online}"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_mode(&self, expected: u8, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let actual =
                sdo::upload_u8(&*self.bus, self.node_id, MODE_DISPLAY, 0, self.sdo_timeout)
                    .await
                    .map_err(|e| anyhow::anyhow!("read ModeDisplay confirmation: {e}"))?;
            self.state.lock().unwrap().mode_display = actual;
            if actual == expected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "ModeDisplay confirmation timed out: expected 0x{expected:02X}, latest 0x{actual:02X}"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn begin_velocity_request(&self) -> u64 {
        let mut demand = self.velocity.lock().unwrap();
        let generation = demand.generation.wrapping_add(1);
        *demand = VelocityDemand {
            generation,
            ..Default::default()
        };
        generation
    }

    fn velocity_request_is_current(&self, generation: u64) -> bool {
        self.velocity.lock().unwrap().generation == generation
    }

    fn cancel_velocity(&self) {
        let mut demand = self.velocity.lock().unwrap();
        let generation = demand.generation.wrapping_add(1);
        *demand = VelocityDemand {
            generation,
            ..Default::default()
        };
    }

    fn clear_error(&self) {
        self.state.lock().unwrap().last_error = None;
    }

    fn record_error(&self, error: &anyhow::Error) {
        self.state.lock().unwrap().last_error = Some(error.to_string());
    }

    fn sync_freshness(&self) {
        sync_freshness_state(&self.freshness, &self.state);
    }
}

fn is_fresh(last_valid: Option<Instant>, timeout: Duration, now: Instant) -> bool {
    last_valid.is_some_and(|instant| now.saturating_duration_since(instant) < timeout)
}

fn sync_freshness_state(freshness: &StdMutex<TelemetryFreshness>, state: &StdMutex<LiftState>) {
    let now = Instant::now();
    let (online, tpdo1_fresh, tpdo2_fresh) = {
        let freshness = freshness.lock().unwrap();
        (
            is_fresh(freshness.last_valid_heartbeat, HEARTBEAT_TIMEOUT, now),
            is_fresh(freshness.last_valid_tpdo1, TPDO_TIMEOUT, now),
            is_fresh(freshness.last_valid_tpdo2, TPDO_TIMEOUT, now),
        )
    };
    let mut state = state.lock().unwrap();
    state.online = online;
    state.tpdo1_fresh = tpdo1_fresh;
    state.tpdo2_fresh = tpdo2_fresh;
}

async fn heartbeat_loop(
    mut rx: Box<dyn CanRx>,
    node_id: u8,
    state: Arc<StdMutex<LiftState>>,
    freshness: Arc<StdMutex<TelemetryFreshness>>,
    running: Arc<AtomicBool>,
) {
    let mut valid_deadline = Instant::now() + HEARTBEAT_TIMEOUT;
    while running.load(Ordering::SeqCst) {
        let remaining = valid_deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(frame)) => {
                if frame.kind() == FrameKind::Data && frame.dlc() == 1 {
                    let now = Instant::now();
                    valid_deadline = now + HEARTBEAT_TIMEOUT;
                    freshness.lock().unwrap().last_valid_heartbeat = Some(now);
                    let nmt_state = frame.data()[0];
                    let mut state = state.lock().unwrap();
                    state.online = true;
                    state.nmt_state = nmt_state;
                }
            }
            Ok(Err(error)) => {
                freshness.lock().unwrap().last_valid_heartbeat = None;
                let mut state = state.lock().unwrap();
                state.online = false;
                state.last_error = Some(format!("heartbeat receive: {error}"));
                break;
            }
            Err(_) => {
                state.lock().unwrap().online = false;
                valid_deadline = Instant::now() + HEARTBEAT_TIMEOUT;
            }
        }
    }
    log::info!("Lift 0x{node_id:02X}: heartbeat loop stopped");
}

async fn tpdo1_loop(
    mut rx: Box<dyn CanRx>,
    node_id: u8,
    state: Arc<StdMutex<LiftState>>,
    freshness: Arc<StdMutex<TelemetryFreshness>>,
    running: Arc<AtomicBool>,
) {
    let mut valid_deadline = Instant::now() + TPDO_TIMEOUT;
    while running.load(Ordering::SeqCst) {
        let remaining = valid_deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(frame)) => {
                if frame.kind() == FrameKind::Data {
                    if let Some((position, timestamp, mode, status)) = parse_tpdo1(frame.data()) {
                        let now = Instant::now();
                        valid_deadline = now + TPDO_TIMEOUT;
                        freshness.lock().unwrap().last_valid_tpdo1 = Some(now);
                        let mut state = state.lock().unwrap();
                        state.tpdo1_fresh = true;
                        state.actual_position_m = position;
                        state.sample_timestamp_us = timestamp;
                        state.mode_display = mode;
                        state.status_word = status;
                    }
                }
            }
            Ok(Err(error)) => {
                freshness.lock().unwrap().last_valid_tpdo1 = None;
                let mut state = state.lock().unwrap();
                state.tpdo1_fresh = false;
                state.last_error = Some(format!("TPDO1 receive: {error}"));
                break;
            }
            Err(_) => {
                state.lock().unwrap().tpdo1_fresh = false;
                valid_deadline = Instant::now() + TPDO_TIMEOUT;
            }
        }
    }
    log::info!("Lift 0x{node_id:02X}: TPDO1 loop stopped");
}

async fn tpdo2_loop(
    mut rx: Box<dyn CanRx>,
    node_id: u8,
    state: Arc<StdMutex<LiftState>>,
    freshness: Arc<StdMutex<TelemetryFreshness>>,
    running: Arc<AtomicBool>,
) {
    let mut valid_deadline = Instant::now() + TPDO_TIMEOUT;
    while running.load(Ordering::SeqCst) {
        let remaining = valid_deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(frame)) => {
                if frame.kind() == FrameKind::Data {
                    if let Some((voltage, current)) = parse_tpdo2(frame.data()) {
                        let now = Instant::now();
                        valid_deadline = now + TPDO_TIMEOUT;
                        freshness.lock().unwrap().last_valid_tpdo2 = Some(now);
                        let mut state = state.lock().unwrap();
                        state.tpdo2_fresh = true;
                        state.bus_voltage_v = voltage;
                        state.bus_current_a = current;
                    }
                }
            }
            Ok(Err(error)) => {
                freshness.lock().unwrap().last_valid_tpdo2 = None;
                let mut state = state.lock().unwrap();
                state.tpdo2_fresh = false;
                state.last_error = Some(format!("TPDO2 receive: {error}"));
                break;
            }
            Err(_) => {
                state.lock().unwrap().tpdo2_fresh = false;
                valid_deadline = Instant::now() + TPDO_TIMEOUT;
            }
        }
    }
    log::info!("Lift 0x{node_id:02X}: TPDO2 loop stopped");
}

async fn velocity_loop(
    bus: Arc<dyn CanBus>,
    node_id: u8,
    velocity: Arc<StdMutex<VelocityDemand>>,
    state: Arc<StdMutex<LiftState>>,
    freshness: Arc<StdMutex<TelemetryFreshness>>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::SeqCst) {
        let demand = *velocity.lock().unwrap();
        if demand.active {
            sync_freshness_state(&freshness, &state);
            let blocker = {
                let state = state.lock().unwrap();
                if demand
                    .lease_deadline
                    .is_none_or(|deadline| Instant::now() >= deadline)
                {
                    Some("operator deadman lease expired")
                } else if !state.online {
                    Some("heartbeat became stale")
                } else if !state.tpdo1_fresh || !state.tpdo2_fresh {
                    Some("TPDO telemetry became stale")
                } else if state.nmt_state != NMT_OPERATIONAL {
                    Some("NMT left Operational")
                } else if state.mode_display != MODE_VELOCITY {
                    Some("ModeDisplay left Velocity")
                } else if state.status_word & STATUS_CONFIG_VALID == 0 {
                    Some("CONFIG_VALID cleared")
                } else if state.status_word & STATUS_HOMED == 0 {
                    Some("HOMED cleared")
                } else if state.status_word & STATUS_FAULT != 0 || state.detailed_fault != 0 {
                    Some("lift faulted")
                } else {
                    None
                }
            };

            if let Some(reason) = blocker {
                {
                    let mut current = velocity.lock().unwrap();
                    if current.generation == demand.generation {
                        current.active = false;
                        current.lease_deadline = None;
                    }
                }
                let _ = send_nmt(&bus, 0x02, node_id).await;
                let _ = send_rpdo_velocity(&bus, node_id, 0.0).await;
                state.lock().unwrap().last_error = Some(format!(
                    "velocity stream stopped ({reason}); directed NMT Stop sent"
                ));
                continue;
            }

            if let Err(error) = send_rpdo_velocity(&bus, node_id, demand.target_mps).await {
                {
                    let mut current = velocity.lock().unwrap();
                    if current.generation == demand.generation {
                        current.active = false;
                        current.lease_deadline = None;
                    }
                }
                state.lock().unwrap().last_error = Some(format!(
                    "velocity RPDO failed; directed NMT Stop sent: {error}"
                ));
                let _ = send_nmt(&bus, 0x02, node_id).await;
            }
            tokio::time::sleep(Duration::from_millis(demand.period_ms.max(10))).await;
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn send_nmt(bus: &Arc<dyn CanBus>, cs: u8, node_id: u8) -> anyhow::Result<()> {
    let frame = CanFrame::new_data(0x000u16, &[cs, node_id])
        .map_err(|e| anyhow::anyhow!("build NMT frame: {e}"))?;
    bus.send(frame)
        .await
        .map_err(|e| anyhow::anyhow!("send NMT: {e}"))
}

async fn send_rpdo_velocity(
    bus: &Arc<dyn CanBus>,
    node_id: u8,
    velocity_mps: f32,
) -> anyhow::Result<()> {
    let frame = CanFrame::new_data(rpdo1_cob_id(node_id), &velocity_mps.to_le_bytes())
        .map_err(|e| anyhow::anyhow!("build lift RPDO1: {e}"))?;
    bus.send(frame)
        .await
        .map_err(|e| anyhow::anyhow!("send lift RPDO1: {e}"))
}

async fn read_i32(
    bus: &(impl CanBus + ?Sized),
    nid: u8,
    index: u16,
    sub: u8,
    timeout: Option<Duration>,
) -> anyhow::Result<i32> {
    let raw = sdo::upload(bus, nid, index, sub, timeout).await?;
    if raw.len() < 4 {
        anyhow::bail!(
            "0x{index:04X}:{sub:02X}: expected i32, got {} bytes",
            raw.len()
        );
    }
    Ok(i32::from_le_bytes(raw[..4].try_into().unwrap()))
}

async fn read_i16(
    bus: &(impl CanBus + ?Sized),
    nid: u8,
    index: u16,
    sub: u8,
    timeout: Option<Duration>,
) -> anyhow::Result<i16> {
    let raw = sdo::upload(bus, nid, index, sub, timeout).await?;
    if raw.len() < 2 {
        anyhow::bail!(
            "0x{index:04X}:{sub:02X}: expected i16, got {} bytes",
            raw.len()
        );
    }
    Ok(i16::from_le_bytes(raw[..2].try_into().unwrap()))
}

fn parse_tpdo1(data: &[u8]) -> Option<(f32, u16, u8, u8)> {
    if data.len() != 8 {
        return None;
    }
    Some((
        f32::from_le_bytes(data[0..4].try_into().ok()?),
        u16::from_le_bytes(data[4..6].try_into().ok()?),
        data[6],
        data[7],
    ))
}

fn parse_tpdo2(data: &[u8]) -> Option<(f32, f32)> {
    if data.len() != 8 {
        return None;
    }
    Some((
        f32::from_le_bytes(data[0..4].try_into().ok()?),
        f32::from_le_bytes(data[4..8].try_into().ok()?),
    ))
}

fn lift_nameplate_crc(layout_id: u32, used: u8, payload: &[u32; 64]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in layout_id.to_le_bytes().into_iter().chain([used]).chain(
        payload
            .iter()
            .take(used as usize)
            .flat_map(|word| word.to_le_bytes()),
    ) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

const fn heartbeat_cob_id(node_id: u8) -> u16 {
    0x700 + node_id as u16
}

const fn tpdo1_cob_id(node_id: u8) -> u16 {
    0x180 + node_id as u16
}

const fn tpdo2_cob_id(node_id: u8) -> u16 {
    0x280 + node_id as u16
}

const fn rpdo1_cob_id(node_id: u8) -> u16 {
    0x200 + node_id as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_provisioned_lift_70_nameplate() {
        let mut payload = [0u32; 64];
        payload[0] = 0.0f32.to_bits();
        payload[1] = 1.0597f32.to_bits();
        payload[2] = 0.0f32.to_bits();
        payload[3] = 0.7f32.to_bits();
        payload[20] = 1;
        assert_eq!(lift_nameplate_crc(0x0003_0001, 21, &payload), 0x486E_E73A);
    }

    #[test]
    fn parses_locked_tpdo_layouts_and_rejects_bad_dlc() {
        let mut tpdo1 = [0u8; 8];
        tpdo1[..4].copy_from_slice(&0.25f32.to_le_bytes());
        tpdo1[4..6].copy_from_slice(&1234u16.to_le_bytes());
        tpdo1[6] = 2;
        tpdo1[7] = 0x4B;
        assert_eq!(parse_tpdo1(&tpdo1), Some((0.25, 1234, 2, 0x4B)));
        assert!(parse_tpdo1(&tpdo1[..7]).is_none());

        let mut tpdo2 = [0u8; 8];
        tpdo2[..4].copy_from_slice(&24.0f32.to_le_bytes());
        tpdo2[4..].copy_from_slice(&1.5f32.to_le_bytes());
        assert_eq!(parse_tpdo2(&tpdo2), Some((24.0, 1.5)));
        assert!(parse_tpdo2(&tpdo2[..4]).is_none());
    }

    #[test]
    fn freshness_is_measured_from_the_last_valid_frame() {
        let now = Instant::now();
        assert!(is_fresh(
            Some(now - (TPDO_TIMEOUT - Duration::from_millis(1))),
            TPDO_TIMEOUT,
            now,
        ));
        assert!(!is_fresh(
            Some(now - TPDO_TIMEOUT),
            TPDO_TIMEOUT,
            now,
        ));
        assert!(!is_fresh(None, TPDO_TIMEOUT, now));
    }

    /// Explicit commissioning smoke test for the current bridge-disabled
    /// lift firmware. It is ignored in normal CI because it owns real CAN.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires LIFT_HW_IFACE (default can0) and a lift at node 20"]
    async fn hardware_bridge_disabled_smoke() -> anyhow::Result<()> {
        let iface = std::env::var("LIFT_HW_IFACE").unwrap_or_else(|_| "can0".into());
        let node_id = std::env::var("LIFT_HW_NODE")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(20);
        let (bus, _) = crate::backend::open_bus(&iface, false).await?;
        let manager = Arc::new(Cia402Manager::new(
            bus,
            hex_motor::cia402::Cia402ManagerOptions {
                heartbeat_node_id: 0x10,
                broadcast_heartbeat: false,
                ..Default::default()
            },
        )?);
        let session = LiftSession::start(manager, node_id).await?;

        let exercise = async {
            session.set_nmt("operational").await?;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let state = session.state();
                if state.online
                    && state.tpdo1_fresh
                    && state.tpdo2_fresh
                    && state.nmt_state == NMT_OPERATIONAL
                {
                    break;
                }
                if Instant::now() >= deadline {
                    anyhow::bail!(
                        "live PDO gate did not become fresh: online={}, TPDO1={}, TPDO2={}, NMT=0x{:02X}",
                        state.online,
                        state.tpdo1_fresh,
                        state.tpdo2_fresh,
                        state.nmt_state
                    );
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            let state = session.refresh().await?;
            if state.status_word & STATUS_CONFIG_VALID != 0 {
                anyhow::bail!("commissioning firmware unexpectedly reports CONFIG_VALID");
            }
            if session.set_position(0.1).await.is_ok() {
                anyhow::bail!("Position unexpectedly passed the fail-closed gate");
            }
            anyhow::Ok(())
        }
        .await;

        // Always attempt the confirmed safe detach, even if an assertion in
        // the exercise failed after entering Operational.
        let detach = session.stop().await;
        detach?;
        exercise
    }
}
