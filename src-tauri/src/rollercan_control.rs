//! Stock-firmware Unit RollerCAN motor control.
//!
//! This module intentionally stays separate from `rollercan.rs`.  That file
//! implements the dedicated SmartKnob firmware application, while this module
//! speaks the public Unit RollerCAN motor-control protocol documented by M5Stack.
//! Both may observe the same manager-owned CAN bus, but this controller owns no
//! host-side streaming loop and never changes SmartKnob configuration.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use can_transport::{CanBus, CanFilter, CanFrame, CanId, CanIoError, CanRx, FrameKind};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

const HOST_ID: u8 = 0;
#[cfg(test)]
const DEFAULT_NODE_ID: u8 = 0xA8;
const READ_TIMEOUT: Duration = Duration::from_millis(160);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_STEP: Duration = Duration::from_millis(20);
const KNOWN_PING_PERIOD: Duration = Duration::from_secs(1);

const OD_ENABLE: u16 = 0x7004;
const OD_RUN_MODE: u16 = 0x7005;
const OD_CURRENT: u16 = 0x7006;
const OD_SPEED: u16 = 0x700A;
const OD_POSITION: u16 = 0x7016;
const OD_POSITION_MAX_CURRENT: u16 = 0x7017;
const OD_SPEED_MAX_CURRENT: u16 = 0x7018;
const OD_SPEED_READBACK: u16 = 0x7030;
const OD_POSITION_READBACK: u16 = 0x7031;
const OD_CURRENT_READBACK: u16 = 0x7032;
const OD_ENCODER: u16 = 0x7033;
const OD_VIN: u16 = 0x7034;
const OD_TEMPERATURE: u16 = 0x7035;

const POLL_INDICES: [u16; 10] = [
    OD_ENABLE,
    OD_RUN_MODE,
    OD_SPEED_READBACK,
    OD_POSITION_READBACK,
    OD_CURRENT_READBACK,
    OD_VIN,
    OD_TEMPERATURE,
    OD_ENCODER,
    OD_POSITION_MAX_CURRENT,
    OD_SPEED_MAX_CURRENT,
];

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum RollerCanControlMode {
    #[default]
    Speed,
    Position,
    Current,
    Encoder,
}

impl RollerCanControlMode {
    fn raw(self) -> i32 {
        match self {
            Self::Speed => 1,
            Self::Position => 2,
            Self::Current => 3,
            Self::Encoder => 4,
        }
    }

    fn from_raw(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::Speed),
            2 => Some(Self::Position),
            3 => Some(Self::Current),
            4 => Some(Self::Encoder),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind")]
pub enum RollerCanControlTarget {
    Speed { speed_rpm: f64 },
    Position { position_deg: f64 },
    Current { current_ma: f64 },
    Encoder { encoder_count: i32 },
}

impl RollerCanControlTarget {
    fn mode(&self) -> RollerCanControlMode {
        match self {
            Self::Speed { .. } => RollerCanControlMode::Speed,
            Self::Position { .. } => RollerCanControlMode::Position,
            Self::Current { .. } => RollerCanControlMode::Current,
            Self::Encoder { .. } => RollerCanControlMode::Encoder,
        }
    }

    fn parameter(&self) -> Result<(u16, i32)> {
        match *self {
            Self::Speed { speed_rpm } => Ok((OD_SPEED, scaled_i32(speed_rpm, 100.0, "speed")?)),
            Self::Position { position_deg } => {
                Ok((OD_POSITION, scaled_i32(position_deg, 100.0, "position")?))
            }
            Self::Current { current_ma } => {
                if !current_ma.is_finite() || !(-1200.0..=1200.0).contains(&current_ma) {
                    bail!("RollerCAN current target must be within -1200..=1200 mA");
                }
                Ok((OD_CURRENT, scaled_i32(current_ma, 100.0, "current")?))
            }
            Self::Encoder { encoder_count } => Ok((OD_ENCODER, encoder_count)),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RollerCanControlState {
    pub attached: bool,
    pub node_id: u8,
    pub online: bool,
    pub enabled: bool,
    pub mode: RollerCanControlMode,
    /// 0 = Standby, 1 = Running, 2 = Error.
    pub state_code: u8,
    /// bit0 over-voltage, bit1 stall, bit2 out-of-range.
    pub fault_bits: u8,
    pub speed_rpm: Option<f64>,
    pub position_deg: Option<f64>,
    pub current_ma: Option<f64>,
    pub voltage_v: Option<f64>,
    pub temperature_c: Option<f64>,
    pub encoder_count: Option<i32>,
    pub position_max_current_ma: Option<f64>,
    pub speed_max_current_ma: Option<f64>,
    pub feedback_age_ms: Option<u64>,
    pub feedback_rate_hz: Option<f64>,
    pub rx_count: u64,
    pub last_error: Option<String>,
}

impl RollerCanControlState {
    fn new(node_id: u8) -> Self {
        Self {
            attached: false,
            node_id,
            online: false,
            enabled: false,
            mode: RollerCanControlMode::Speed,
            state_code: 0,
            fault_bits: 0,
            speed_rpm: None,
            position_deg: None,
            current_ma: None,
            voltage_v: None,
            temperature_c: None,
            encoder_count: None,
            position_max_current_ma: None,
            speed_max_current_ma: None,
            feedback_age_ms: None,
            feedback_rate_hz: None,
            rx_count: 0,
            last_error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RollerCanControlDevice {
    pub node_id: u8,
    pub online: bool,
    pub attached: bool,
    pub enabled: bool,
    pub mode: RollerCanControlMode,
    pub state_code: u8,
    pub fault_bits: u8,
    pub feedback_age_ms: Option<u64>,
    pub rx_count: u64,
}

struct NodeState {
    view: RollerCanControlState,
    last_seen: Instant,
    last_feedback: Option<Instant>,
    feedback_times: VecDeque<Instant>,
}

impl NodeState {
    fn new(node_id: u8, now: Instant) -> Self {
        Self {
            view: RollerCanControlState::new(node_id),
            last_seen: now,
            last_feedback: None,
            feedback_times: VecDeque::with_capacity(64),
        }
    }

    fn observe(&mut self, now: Instant, feedback: bool) {
        self.last_seen = now;
        if feedback {
            self.last_feedback = Some(now);
            self.feedback_times.push_back(now);
            while self.feedback_times.len() > 64 {
                self.feedback_times.pop_front();
            }
            let cutoff = now.checked_sub(Duration::from_secs(2)).unwrap_or(now);
            while self
                .feedback_times
                .front()
                .is_some_and(|seen| *seen < cutoff)
            {
                self.feedback_times.pop_front();
            }
            self.view.rx_count = self.view.rx_count.saturating_add(1);
        }
    }

    fn snapshot(&self, attached: bool, now: Instant) -> RollerCanControlState {
        let mut view = self.view.clone();
        view.attached = attached;
        view.online = now.saturating_duration_since(self.last_seen) <= ONLINE_TIMEOUT;
        view.feedback_age_ms = self
            .last_feedback
            .map(|seen| now.saturating_duration_since(seen).as_millis() as u64);
        view.feedback_rate_hz = feedback_rate(&self.feedback_times);
        view
    }
}

#[derive(Default)]
struct SharedState {
    nodes: HashMap<u8, NodeState>,
    attached: Option<u8>,
    pending_reads: HashMap<(u8, u16), Vec<oneshot::Sender<i32>>>,
}

pub struct RollerCanControl {
    bus: Arc<dyn CanBus>,
    state: Arc<StdMutex<SharedState>>,
    transaction_gate: Arc<Mutex<()>>,
    running: Arc<AtomicBool>,
    scan_requested: Arc<AtomicBool>,
    rx_task: StdMutex<Option<JoinHandle<()>>>,
    poll_task: StdMutex<Option<JoinHandle<()>>>,
    discovery_task: StdMutex<Option<JoinHandle<()>>>,
}

impl RollerCanControl {
    pub async fn start(bus: Arc<dyn CanBus>) -> Result<Arc<Self>> {
        let rx = bus
            .subscribe(CanFilter::pass_all_extended())
            .await
            .map_err(|error| anyhow!("subscribe RollerCAN control frames: {error}"))?;
        let this = Arc::new(Self {
            bus,
            state: Arc::new(StdMutex::new(SharedState::default())),
            transaction_gate: Arc::new(Mutex::new(())),
            running: Arc::new(AtomicBool::new(true)),
            scan_requested: Arc::new(AtomicBool::new(true)),
            rx_task: StdMutex::new(None),
            poll_task: StdMutex::new(None),
            discovery_task: StdMutex::new(None),
        });

        *this.rx_task.lock().unwrap() = Some(tokio::spawn(rx_loop(
            rx,
            this.state.clone(),
            this.running.clone(),
        )));
        *this.poll_task.lock().unwrap() = Some(tokio::spawn(poll_loop(
            this.bus.clone(),
            this.state.clone(),
            this.transaction_gate.clone(),
            this.running.clone(),
        )));
        *this.discovery_task.lock().unwrap() = Some(tokio::spawn(discovery_loop(
            this.bus.clone(),
            this.state.clone(),
            this.scan_requested.clone(),
            this.running.clone(),
        )));
        Ok(this)
    }

    pub fn request_scan(&self) {
        self.scan_requested.store(true, Ordering::SeqCst);
    }

    pub fn devices(&self) -> Vec<RollerCanControlDevice> {
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        let mut devices: Vec<_> = state
            .nodes
            .iter()
            .map(|(&node_id, node)| {
                let view = node.snapshot(state.attached == Some(node_id), now);
                RollerCanControlDevice {
                    node_id,
                    online: view.online,
                    attached: view.attached,
                    enabled: view.enabled,
                    mode: view.mode,
                    state_code: view.state_code,
                    fault_bits: view.fault_bits,
                    feedback_age_ms: view.feedback_age_ms,
                    rx_count: view.rx_count,
                }
            })
            .collect();
        devices.sort_by_key(|device| device.node_id);
        devices
    }

    pub fn attached_node(&self) -> Option<u8> {
        self.state.lock().unwrap().attached
    }

    pub fn snapshot(&self, node_id: u8) -> Result<RollerCanControlState> {
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        if state.attached != Some(node_id) {
            bail!("RollerCAN 0x{node_id:02X} is not attached");
        }
        Ok(state
            .nodes
            .get(&node_id)
            .map(|node| node.snapshot(true, now))
            .unwrap_or_else(|| {
                let mut view = RollerCanControlState::new(node_id);
                view.attached = true;
                view
            }))
    }

    pub async fn attach(&self, node_id: u8) -> Result<RollerCanControlState> {
        if let Some(current) = self.attached_node() {
            if current == node_id {
                return self.snapshot(node_id);
            }
            bail!(
                "RollerCAN 0x{current:02X} is already attached; detach it before selecting 0x{node_id:02X}"
            );
        }
        let _transaction = self.transaction_gate.lock().await;
        send_protocol_frame(&self.bus, 0x00, 0, HOST_ID, node_id, [0; 8], "probe").await?;
        let enabled = read_parameter_locked(&self.bus, &self.state, node_id, OD_ENABLE)
            .await
            .context("read enable state while attaching")?;
        let mode = read_parameter_locked(&self.bus, &self.state, node_id, OD_RUN_MODE)
            .await
            .context("read run mode while attaching")?;
        let mode = RollerCanControlMode::from_raw(mode)
            .ok_or_else(|| anyhow!("RollerCAN 0x{node_id:02X} reported unsupported mode {mode}"))?;

        {
            let now = Instant::now();
            let mut state = self.state.lock().unwrap();
            state.attached = Some(node_id);
            let node = state
                .nodes
                .entry(node_id)
                .or_insert_with(|| NodeState::new(node_id, now));
            node.view.attached = true;
            node.view.enabled = enabled != 0;
            node.view.mode = mode;
            node.view.last_error = None;
        }
        drop(_transaction);
        self.refresh(node_id).await?;
        self.snapshot(node_id)
    }

    pub async fn detach(&self, node_id: u8) -> Result<()> {
        self.disable(node_id).await?;
        let mut state = self.state.lock().unwrap();
        if state.attached == Some(node_id) {
            state.attached = None;
        }
        if let Some(node) = state.nodes.get_mut(&node_id) {
            node.view.attached = false;
        }
        Ok(())
    }

    pub async fn set_mode(
        &self,
        node_id: u8,
        mode: RollerCanControlMode,
    ) -> Result<RollerCanControlState> {
        self.require_attached(node_id)?;
        if self.snapshot(node_id)?.enabled {
            bail!("disable RollerCAN 0x{node_id:02X} before changing mode");
        }
        let _transaction = self.transaction_gate.lock().await;
        write_parameter_verified_locked(
            &self.bus,
            &self.state,
            node_id,
            OD_RUN_MODE,
            mode.raw(),
            "set run mode",
        )
        .await?;
        self.update_view(node_id, |view| {
            view.mode = mode;
            view.last_error = None;
        });
        self.snapshot(node_id)
    }

    pub async fn enable(&self, node_id: u8) -> Result<()> {
        self.require_attached(node_id)?;
        let snapshot = self.snapshot(node_id)?;
        if snapshot.fault_bits != 0 || snapshot.state_code == 2 {
            bail!(
                "RollerCAN 0x{node_id:02X} has an active fault; release protection before enabling"
            );
        }
        let _transaction = self.transaction_gate.lock().await;
        write_parameter_verified_locked(&self.bus, &self.state, node_id, OD_ENABLE, 1, "enable")
            .await?;
        self.update_view(node_id, |view| {
            view.enabled = true;
            view.last_error = None;
        });
        Ok(())
    }

    pub async fn disable(&self, node_id: u8) -> Result<()> {
        self.require_attached(node_id)?;
        let _transaction = self.transaction_gate.lock().await;
        let result = write_parameter_verified_locked(
            &self.bus,
            &self.state,
            node_id,
            OD_ENABLE,
            0,
            "disable",
        )
        .await;
        match result {
            Ok(()) => {
                self.update_view(node_id, |view| {
                    view.enabled = false;
                    view.state_code = 0;
                    view.last_error = None;
                });
                Ok(())
            }
            Err(error) => {
                self.set_error(node_id, format!("disable failed: {error:#}"));
                Err(error)
            }
        }
    }

    pub async fn release_stall(&self, node_id: u8) -> Result<()> {
        self.require_attached(node_id)?;
        let _transaction = self.transaction_gate.lock().await;
        send_protocol_frame(
            &self.bus,
            0x09,
            0,
            HOST_ID,
            node_id,
            [0; 8],
            "release stall protection",
        )
        .await?;
        self.update_view(node_id, |view| {
            view.fault_bits &= !0b010;
            view.last_error = None;
        });
        Ok(())
    }

    pub async fn send_target(&self, node_id: u8, target: RollerCanControlTarget) -> Result<()> {
        self.require_attached(node_id)?;
        let view = self.snapshot(node_id)?;
        if !view.enabled {
            bail!("enable RollerCAN 0x{node_id:02X} before sending a target");
        }
        if view.mode != target.mode() {
            bail!(
                "RollerCAN target mode mismatch: motor is {:?}, target is {:?}",
                view.mode,
                target.mode()
            );
        }
        let (index, value) = target.parameter()?;
        let _transaction = self.transaction_gate.lock().await;
        write_parameter_verified_locked(
            &self.bus,
            &self.state,
            node_id,
            index,
            value,
            "send target",
        )
        .await?;
        self.update_view(node_id, |view| view.last_error = None);
        Ok(())
    }

    pub async fn set_current_limit(&self, node_id: u8, current_ma: f64) -> Result<()> {
        self.require_attached(node_id)?;
        if !current_ma.is_finite() || !(0.0..=1200.0).contains(&current_ma) {
            bail!("RollerCAN current limit must be within 0..=1200 mA");
        }
        let mode = self.snapshot(node_id)?.mode;
        let index = match mode {
            RollerCanControlMode::Speed => OD_SPEED_MAX_CURRENT,
            RollerCanControlMode::Position => OD_POSITION_MAX_CURRENT,
            _ => bail!(
                "current limiting is exposed by the protocol only in Speed and Position modes"
            ),
        };
        let value = scaled_i32(current_ma, 100.0, "current limit")?;
        let _transaction = self.transaction_gate.lock().await;
        write_parameter_verified_locked(
            &self.bus,
            &self.state,
            node_id,
            index,
            value,
            "set current limit",
        )
        .await?;
        self.update_view(node_id, |view| match mode {
            RollerCanControlMode::Speed => view.speed_max_current_ma = Some(current_ma),
            RollerCanControlMode::Position => view.position_max_current_ma = Some(current_ma),
            _ => {}
        });
        Ok(())
    }

    pub async fn refresh(&self, node_id: u8) -> Result<()> {
        self.require_attached(node_id)?;
        for index in POLL_INDICES {
            let _transaction = self.transaction_gate.lock().await;
            if let Err(error) = read_parameter_locked(&self.bus, &self.state, node_id, index).await
            {
                self.set_error(node_id, format!("refresh 0x{index:04X} failed: {error:#}"));
                return Err(error);
            }
        }
        self.update_view(node_id, |view| view.last_error = None);
        Ok(())
    }

    /// Stop this controller.  With `force=false`, a failed confirmed disable
    /// keeps the monitor alive so the caller can retry.  Forced application
    /// shutdown still tears down tasks after reporting the safety failure.
    pub async fn stop(&self, force: bool) -> Result<()> {
        let disable_result = if let Some(node_id) = self.attached_node() {
            self.disable(node_id).await
        } else {
            Ok(())
        };
        if disable_result.is_err() && !force {
            return disable_result;
        }

        self.running.store(false, Ordering::SeqCst);
        let tasks = [
            self.discovery_task.lock().unwrap().take(),
            self.poll_task.lock().unwrap().take(),
            self.rx_task.lock().unwrap().take(),
        ];
        for task in tasks.into_iter().flatten() {
            task.abort();
            let _ = task.await;
        }
        disable_result
    }

    fn require_attached(&self, node_id: u8) -> Result<()> {
        match self.attached_node() {
            Some(current) if current == node_id => Ok(()),
            Some(current) => bail!(
                "RollerCAN 0x{node_id:02X} is not attached (0x{current:02X} currently owns the control window)"
            ),
            None => bail!("RollerCAN 0x{node_id:02X} is not attached"),
        }
    }

    fn update_view(&self, node_id: u8, update: impl FnOnce(&mut RollerCanControlState)) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        let node = state
            .nodes
            .entry(node_id)
            .or_insert_with(|| NodeState::new(node_id, now));
        update(&mut node.view);
    }

    fn set_error(&self, node_id: u8, error: String) {
        self.update_view(node_id, |view| view.last_error = Some(error));
    }
}

async fn discovery_loop(
    bus: Arc<dyn CanBus>,
    state: Arc<StdMutex<SharedState>>,
    scan_requested: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::SeqCst) {
        if scan_requested.swap(false, Ordering::SeqCst) {
            for node_id in u8::MIN..=u8::MAX {
                if !running.load(Ordering::SeqCst) {
                    return;
                }
                if let Err(error) =
                    send_protocol_frame(&bus, 0x00, 0, HOST_ID, node_id, [0; 8], "discovery ping")
                        .await
                {
                    log::warn!("RollerCAN control discovery ping failed: {error}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(12)).await;
            }
        }

        let known: Vec<u8> = state.lock().unwrap().nodes.keys().copied().collect();
        for node_id in known {
            if !running.load(Ordering::SeqCst) {
                return;
            }
            if let Err(error) =
                send_protocol_frame(&bus, 0x00, 0, HOST_ID, node_id, [0; 8], "presence ping").await
            {
                log::warn!("RollerCAN 0x{node_id:02X} presence ping failed: {error}");
            }
        }
        tokio::time::sleep(KNOWN_PING_PERIOD).await;
    }
}

async fn poll_loop(
    bus: Arc<dyn CanBus>,
    state: Arc<StdMutex<SharedState>>,
    transaction_gate: Arc<Mutex<()>>,
    running: Arc<AtomicBool>,
) {
    let mut cursor = 0usize;
    while running.load(Ordering::SeqCst) {
        tokio::time::sleep(POLL_STEP).await;
        let attached = state.lock().unwrap().attached;
        let Some(node_id) = attached else {
            continue;
        };
        let index = POLL_INDICES[cursor % POLL_INDICES.len()];
        cursor = cursor.wrapping_add(1);
        let _transaction = transaction_gate.lock().await;
        if let Err(error) = read_parameter_locked(&bus, &state, node_id, index).await {
            log::debug!("RollerCAN 0x{node_id:02X} poll 0x{index:04X}: {error}");
        }
    }
}

async fn rx_loop(
    mut rx: Box<dyn CanRx>,
    state: Arc<StdMutex<SharedState>>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::SeqCst) {
        match rx.recv().await {
            Ok(frame) => {
                if frame.kind() != FrameKind::Data || !frame.id().is_extended() {
                    continue;
                }
                let raw_id = frame.id().raw();
                let cmd = ((raw_id >> 24) & 0x1F) as u8;
                let now = Instant::now();
                match cmd {
                    0x00 if (raw_id & 0xFF) == 0xFE => {
                        let node_id = ((raw_id >> 8) & 0xFF) as u8;
                        observe_presence(&state, node_id, now);
                    }
                    0x02 if frame.data().len() >= 8 => {
                        ingest_status(&state, raw_id, frame.data(), now);
                    }
                    0x11 if frame.data().len() >= 8 => {
                        let node_id = (raw_id & 0xFF) as u8;
                        let host_id = ((raw_id >> 8) & 0xFF) as u8;
                        if host_id == HOST_ID {
                            ingest_parameter(&state, node_id, frame.data(), now);
                        }
                    }
                    _ => {}
                }
            }
            Err(CanIoError::Lagged { dropped }) => {
                log::warn!("RollerCAN control receive lagged; dropped {dropped} frames");
            }
            Err(CanIoError::Disconnected) => break,
            Err(error) => log::warn!("RollerCAN control receive failed: {error}"),
        }
    }
}

fn observe_presence(state: &Arc<StdMutex<SharedState>>, node_id: u8, now: Instant) {
    let mut state = state.lock().unwrap();
    state
        .nodes
        .entry(node_id)
        .or_insert_with(|| NodeState::new(node_id, now))
        .observe(now, false);
}

fn ingest_status(state: &Arc<StdMutex<SharedState>>, raw_id: u32, data: &[u8], now: Instant) {
    let node_id = ((raw_id >> 8) & 0xFF) as u8;
    let mode_raw = ((raw_id >> 19) & 0x07) as i32;
    let state_code = ((raw_id >> 22) & 0x03) as u8;
    let fault_bits = ((raw_id >> 16) & 0x07) as u8;
    let mut state = state.lock().unwrap();
    let node = state
        .nodes
        .entry(node_id)
        .or_insert_with(|| NodeState::new(node_id, now));
    node.observe(now, true);
    node.view.state_code = state_code;
    node.view.fault_bits = fault_bits;
    node.view.enabled = state_code == 1;
    if let Some(mode) = RollerCanControlMode::from_raw(mode_raw) {
        node.view.mode = mode;
    }
    node.view.speed_rpm = Some(i16::from_le_bytes([data[0], data[1]]) as f64);
    node.view.position_deg = Some(i16::from_le_bytes([data[2], data[3]]) as f64);
    node.view.current_ma = Some(i16::from_le_bytes([data[4], data[5]]) as f64);
    node.view.voltage_v = Some(i16::from_le_bytes([data[6], data[7]]) as f64);
    node.view.last_error = status_error(fault_bits, state_code);
}

fn ingest_parameter(state: &Arc<StdMutex<SharedState>>, node_id: u8, data: &[u8], now: Instant) {
    let index = u16::from_le_bytes([data[0], data[1]]);
    let value = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let mut state = state.lock().unwrap();
    let node = state
        .nodes
        .entry(node_id)
        .or_insert_with(|| NodeState::new(node_id, now));
    node.observe(now, true);
    apply_parameter(&mut node.view, index, value);
    if let Some(waiters) = state.pending_reads.remove(&(node_id, index)) {
        for waiter in waiters {
            let _ = waiter.send(value);
        }
    }
}

fn apply_parameter(view: &mut RollerCanControlState, index: u16, value: i32) {
    match index {
        OD_ENABLE => view.enabled = value != 0,
        OD_RUN_MODE => {
            if let Some(mode) = RollerCanControlMode::from_raw(value) {
                view.mode = mode;
            }
        }
        OD_SPEED_READBACK => view.speed_rpm = Some(value as f64 / 100.0),
        OD_POSITION_READBACK => view.position_deg = Some(value as f64 / 100.0),
        OD_CURRENT_READBACK => view.current_ma = Some(value as f64 / 100.0),
        OD_ENCODER => view.encoder_count = Some(value),
        OD_VIN => view.voltage_v = Some(value as f64 / 100.0),
        OD_TEMPERATURE => view.temperature_c = Some(value as f64),
        OD_POSITION_MAX_CURRENT => view.position_max_current_ma = Some(value as f64 / 100.0),
        OD_SPEED_MAX_CURRENT => view.speed_max_current_ma = Some(value as f64 / 100.0),
        _ => {}
    }
}

async fn read_parameter_locked(
    bus: &Arc<dyn CanBus>,
    state: &Arc<StdMutex<SharedState>>,
    node_id: u8,
    index: u16,
) -> Result<i32> {
    let (sender, receiver) = oneshot::channel();
    {
        let mut state = state.lock().unwrap();
        let waiters = state.pending_reads.entry((node_id, index)).or_default();
        waiters.retain(|waiter| !waiter.is_closed());
        waiters.push(sender);
    }
    let mut data = [0u8; 8];
    data[0..2].copy_from_slice(&index.to_le_bytes());
    if let Err(error) =
        send_protocol_frame(bus, 0x11, 0, HOST_ID, node_id, data, "read parameter").await
    {
        remove_closed_waiters(state, node_id, index);
        return Err(error);
    }
    match tokio::time::timeout(READ_TIMEOUT, receiver).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(anyhow!(
            "RollerCAN 0x{node_id:02X} parameter 0x{index:04X} response was cancelled"
        )),
        Err(_) => {
            remove_closed_waiters(state, node_id, index);
            Err(anyhow!(
                "RollerCAN 0x{node_id:02X} parameter 0x{index:04X} timed out after {} ms",
                READ_TIMEOUT.as_millis()
            ))
        }
    }
}

async fn write_parameter_verified_locked(
    bus: &Arc<dyn CanBus>,
    state: &Arc<StdMutex<SharedState>>,
    node_id: u8,
    index: u16,
    value: i32,
    note: &'static str,
) -> Result<()> {
    let mut data = [0u8; 8];
    data[0..2].copy_from_slice(&index.to_le_bytes());
    data[4..8].copy_from_slice(&value.to_le_bytes());
    send_protocol_frame(bus, 0x12, 0, HOST_ID, node_id, data, note).await?;
    let actual = read_parameter_locked(bus, state, node_id, index).await?;
    if actual != value {
        bail!(
            "RollerCAN 0x{node_id:02X} rejected {note}: 0x{index:04X} expected {value}, read back {actual}"
        );
    }
    Ok(())
}

async fn send_protocol_frame(
    bus: &Arc<dyn CanBus>,
    cmd: u8,
    param: u8,
    host_id: u8,
    target_id: u8,
    data: [u8; 8],
    note: &'static str,
) -> Result<()> {
    let frame = build_protocol_frame(cmd, param, host_id, target_id, data)?;
    bus.send(frame)
        .await
        .map_err(|error| anyhow!("{note}: send RollerCAN frame: {error}"))
}

fn build_protocol_frame(
    cmd: u8,
    param: u8,
    host_id: u8,
    target_id: u8,
    data: [u8; 8],
) -> Result<CanFrame> {
    if cmd > 0x1F {
        bail!("RollerCAN command 0x{cmd:02X} exceeds 5 bits");
    }
    let raw_id =
        ((cmd as u32) << 24) | ((param as u32) << 16) | ((host_id as u32) << 8) | target_id as u32;
    let id = CanId::new_extended(raw_id).map_err(|error| anyhow!("bad RollerCAN id: {error}"))?;
    CanFrame::new_data(id, &data).map_err(|error| anyhow!("build RollerCAN frame: {error}"))
}

fn remove_closed_waiters(state: &Arc<StdMutex<SharedState>>, node_id: u8, index: u16) {
    let mut state = state.lock().unwrap();
    if let Some(waiters) = state.pending_reads.get_mut(&(node_id, index)) {
        waiters.retain(|waiter| !waiter.is_closed());
        if waiters.is_empty() {
            state.pending_reads.remove(&(node_id, index));
        }
    }
}

fn feedback_rate(times: &VecDeque<Instant>) -> Option<f64> {
    let first = *times.front()?;
    let last = *times.back()?;
    if times.len() < 2 {
        return None;
    }
    let elapsed = last.saturating_duration_since(first).as_secs_f64();
    (elapsed > 0.0).then_some((times.len() - 1) as f64 / elapsed)
}

fn status_error(fault_bits: u8, state_code: u8) -> Option<String> {
    let mut faults = Vec::new();
    if fault_bits & 0b001 != 0 {
        faults.push("over-voltage");
    }
    if fault_bits & 0b010 != 0 {
        faults.push("stall");
    }
    if fault_bits & 0b100 != 0 {
        faults.push("out-of-range");
    }
    if faults.is_empty() && state_code != 2 {
        None
    } else if faults.is_empty() {
        Some("motor reported Error state".into())
    } else {
        Some(format!("motor fault: {}", faults.join(", ")))
    }
}

fn scaled_i32(value: f64, scale: f64, label: &str) -> Result<i32> {
    if !value.is_finite() {
        bail!("RollerCAN {label} must be finite");
    }
    let scaled = (value * scale).round();
    if scaled < i32::MIN as f64 || scaled > i32::MAX as f64 {
        bail!("RollerCAN {label} is outside the protocol's int32 range");
    }
    Ok(scaled as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_control_frames_remain_extended_classic_can() {
        let frame = build_protocol_frame(0x12, 0, HOST_ID, DEFAULT_NODE_ID, [0x5A; 8])
            .expect("valid stock RollerCAN frame");

        assert_eq!(frame.kind(), FrameKind::Data);
        assert!(frame.id().is_extended());
        assert_eq!(frame.data(), &[0x5A; 8]);
    }

    #[test]
    fn encodes_documented_target_scaling() {
        assert_eq!(
            RollerCanControlTarget::Speed { speed_rpm: 3000.0 }
                .parameter()
                .unwrap(),
            (OD_SPEED, 300_000)
        );
        assert_eq!(
            RollerCanControlTarget::Position {
                position_deg: -20_000.0
            }
            .parameter()
            .unwrap(),
            (OD_POSITION, -2_000_000)
        );
        assert_eq!(
            RollerCanControlTarget::Current { current_ma: 450.0 }
                .parameter()
                .unwrap(),
            (OD_CURRENT, 45_000)
        );
    }

    #[test]
    fn rejects_current_beyond_firmware_limit() {
        assert!(RollerCanControlTarget::Current { current_ma: 1200.1 }
            .parameter()
            .is_err());
    }

    #[test]
    fn decodes_parameter_units() {
        let mut view = RollerCanControlState::new(DEFAULT_NODE_ID);
        apply_parameter(&mut view, OD_SPEED_READBACK, -123_456);
        apply_parameter(&mut view, OD_POSITION_READBACK, 9_001);
        apply_parameter(&mut view, OD_CURRENT_READBACK, 45_000);
        apply_parameter(&mut view, OD_VIN, 1_602);
        assert_eq!(view.speed_rpm, Some(-1234.56));
        assert_eq!(view.position_deg, Some(90.01));
        assert_eq!(view.current_ma, Some(450.0));
        assert_eq!(view.voltage_v, Some(16.02));
    }
}
