//! Firmware-owned low-duty lift commissioning over the frozen ABI v2.
//!
//! The WebView never owns CAN timing. This module sends the 20 ms RPDO3
//! stream, including zero keepalives while armed-idle, and independently
//! expires the short operator lease. Firmware remains authoritative for all
//! sensor gates, duty/current limits, lease expiry, and absolute pulse time.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use can_transport::{CanBus, CanFilter, CanFrame, CanRx, FrameKind};
use hex_motor::canopen::sdo;
use serde::Serialize;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::lift::LiftState;

const OD: u16 = 0x4700;
const HIGHEST_SUBINDEX: u8 = 27;
const ABI_SUB: u8 = 0x01;
const ACTIVE_SESSION_SUB: u8 = 0x02;
const STATE_SUB: u8 = 0x03;
const FLAGS_SUB: u8 = 0x04;
const BOOT_EPOCH_SUB: u8 = 0x14;
const CHALLENGE_SUB: u8 = 0x15;
const CHALLENGE_KIND_SUB: u8 = 0x16;
const EXPECTED_PULSE_SUB: u8 = 0x17;
const EPOCH_SERVICE_SUB: u8 = 0x19;
const EPOCH_STATUS_SUB: u8 = 0x1b;

const DEVICE_NAME: &str = "hexmeow-lift-commission";
const ABI_VERSION: u16 = 2;
const CHALLENGE_KIND_NONE: u8 = 0;
const CHALLENGE_KIND_ARM: u8 = 1;
const CHALLENGE_KIND_CLEAR_FAULT: u8 = 2;
const EPOCH_STATUS_READY: u8 = 0;
const EPOCH_STATUS_MISSING_OR_UNREADABLE: u8 = 1;
const EPOCH_STATUS_CORRUPT: u8 = 2;
const EPOCH_STATUS_EXHAUSTED: u8 = 3;
const EPOCH_STATUS_WRITE_FAILED: u8 = 4;
pub const STATE_DISARMED: u8 = 0;
pub const STATE_ARMED_IDLE: u8 = 1;
pub const STATE_FAULT_LATCHED: u8 = 0x80;

pub const FLAG_ARMED: u8 = 1 << 0;
pub const FLAG_OUTPUT_ACTIVE: u8 = 1 << 2;
pub const FLAG_FAULT: u8 = 1 << 7;

const NMT_OPERATIONAL: u8 = 0x05;
const NMT_PRE_OPERATIONAL: u8 = 0x7f;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(1_200);
const TPDO_TIMEOUT: Duration = Duration::from_millis(250);
const COMMAND_PERIOD: Duration = Duration::from_millis(20);
const CONFIRM_TIMEOUT: Duration = Duration::from_millis(500);
const SEQUENCE_SYNC_TIMEOUT: Duration = Duration::from_millis(150);
const TELEMETRY_CAPACITY: usize = 2_000;

#[derive(Debug, Clone, Serialize)]
pub struct CommissionView {
    pub available: bool,
    pub highest_subindex: u8,
    pub abi: u16,
    pub active_session: u32,
    pub state: u8,
    pub flags: u8,
    pub requested_duty_permille: i16,
    pub applied_duty_permille: i16,
    pub hard_cap_permille: u16,
    pub lease_ms: u16,
    pub max_pulse_ms: u16,
    pub pulse_elapsed_ms: u16,
    pub command_age_ms: u16,
    pub stop_reason: u16,
    pub soft_current_a: f32,
    pub active_pulse: u16,
    pub energized_ms: u16,
    pub foldback_cap_permille: u16,
    pub overcurrent_ms: u16,
    pub gap_remaining_ms: u16,
    pub hard_current_a: f32,
    pub boot_epoch: u32,
    pub challenge: u32,
    pub challenge_kind: u8,
    pub expected_pulse_id: u16,
    pub encoder_sign: i8,
    pub ina_fingerprint_mismatch: u16,
    pub epoch_status: u8,

    pub tpdo3_fresh: bool,
    pub tpdo4_fresh: bool,
    pub pair_fresh: bool,
    pub tick: u16,
    pub raw_count: i32,
    pub current_a: f32,
    pub host_remaining_ms: u16,
    pub buffered_samples: usize,
    pub dropped_pairs: u64,
}

impl Default for CommissionView {
    fn default() -> Self {
        Self {
            available: false,
            highest_subindex: 0,
            abi: 0,
            active_session: 0,
            state: STATE_DISARMED,
            flags: 0,
            requested_duty_permille: 0,
            applied_duty_permille: 0,
            hard_cap_permille: 0,
            lease_ms: 0,
            max_pulse_ms: 0,
            pulse_elapsed_ms: 0,
            command_age_ms: 0,
            stop_reason: 0,
            soft_current_a: 0.0,
            active_pulse: 0,
            energized_ms: 0,
            foldback_cap_permille: 0,
            overcurrent_ms: 0,
            gap_remaining_ms: 0,
            hard_current_a: 0.0,
            boot_epoch: 0,
            challenge: 0,
            challenge_kind: CHALLENGE_KIND_NONE,
            expected_pulse_id: 0,
            encoder_sign: 0,
            ina_fingerprint_mismatch: 0,
            epoch_status: EPOCH_STATUS_MISSING_OR_UNREADABLE,
            tpdo3_fresh: false,
            tpdo4_fresh: false,
            pair_fresh: false,
            tick: 0,
            raw_count: 0,
            current_a: 0.0,
            host_remaining_ms: 0,
            buffered_samples: 0,
            dropped_pairs: 0,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Demand {
    generation: u64,
    session: u32,
    pulse_id: u16,
    duty_permille: i16,
    armed: bool,
    holding: bool,
    lease_deadline: Option<Instant>,
    hold_started: Option<Instant>,
    max_pulse_ms: u16,
    lease_ms: u16,
    sequence_sync_from: Option<u16>,
    sequence_sync_deadline: Option<Instant>,
    wait_for_idle: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DemandEvent {
    None,
    OperatorLeaseExpired,
    HostPulseExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DemandFrame {
    session: u32,
    pulse_id: u16,
    duty_permille: i16,
    event: DemandEvent,
}

fn demand_frame(demand: &mut Demand, now: Instant) -> Option<DemandFrame> {
    if !demand.armed {
        return None;
    }

    let mut event = DemandEvent::None;
    if demand.holding {
        if demand.lease_deadline.is_none_or(|deadline| now >= deadline) {
            event = DemandEvent::OperatorLeaseExpired;
            demand.holding = false;
            demand.duty_permille = 0;
            demand.lease_deadline = None;
            demand.hold_started = None;
            mark_sequence_sync(demand, now);
        } else if demand.hold_started.is_some_and(|started| {
            now.saturating_duration_since(started)
                >= Duration::from_millis(u64::from(demand.max_pulse_ms))
        }) {
            event = DemandEvent::HostPulseExpired;
            demand.holding = false;
            demand.duty_permille = 0;
            demand.lease_deadline = None;
            demand.hold_started = None;
            mark_sequence_sync(demand, now);
        }
    }

    Some(DemandFrame {
        session: demand.session,
        pulse_id: demand.pulse_id,
        duty_permille: if demand.holding {
            demand.duty_permille
        } else {
            0
        },
        event,
    })
}

fn nonzero_frame_is_current(demand: &Demand, frame: DemandFrame, now: Instant) -> bool {
    frame.duty_permille != 0
        && demand.armed
        && demand.holding
        && demand.session == frame.session
        && demand.pulse_id == frame.pulse_id
        && demand.duty_permille == frame.duty_permille
        && demand.lease_deadline.is_some_and(|deadline| now < deadline)
        && demand.hold_started.is_some_and(|started| {
            now.saturating_duration_since(started)
                < Duration::from_millis(u64::from(demand.max_pulse_ms))
        })
}

fn release_demand(demand: &mut Demand, now: Instant) -> Option<DemandFrame> {
    if !demand.armed {
        return None;
    }
    let was_holding = demand.holding;
    demand.holding = false;
    demand.duty_permille = 0;
    demand.lease_deadline = None;
    demand.hold_started = None;
    if was_holding {
        mark_sequence_sync(demand, now);
    }
    Some(DemandFrame {
        session: demand.session,
        pulse_id: demand.pulse_id,
        duty_permille: 0,
        event: DemandEvent::None,
    })
}

fn mark_sequence_sync(demand: &mut Demand, now: Instant) {
    if demand.sequence_sync_from.is_none() {
        demand.sequence_sync_from = Some(demand.pulse_id);
        demand.sequence_sync_deadline = Some(now + SEQUENCE_SYNC_TIMEOUT);
    }
    demand.wait_for_idle = true;
}

fn sequence_sync_expired(demand: &Demand, now: Instant) -> bool {
    demand
        .sequence_sync_deadline
        .is_some_and(|deadline| now >= deadline)
}

fn apply_ready_sequence(
    demand: &mut Demand,
    sync_from: Option<u16>,
    observed_pulse: u16,
    idle: bool,
) {
    if let Some(from) = sync_from {
        demand.pulse_id = observed_pulse;
        if idle || observed_pulse != from {
            demand.sequence_sync_from = None;
            demand.sequence_sync_deadline = None;
            demand.wait_for_idle = !idle;
        } else {
            // 0xFFFF may still be the active final pulse rather than the
            // no-active sentinel. Retain the hard sync deadline until the
            // firmware explicitly ends the session.
            demand.wait_for_idle = true;
        }
    } else if demand.wait_for_idle && demand.pulse_id == observed_pulse && idle {
        demand.wait_for_idle = false;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Tpdo3 {
    tick: u16,
    raw_count: i32,
    applied_duty_permille: i16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Tpdo4 {
    tick: u16,
    current_a: f32,
    requested_duty_permille: i16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Joined {
    tick: u16,
    raw_count: i32,
    applied_duty_permille: i16,
    current_a: f32,
    requested_duty_permille: i16,
}

#[derive(Default)]
struct PairJoiner {
    tpdo3: Option<Tpdo3>,
    tpdo4: Option<Tpdo4>,
    dropped: u64,
}

impl PairJoiner {
    fn push_tpdo3(&mut self, sample: Tpdo3) -> Option<Joined> {
        if self.tpdo3.replace(sample).is_some() {
            self.dropped = self.dropped.saturating_add(1);
        }
        self.try_join()
    }

    fn push_tpdo4(&mut self, sample: Tpdo4) -> Option<Joined> {
        if self.tpdo4.replace(sample).is_some() {
            self.dropped = self.dropped.saturating_add(1);
        }
        self.try_join()
    }

    fn try_join(&mut self) -> Option<Joined> {
        let (tpdo3, tpdo4) = (self.tpdo3?, self.tpdo4?);
        if tpdo3.tick == tpdo4.tick {
            self.tpdo3 = None;
            self.tpdo4 = None;
            return Some(Joined {
                tick: tpdo3.tick,
                raw_count: tpdo3.raw_count,
                applied_duty_permille: tpdo3.applied_duty_permille,
                current_a: tpdo4.current_a,
                requested_duty_permille: tpdo4.requested_duty_permille,
            });
        }

        // A wrapping distance below half the u16 range means lhs is newer.
        if tpdo3.tick.wrapping_sub(tpdo4.tick) < 0x8000 {
            self.tpdo4 = None;
        } else {
            self.tpdo3 = None;
        }
        self.dropped = self.dropped.saturating_add(1);
        None
    }
}

#[derive(Clone)]
struct CsvSample {
    host_unix_ms: u64,
    node_id: u8,
    active_session: u32,
    pulse_id: u16,
    tick: u16,
    raw_count: i32,
    current_a: f32,
    requested_duty_permille: i16,
    applied_duty_permille: i16,
    bus_voltage_v: f32,
    state: u8,
    flags: u8,
    stop_reason: u16,
}

#[derive(Default)]
struct Telemetry {
    joiner: PairJoiner,
    samples: VecDeque<CsvSample>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SequenceSnapshot {
    active_session: u32,
    state: u8,
    active_pulse: u16,
    gap_remaining_ms: u16,
    expected_pulse_id: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EpochServiceSnapshot {
    active_session: u32,
    state: u8,
    flags: u8,
    boot_epoch: u32,
    epoch_status: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceFeedback {
    Pending,
    Active(u16),
    Ready { pulse_id: u16, idle: bool },
    SessionEnded,
    Unsafe,
}

#[derive(Default)]
struct Freshness {
    tpdo3: Option<Instant>,
    tpdo4: Option<Instant>,
    pair: Option<Instant>,
}

/// One firmware-owned commissioning stream associated with a LiftSession.
pub struct Commissioning {
    node_id: u8,
    bus: Arc<dyn CanBus>,
    sdo_timeout: Option<Duration>,
    sdo_gate: Arc<AsyncMutex<()>>,
    rpdo_gate: Arc<AsyncMutex<()>>,
    base_state: Arc<StdMutex<LiftState>>,
    view: Arc<StdMutex<CommissionView>>,
    demand: Arc<StdMutex<Demand>>,
    telemetry: Arc<StdMutex<Telemetry>>,
    freshness: Arc<StdMutex<Freshness>>,
    running: Arc<AtomicBool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl Commissioning {
    pub async fn start(
        bus: Arc<dyn CanBus>,
        node_id: u8,
        sdo_timeout: Option<Duration>,
        sdo_gate: Arc<AsyncMutex<()>>,
        base_state: Arc<StdMutex<LiftState>>,
    ) -> anyhow::Result<Self> {
        let tpdo3_rx = bus
            .subscribe(CanFilter::exact_standard(tpdo3_cob_id(node_id)))
            .await
            .map_err(|e| anyhow::anyhow!("subscribe commissioning TPDO3: {e}"))?;
        let tpdo4_rx = bus
            .subscribe(CanFilter::exact_standard(tpdo4_cob_id(node_id)))
            .await
            .map_err(|e| anyhow::anyhow!("subscribe commissioning TPDO4: {e}"))?;

        let view = Arc::new(StdMutex::new(CommissionView::default()));
        let demand = Arc::new(StdMutex::new(Demand::default()));
        let telemetry = Arc::new(StdMutex::new(Telemetry::default()));
        let freshness = Arc::new(StdMutex::new(Freshness::default()));
        let running = Arc::new(AtomicBool::new(true));
        let rpdo_gate = Arc::new(AsyncMutex::new(()));

        let session = Self {
            node_id,
            bus,
            sdo_timeout,
            sdo_gate,
            rpdo_gate,
            base_state,
            view,
            demand,
            telemetry,
            freshness,
            running,
            tasks: StdMutex::new(Vec::new()),
        };

        *session.tasks.lock().unwrap() = vec![
            tokio::spawn(tpdo3_loop(
                tpdo3_rx,
                node_id,
                session.view.clone(),
                session.demand.clone(),
                session.telemetry.clone(),
                session.freshness.clone(),
                session.base_state.clone(),
                session.running.clone(),
            )),
            tokio::spawn(tpdo4_loop(
                tpdo4_rx,
                node_id,
                session.view.clone(),
                session.demand.clone(),
                session.telemetry.clone(),
                session.freshness.clone(),
                session.base_state.clone(),
                session.running.clone(),
            )),
            tokio::spawn(command_loop(
                session.bus.clone(),
                node_id,
                session.view.clone(),
                session.demand.clone(),
                session.rpdo_gate.clone(),
                session.freshness.clone(),
                session.base_state.clone(),
                session.running.clone(),
            )),
            tokio::spawn(sequence_sync_loop(
                session.bus.clone(),
                node_id,
                session.sdo_timeout,
                session.sdo_gate.clone(),
                session.view.clone(),
                session.demand.clone(),
                session.base_state.clone(),
                session.running.clone(),
            )),
        ];

        Ok(session)
    }

    pub fn view(&self) -> CommissionView {
        sync_freshness(&self.freshness, &self.view);
        let demand = *self.demand.lock().unwrap();
        let (buffered_samples, dropped_pairs) = {
            let telemetry = self.telemetry.lock().unwrap();
            (telemetry.samples.len(), telemetry.joiner.dropped)
        };
        let mut view = self.view.lock().unwrap();
        view.host_remaining_ms = demand
            .hold_started
            .map(|started| {
                let elapsed = Instant::now().saturating_duration_since(started);
                Duration::from_millis(u64::from(demand.max_pulse_ms))
                    .saturating_sub(elapsed)
                    .as_millis()
                    .min(u128::from(u16::MAX)) as u16
            })
            .unwrap_or(0);
        view.buffered_samples = buffered_samples;
        view.dropped_pairs = dropped_pairs;
        view.clone()
    }

    pub fn is_armed(&self) -> bool {
        self.demand.lock().unwrap().armed || self.view.lock().unwrap().flags & FLAG_ARMED != 0
    }

    /// Probe ABI only for the exact commissioning identity. Other lift
    /// firmware remains observation-compatible and never exposes controls.
    pub async fn probe_identity(&self, device_name: &str) -> anyhow::Result<()> {
        let mut next = CommissionView::default();
        if device_name == DEVICE_NAME {
            let probe = async {
                let highest =
                    sdo::upload_u8(&*self.bus, self.node_id, OD, 0, self.sdo_timeout).await?;
                let abi = sdo::upload_u16(&*self.bus, self.node_id, OD, ABI_SUB, self.sdo_timeout)
                    .await?;
                Ok::<_, anyhow::Error>((highest, abi))
            }
            .await;
            match probe {
                Ok((highest, abi)) => {
                    next.highest_subindex = highest;
                    next.abi = abi;
                }
                Err(error) => {
                    log::warn!(
                        "Lift 0x{:02X}: commissioning identity but ABI probe failed: {error}",
                        self.node_id
                    );
                }
            }
        }
        next.available = identity_matches(device_name, next.highest_subindex, next.abi);
        *self.view.lock().unwrap() = next;
        if self.view.lock().unwrap().available {
            self.refresh_locked().await?;
        }
        Ok(())
    }

    pub fn is_available(&self) -> bool {
        self.view.lock().unwrap().available
    }

    pub async fn refresh_locked(&self) -> anyhow::Result<()> {
        if !self.view.lock().unwrap().available {
            return Ok(());
        }
        let bus = &*self.bus;
        let nid = self.node_id;
        let timeout = self.sdo_timeout;
        let active_session = sdo::upload_u32(bus, nid, OD, ACTIVE_SESSION_SUB, timeout).await?;
        let state = sdo::upload_u8(bus, nid, OD, STATE_SUB, timeout).await?;
        let flags = sdo::upload_u8(bus, nid, OD, FLAGS_SUB, timeout).await?;
        let requested = read_i16(bus, nid, OD, 0x05, timeout).await?;
        let applied = read_i16(bus, nid, OD, 0x06, timeout).await?;
        let hard_cap = sdo::upload_u16(bus, nid, OD, 0x07, timeout).await?;
        let lease_ms = sdo::upload_u16(bus, nid, OD, 0x08, timeout).await?;
        let max_pulse_ms = sdo::upload_u16(bus, nid, OD, 0x09, timeout).await?;
        let pulse_elapsed_ms = sdo::upload_u16(bus, nid, OD, 0x0a, timeout).await?;
        let command_age_ms = sdo::upload_u16(bus, nid, OD, 0x0b, timeout).await?;
        let stop_reason = sdo::upload_u16(bus, nid, OD, 0x0c, timeout).await?;
        let soft_current_a = sdo::upload_f32(bus, nid, OD, 0x0d, timeout).await?;
        let active_pulse = sdo::upload_u16(bus, nid, OD, 0x0e, timeout).await?;
        let energized_ms = sdo::upload_u16(bus, nid, OD, 0x0f, timeout).await?;
        let foldback_cap = sdo::upload_u16(bus, nid, OD, 0x10, timeout).await?;
        let overcurrent_ms = sdo::upload_u16(bus, nid, OD, 0x11, timeout).await?;
        let gap_remaining_ms = sdo::upload_u16(bus, nid, OD, 0x12, timeout).await?;
        let hard_current_a = sdo::upload_f32(bus, nid, OD, 0x13, timeout).await?;
        let boot_epoch = sdo::upload_u32(bus, nid, OD, BOOT_EPOCH_SUB, timeout).await?;
        let challenge = sdo::upload_u32(bus, nid, OD, CHALLENGE_SUB, timeout).await?;
        let challenge_kind = sdo::upload_u8(bus, nid, OD, CHALLENGE_KIND_SUB, timeout).await?;
        let expected_pulse_id = sdo::upload_u16(bus, nid, OD, EXPECTED_PULSE_SUB, timeout).await?;
        let encoder_sign = sdo::upload_u8(bus, nid, OD, 0x18, timeout).await? as i8;
        let ina_fingerprint_mismatch = sdo::upload_u16(bus, nid, OD, 0x1a, timeout).await?;
        let epoch_status = sdo::upload_u8(bus, nid, OD, EPOCH_STATUS_SUB, timeout).await?;

        let mut view = self.view.lock().unwrap();
        view.active_session = active_session;
        view.state = state;
        view.flags = flags;
        view.requested_duty_permille = requested;
        view.applied_duty_permille = applied;
        view.hard_cap_permille = hard_cap;
        view.lease_ms = lease_ms;
        view.max_pulse_ms = max_pulse_ms;
        view.pulse_elapsed_ms = pulse_elapsed_ms;
        view.command_age_ms = command_age_ms;
        view.stop_reason = stop_reason;
        view.soft_current_a = soft_current_a;
        view.active_pulse = active_pulse;
        view.energized_ms = energized_ms;
        view.foldback_cap_permille = foldback_cap;
        view.overcurrent_ms = overcurrent_ms;
        view.gap_remaining_ms = gap_remaining_ms;
        view.hard_current_a = hard_current_a;
        view.boot_epoch = boot_epoch;
        view.challenge = challenge;
        view.challenge_kind = challenge_kind;
        view.expected_pulse_id = expected_pulse_id;
        view.encoder_sign = encoder_sign;
        view.ina_fingerprint_mismatch = ina_fingerprint_mismatch;
        view.epoch_status = epoch_status;
        Ok(())
    }

    /// Reserve a generation before any asynchronous ARM preflight. Emergency
    /// stop, disarm, NMT exit, and detach all invalidate this generation.
    pub fn begin_arm_request(&self) -> anyhow::Result<u64> {
        {
            let view = self.view.lock().unwrap();
            if view.active_session != 0
                || view.state != STATE_DISARMED
                || view.flags & FLAG_ARMED != 0
            {
                anyhow::bail!("commissioning is not Disarmed");
            }
            if view.epoch_status != EPOCH_STATUS_READY || view.boot_epoch == 0 {
                anyhow::bail!(
                    "commissioning anti-replay epoch is not ready (status={}, epoch={})",
                    view.epoch_status,
                    view.boot_epoch
                );
            }
        }
        let mut demand = self.demand.lock().unwrap();
        if demand.armed || demand.session != 0 {
            anyhow::bail!("commissioning session is already armed");
        }
        let generation = demand.generation.wrapping_add(1);
        *demand = Demand {
            generation,
            ..Default::default()
        };
        Ok(generation)
    }

    pub fn abort_arm_request(&self, generation: u64) {
        let mut demand = self.demand.lock().unwrap();
        if demand.generation == generation && !demand.armed {
            *demand = Demand {
                generation: generation.wrapping_add(1),
                ..Default::default()
            };
        }
    }

    pub async fn arm(&self, generation: u64) -> anyhow::Result<u32> {
        if let Err(error) = self.require_gate() {
            self.cancel_local();
            return Err(error);
        }
        let (lease_ms, max_pulse_ms) = {
            let view = self.view.lock().unwrap();
            if view.active_session != 0
                || view.state != STATE_DISARMED
                || view.flags & FLAG_ARMED != 0
            {
                anyhow::bail!("commissioning is not Disarmed");
            }
            (view.lease_ms, view.max_pulse_ms)
        };
        let challenge = match self
            .wait_for_challenge(CHALLENGE_KIND_ARM, generation)
            .await
        {
            Ok(challenge) => challenge,
            Err(error) => {
                self.cancel_local();
                return Err(error);
            }
        };

        {
            let mut telemetry = self.telemetry.lock().unwrap();
            telemetry.samples.clear();
            telemetry.joiner = PairJoiner::default();
        }
        {
            let mut demand = self.demand.lock().unwrap();
            if demand.generation != generation {
                anyhow::bail!("commission ARM was canceled during preflight");
            }
            *demand = Demand {
                generation,
                session: challenge,
                max_pulse_ms,
                lease_ms,
                ..Default::default()
            };
        }

        if let Err(error) = sdo::download_u32(
            &*self.bus,
            self.node_id,
            OD,
            ACTIVE_SESSION_SUB,
            challenge,
            self.sdo_timeout,
        )
        .await
        {
            self.cancel_local();
            let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
            return Err(anyhow::anyhow!(
                "commission ARM device-challenge echo unconfirmed; directed NMT Stop sent: {error}"
            ));
        }

        let expected_pulse = match self.wait_for_arm(challenge, generation).await {
            Ok(expected) => expected,
            Err(error) => {
                self.cancel_local();
                let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
                return Err(error);
            }
        };

        let canceled_after_confirmation = {
            let mut demand = self.demand.lock().unwrap();
            if demand.generation != generation {
                true
            } else {
                demand.armed = true;
                demand.pulse_id = expected_pulse;
                false
            }
        };
        if canceled_after_confirmation {
            let _ = sdo::download_u32(
                &*self.bus,
                self.node_id,
                OD,
                ACTIVE_SESSION_SUB,
                0,
                self.sdo_timeout,
            )
            .await;
            anyhow::bail!("commission ARM was canceled before confirmation");
        }

        if let Err(error) = send_rpdo3_serialized(
            &self.rpdo_gate,
            &self.bus,
            self.node_id,
            challenge,
            expected_pulse,
            0,
        )
        .await
        {
            self.cancel_local();
            let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
            anyhow::bail!("initial commissioning zero keepalive failed; NMT Stop sent: {error}");
        }
        Ok(challenge)
    }

    async fn wait_for_challenge(&self, expected_kind: u8, generation: u64) -> anyhow::Result<u32> {
        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        loop {
            if self.demand.lock().unwrap().generation != generation {
                anyhow::bail!("commission challenge wait canceled");
            }
            self.require_nmt(NMT_OPERATIONAL)?;
            let kind_before = sdo::upload_u8(
                &*self.bus,
                self.node_id,
                OD,
                CHALLENGE_KIND_SUB,
                self.sdo_timeout,
            )
            .await?;
            let challenge = sdo::upload_u32(
                &*self.bus,
                self.node_id,
                OD,
                CHALLENGE_SUB,
                self.sdo_timeout,
            )
            .await?;
            let kind_after = sdo::upload_u8(
                &*self.bus,
                self.node_id,
                OD,
                CHALLENGE_KIND_SUB,
                self.sdo_timeout,
            )
            .await?;
            {
                let mut view = self.view.lock().unwrap();
                view.challenge = challenge;
                view.challenge_kind = kind_after;
            }
            if let Some(challenge) =
                coherent_challenge(kind_before, challenge, kind_after, expected_kind)
            {
                return Ok(challenge);
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "device challenge timed out (expected kind {}, latest kind {}, challenge=0x{challenge:08X})",
                    expected_kind,
                    kind_after
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_arm(&self, session: u32, generation: u64) -> anyhow::Result<u16> {
        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        loop {
            if self.demand.lock().unwrap().generation != generation {
                anyhow::bail!("commission ARM canceled");
            }
            self.require_nmt(NMT_OPERATIONAL)?;
            let active_session = sdo::upload_u32(
                &*self.bus,
                self.node_id,
                OD,
                ACTIVE_SESSION_SUB,
                self.sdo_timeout,
            )
            .await?;
            let state =
                sdo::upload_u8(&*self.bus, self.node_id, OD, STATE_SUB, self.sdo_timeout).await?;
            let flags =
                sdo::upload_u8(&*self.bus, self.node_id, OD, FLAGS_SUB, self.sdo_timeout).await?;
            let expected_pulse_id = sdo::upload_u16(
                &*self.bus,
                self.node_id,
                OD,
                EXPECTED_PULSE_SUB,
                self.sdo_timeout,
            )
            .await?;
            let challenge = sdo::upload_u32(
                &*self.bus,
                self.node_id,
                OD,
                CHALLENGE_SUB,
                self.sdo_timeout,
            )
            .await?;
            let challenge_kind = sdo::upload_u8(
                &*self.bus,
                self.node_id,
                OD,
                CHALLENGE_KIND_SUB,
                self.sdo_timeout,
            )
            .await?;
            {
                let mut view = self.view.lock().unwrap();
                view.active_session = active_session;
                view.state = state;
                view.flags = flags;
                view.expected_pulse_id = expected_pulse_id;
                view.challenge = challenge;
                view.challenge_kind = challenge_kind;
            }
            if active_session == session
                && state == STATE_ARMED_IDLE
                && flags & FLAG_ARMED != 0
                && expected_pulse_id != 0
                && challenge == 0
                && challenge_kind == CHALLENGE_KIND_NONE
            {
                return Ok(expected_pulse_id);
            }
            if state == STATE_FAULT_LATCHED || flags & FLAG_FAULT != 0 {
                anyhow::bail!(
                    "firmware rejected commissioning ARM (state=0x{state:02X}, flags=0x{flags:02X})"
                );
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "commission ARM confirmation timed out (active_session=0x{active_session:08X}, state=0x{state:02X}, flags=0x{flags:02X}, expected_pulse={expected_pulse_id})"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn hold(&self, duty_permille: i16) -> anyhow::Result<u16> {
        self.require_gate()?;
        if duty_permille == 0 {
            anyhow::bail!("commissioning hold duty must be non-zero");
        }
        let view = self.view();
        if view.state == STATE_FAULT_LATCHED || view.flags & FLAG_FAULT != 0 {
            anyhow::bail!("commissioning fault is latched");
        }
        if view.state != STATE_ARMED_IDLE || view.gap_remaining_ms != 0 {
            anyhow::bail!(
                "commissioning is not ready for a pulse (state=0x{:02X}, gap={} ms)",
                view.state,
                view.gap_remaining_ms
            );
        }
        if view.flags & FLAG_ARMED == 0 {
            anyhow::bail!("firmware does not report ARMED");
        }
        if duty_permille.unsigned_abs() > view.hard_cap_permille {
            anyhow::bail!(
                "requested duty {}‰ exceeds firmware hard cap {}‰",
                duty_permille.unsigned_abs(),
                view.hard_cap_permille
            );
        }

        let pulse_id = {
            let mut demand = self.demand.lock().unwrap();
            if !demand.armed {
                anyhow::bail!("commissioning session is not armed");
            }
            if demand.holding {
                anyhow::bail!("a commissioning direction is already held");
            }
            if demand.sequence_sync_from.is_some() || demand.wait_for_idle {
                anyhow::bail!("waiting for firmware pulse-sequence feedback");
            }
            if demand.pulse_id == 0 || demand.pulse_id != view.expected_pulse_id {
                anyhow::bail!(
                    "firmware/host pulse sequence is not synchronized (firmware={}, host={})",
                    view.expected_pulse_id,
                    demand.pulse_id
                );
            }
            let pulse_id = demand.pulse_id;
            demand.duty_permille = duty_permille;
            demand.holding = true;
            let now = Instant::now();
            demand.lease_deadline = Some(now + Duration::from_millis(u64::from(demand.lease_ms)));
            demand.hold_started = Some(now);
            pulse_id
        };

        // command_loop is the sole non-zero RPDO3 sender. It takes the shared
        // send gate and revalidates this demand immediately before bus I/O.
        Ok(pulse_id)
    }

    pub fn renew_operator_lease(&self) -> anyhow::Result<()> {
        self.require_gate()?;
        let mut demand = self.demand.lock().unwrap();
        if !demand.armed || !demand.holding {
            anyhow::bail!("commissioning hold is not active");
        }
        demand.lease_deadline =
            Some(Instant::now() + Duration::from_millis(u64::from(demand.lease_ms)));
        Ok(())
    }

    pub async fn release(&self) -> anyhow::Result<()> {
        let frame = self.release_local();
        let Some(frame) = frame else {
            return Ok(());
        };
        if let Err(error) = send_rpdo3_serialized(
            &self.rpdo_gate,
            &self.bus,
            self.node_id,
            frame.session,
            frame.pulse_id,
            0,
        )
        .await
        {
            self.cancel_local();
            let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
            anyhow::bail!("commissioning release zero failed; directed NMT Stop sent: {error}");
        }
        Ok(())
    }

    pub async fn disarm(&self, sdo_gate: &AsyncMutex<()>) -> anyhow::Result<()> {
        let frame = self.cancel_local();
        if let Some(frame) = frame {
            let _ = send_rpdo3_serialized(
                &self.rpdo_gate,
                &self.bus,
                self.node_id,
                frame.session,
                frame.pulse_id,
                0,
            )
            .await;
        }
        let _guard = sdo_gate.lock().await;
        if let Err(error) = self.write_and_confirm_safe_off().await {
            let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
            anyhow::bail!("commission DISARM unconfirmed; directed NMT Stop sent: {error}");
        }
        Ok(())
    }

    /// CAN software E-stop. NMT Stop is deliberately sent before waiting for
    /// the serialized SDO gate. The firmware's NMT handler must coast even if
    /// the following active-session clear confirmation cannot be completed.
    pub async fn emergency_stop(&self, sdo_gate: &AsyncMutex<()>) -> anyhow::Result<()> {
        let frame = self.cancel_local();
        let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
        if let Some(frame) = frame {
            let _ = send_rpdo3_serialized(
                &self.rpdo_gate,
                &self.bus,
                self.node_id,
                frame.session,
                frame.pulse_id,
                0,
            )
            .await;
        }

        let result = async {
            let _guard = sdo_gate.lock().await;
            send_nmt(&self.bus, 0x02, self.node_id).await?;
            send_nmt(&self.bus, 0x80, self.node_id).await?;
            self.wait_for_nmt(NMT_PRE_OPERATIONAL, HEARTBEAT_TIMEOUT)
                .await?;
            self.write_and_confirm_safe_off().await
        }
        .await;

        if let Err(error) = result {
            anyhow::bail!(
                "COMMISSION E-STOP UNCONFIRMED; keep physical power removal available: {error}"
            );
        }
        Ok(())
    }

    /// First, non-blocking part of Lift's shared confirmed shutdown path.
    pub async fn immediate_stop(&self) {
        let frame = self.cancel_local();
        let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
        if let Some(frame) = frame {
            let _ = send_rpdo3_serialized(
                &self.rpdo_gate,
                &self.bus,
                self.node_id,
                frame.session,
                frame.pulse_id,
                0,
            )
            .await;
        }
    }

    /// Called with Lift's SDO gate held after a confirmed Pre-op heartbeat.
    pub async fn confirm_stopped_locked(&self) -> anyhow::Result<()> {
        if self.view.lock().unwrap().available {
            self.write_and_confirm_safe_off().await?;
        }
        Ok(())
    }

    pub fn clear_on_nmt_exit(&self) {
        self.cancel_local();
        self.view.lock().unwrap().host_remaining_ms = 0;
    }

    async fn write_and_confirm_safe_off(&self) -> anyhow::Result<()> {
        if !self.view.lock().unwrap().available {
            return Ok(());
        }
        sdo::download_u32(
            &*self.bus,
            self.node_id,
            OD,
            ACTIVE_SESSION_SUB,
            0,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("write commissioning active_session=0: {e}"))?;

        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        loop {
            let active_session = sdo::upload_u32(
                &*self.bus,
                self.node_id,
                OD,
                ACTIVE_SESSION_SUB,
                self.sdo_timeout,
            )
            .await?;
            let state =
                sdo::upload_u8(&*self.bus, self.node_id, OD, STATE_SUB, self.sdo_timeout).await?;
            let flags =
                sdo::upload_u8(&*self.bus, self.node_id, OD, FLAGS_SUB, self.sdo_timeout).await?;
            {
                let mut view = self.view.lock().unwrap();
                view.active_session = active_session;
                view.state = state;
                view.flags = flags;
            }
            if safe_off_confirmed(active_session, state, flags) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "safe-off readback mismatch (active_session=0x{active_session:08X}, state=0x{state:02X}, flags=0x{flags:02X})"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn clear_fault(&self, sdo_gate: &AsyncMutex<()>) -> anyhow::Result<()> {
        if !self.view.lock().unwrap().available {
            anyhow::bail!("commissioning ABI2 is not available");
        }
        if self.view.lock().unwrap().state != STATE_FAULT_LATCHED {
            anyhow::bail!("commissioning fault is not latched");
        }
        if let Some(frame) = self.cancel_local() {
            if let Err(error) = send_rpdo3_serialized(
                &self.rpdo_gate,
                &self.bus,
                self.node_id,
                frame.session,
                frame.pulse_id,
                0,
            )
            .await
            {
                let _ = send_nmt(&self.bus, 0x02, self.node_id).await;
                anyhow::bail!("fault-clear preflight zero failed; directed NMT Stop sent: {error}");
            }
        }
        let generation = self.demand.lock().unwrap().generation;
        let _guard = sdo_gate.lock().await;
        self.require_nmt(NMT_OPERATIONAL)?;

        let active_session = sdo::upload_u32(
            &*self.bus,
            self.node_id,
            OD,
            ACTIVE_SESSION_SUB,
            self.sdo_timeout,
        )
        .await?;
        let state =
            sdo::upload_u8(&*self.bus, self.node_id, OD, STATE_SUB, self.sdo_timeout).await?;
        let flags =
            sdo::upload_u8(&*self.bus, self.node_id, OD, FLAGS_SUB, self.sdo_timeout).await?;
        if active_session != 0
            || state != STATE_FAULT_LATCHED
            || flags & FLAG_FAULT == 0
            || flags & (FLAG_ARMED | FLAG_OUTPUT_ACTIVE) != 0
        {
            anyhow::bail!(
                "fault-clear preflight mismatch (active_session=0x{active_session:08X}, state=0x{state:02X}, flags=0x{flags:02X})"
            );
        }

        let challenge = self
            .wait_for_challenge(CHALLENGE_KIND_CLEAR_FAULT, generation)
            .await?;
        self.require_nmt(NMT_OPERATIONAL)?;
        if self.demand.lock().unwrap().generation != generation {
            anyhow::bail!("commission fault-clear canceled");
        }
        sdo::download_u32(
            &*self.bus,
            self.node_id,
            OD,
            ACTIVE_SESSION_SUB,
            challenge,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("fault-clear challenge echo failed: {e}"))?;
        self.wait_for_fault_clear(generation).await
    }

    async fn wait_for_fault_clear(&self, generation: u64) -> anyhow::Result<()> {
        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        loop {
            if self.demand.lock().unwrap().generation != generation {
                anyhow::bail!("commission fault-clear canceled");
            }
            self.require_nmt(NMT_OPERATIONAL)?;
            let active_session = sdo::upload_u32(
                &*self.bus,
                self.node_id,
                OD,
                ACTIVE_SESSION_SUB,
                self.sdo_timeout,
            )
            .await?;
            let state =
                sdo::upload_u8(&*self.bus, self.node_id, OD, STATE_SUB, self.sdo_timeout).await?;
            let flags =
                sdo::upload_u8(&*self.bus, self.node_id, OD, FLAGS_SUB, self.sdo_timeout).await?;
            let challenge = sdo::upload_u32(
                &*self.bus,
                self.node_id,
                OD,
                CHALLENGE_SUB,
                self.sdo_timeout,
            )
            .await?;
            let challenge_kind = sdo::upload_u8(
                &*self.bus,
                self.node_id,
                OD,
                CHALLENGE_KIND_SUB,
                self.sdo_timeout,
            )
            .await?;
            {
                let mut view = self.view.lock().unwrap();
                view.active_session = active_session;
                view.state = state;
                view.flags = flags;
                view.challenge = challenge;
                view.challenge_kind = challenge_kind;
            }
            if active_session == 0
                && state == STATE_DISARMED
                && flags & (FLAG_ARMED | FLAG_OUTPUT_ACTIVE | FLAG_FAULT) == 0
                && challenge == 0
                && challenge_kind == CHALLENGE_KIND_NONE
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "fault-clear confirmation timed out (active_session=0x{active_session:08X}, state=0x{state:02X}, flags=0x{flags:02X}, challenge_kind={challenge_kind})"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn epoch_service(
        &self,
        sdo_gate: &AsyncMutex<()>,
        motor_disconnected: bool,
    ) -> anyhow::Result<()> {
        if !motor_disconnected {
            anyhow::bail!(
                "Stage A epoch service requires explicit motor-disconnected confirmation"
            );
        }
        if !self.view.lock().unwrap().available {
            anyhow::bail!("commissioning ABI2 is not available");
        }
        if self.is_armed() {
            anyhow::bail!("commissioning must be safely disarmed before Stage A epoch service");
        }
        let _guard = sdo_gate.lock().await;
        let first = self.read_epoch_service_gate().await?;
        validate_epoch_service_gate(&first)?;
        let second = self.read_epoch_service_gate().await?;
        validate_epoch_service_gate(&second)?;
        let provisioning_salt = random_nonzero_provisioning_salt()?;
        sdo::download_u32(
            &*self.bus,
            self.node_id,
            OD,
            EPOCH_SERVICE_SUB,
            provisioning_salt,
            self.sdo_timeout,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Stage A epoch service write failed: {e}"))?;

        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        loop {
            let snapshot = self.read_epoch_service_gate().await?;
            {
                let mut view = self.view.lock().unwrap();
                view.active_session = snapshot.active_session;
                view.state = snapshot.state;
                view.flags = snapshot.flags;
                view.boot_epoch = snapshot.boot_epoch;
                view.epoch_status = snapshot.epoch_status;
            }
            if snapshot.active_session == 0
                && snapshot.state == STATE_DISARMED
                && snapshot.flags & (FLAG_ARMED | FLAG_OUTPUT_ACTIVE) == 0
                && snapshot.epoch_status == EPOCH_STATUS_READY
                && snapshot.boot_epoch != 0
            {
                return Ok(());
            }
            if matches!(
                snapshot.epoch_status,
                EPOCH_STATUS_EXHAUSTED | EPOCH_STATUS_WRITE_FAILED
            ) {
                anyhow::bail!(
                    "Stage A epoch service failed with status {}",
                    snapshot.epoch_status
                );
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "Stage A epoch service confirmation timed out (epoch={}, status={})",
                    snapshot.boot_epoch,
                    snapshot.epoch_status
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn read_epoch_service_gate(&self) -> anyhow::Result<EpochServiceSnapshot> {
        self.require_nmt(NMT_PRE_OPERATIONAL)?;
        let active_session = sdo::upload_u32(
            &*self.bus,
            self.node_id,
            OD,
            ACTIVE_SESSION_SUB,
            self.sdo_timeout,
        )
        .await?;
        let state =
            sdo::upload_u8(&*self.bus, self.node_id, OD, STATE_SUB, self.sdo_timeout).await?;
        let flags =
            sdo::upload_u8(&*self.bus, self.node_id, OD, FLAGS_SUB, self.sdo_timeout).await?;
        let boot_epoch = sdo::upload_u32(
            &*self.bus,
            self.node_id,
            OD,
            BOOT_EPOCH_SUB,
            self.sdo_timeout,
        )
        .await?;
        let epoch_status = sdo::upload_u8(
            &*self.bus,
            self.node_id,
            OD,
            EPOCH_STATUS_SUB,
            self.sdo_timeout,
        )
        .await?;
        self.require_nmt(NMT_PRE_OPERATIONAL)?;
        Ok(EpochServiceSnapshot {
            active_session,
            state,
            flags,
            boot_epoch,
            epoch_status,
        })
    }

    fn require_gate(&self) -> anyhow::Result<()> {
        sync_freshness(&self.freshness, &self.view);
        validate_gate(&self.base_state.lock().unwrap(), &self.view.lock().unwrap())
    }

    fn require_nmt(&self, expected: u8) -> anyhow::Result<()> {
        let state = self.base_state.lock().unwrap();
        if !state.online || state.nmt_state != expected {
            anyhow::bail!(
                "commissioning requires NMT 0x{expected:02X} (latest=0x{:02X}, online={})",
                state.nmt_state,
                state.online
            );
        }
        Ok(())
    }

    async fn wait_for_nmt(&self, expected: u8, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let (online, actual) = {
                let state = self.base_state.lock().unwrap();
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

    fn release_local(&self) -> Option<DemandFrame> {
        let mut demand = self.demand.lock().unwrap();
        release_demand(&mut demand, Instant::now())
    }

    fn cancel_local(&self) -> Option<DemandFrame> {
        let mut demand = self.demand.lock().unwrap();
        let frame = (demand.session != 0).then_some(DemandFrame {
            session: demand.session,
            pulse_id: demand.pulse_id,
            duty_permille: 0,
            event: DemandEvent::None,
        });
        let generation = demand.generation.wrapping_add(1);
        *demand = Demand {
            generation,
            ..Default::default()
        };
        frame
    }

    pub fn csv(&self) -> anyhow::Result<String> {
        if !self.view.lock().unwrap().available {
            anyhow::bail!("commissioning ABI2 is not available");
        }
        let telemetry = self.telemetry.lock().unwrap();
        let mut csv = String::from(
            "host_unix_ms,node_id,active_session,pulse_id,firmware_tick,raw_count,bus_voltage_v,bus_current_a,requested_duty_permille,applied_duty_permille,state,flags,stop_reason\n",
        );
        for sample in &telemetry.samples {
            use std::fmt::Write as _;
            let _ = writeln!(
                csv,
                "{},{},0x{:08X},{},{},{},{},{},{},{},0x{:02X},0x{:02X},{}",
                sample.host_unix_ms,
                sample.node_id,
                sample.active_session,
                sample.pulse_id,
                sample.tick,
                sample.raw_count,
                sample.bus_voltage_v,
                sample.current_a,
                sample.requested_duty_permille,
                sample.applied_duty_permille,
                sample.state,
                sample.flags,
                sample.stop_reason,
            );
        }
        Ok(csv)
    }

    pub async fn shutdown_tasks(&self) {
        self.running.store(false, Ordering::SeqCst);
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }
}

fn random_nonzero_provisioning_salt() -> anyhow::Result<u32> {
    loop {
        let salt = getrandom::u32().map_err(|error| {
            anyhow::anyhow!("OS random provisioning salt is unavailable: {error}")
        })?;
        if salt != 0 {
            return Ok(salt);
        }
    }
}

fn identity_matches(device_name: &str, highest_subindex: u8, abi: u16) -> bool {
    device_name == DEVICE_NAME && highest_subindex == HIGHEST_SUBINDEX && abi == ABI_VERSION
}

fn coherent_challenge(
    kind_before: u8,
    challenge: u32,
    kind_after: u8,
    expected_kind: u8,
) -> Option<u32> {
    (challenge != 0 && kind_before == expected_kind && kind_after == expected_kind)
        .then_some(challenge)
}

fn safe_off_confirmed(active_session: u32, state: u8, flags: u8) -> bool {
    active_session == 0
        && matches!(state, STATE_DISARMED | STATE_FAULT_LATCHED)
        && flags & (FLAG_ARMED | FLAG_OUTPUT_ACTIVE) == 0
}

fn validate_epoch_service_gate(snapshot: &EpochServiceSnapshot) -> anyhow::Result<()> {
    if snapshot.active_session != 0
        || snapshot.state != STATE_DISARMED
        || snapshot.flags & (FLAG_ARMED | FLAG_OUTPUT_ACTIVE) != 0
    {
        anyhow::bail!(
            "Stage A requires safe Disarmed state (active_session=0x{:08X}, state=0x{:02X}, flags=0x{:02X})",
            snapshot.active_session,
            snapshot.state,
            snapshot.flags
        );
    }
    if snapshot.boot_epoch != 0
        || !matches!(
            snapshot.epoch_status,
            EPOCH_STATUS_MISSING_OR_UNREADABLE | EPOCH_STATUS_CORRUPT
        )
    {
        anyhow::bail!(
            "Stage A epoch service is unavailable (epoch={}, status={}); exhausted/write-failed/ready states are never serviced",
            snapshot.boot_epoch,
            snapshot.epoch_status
        );
    }
    Ok(())
}

fn sequence_feedback(snapshot: SequenceSnapshot, session: u32) -> SequenceFeedback {
    if snapshot.active_session == 0 && snapshot.state == STATE_DISARMED {
        return SequenceFeedback::SessionEnded;
    }
    if snapshot.active_session != session || snapshot.state == STATE_FAULT_LATCHED {
        return SequenceFeedback::Unsafe;
    }
    if snapshot.expected_pulse_id == 0 {
        return SequenceFeedback::SessionEnded;
    }
    if snapshot.state == STATE_ARMED_IDLE {
        return SequenceFeedback::Ready {
            pulse_id: snapshot.expected_pulse_id,
            idle: true,
        };
    }
    if matches!(snapshot.state, 2..=4) && snapshot.active_pulse != u16::MAX {
        return if snapshot.active_pulse == 0 {
            SequenceFeedback::Unsafe
        } else {
            SequenceFeedback::Active(snapshot.active_pulse)
        };
    }
    if snapshot.state == 4 && snapshot.active_pulse == u16::MAX {
        return SequenceFeedback::Ready {
            pulse_id: snapshot.expected_pulse_id,
            // 0xFFFF is both the no-active-pulse sentinel and a valid final
            // pulse id. Only explicit ArmedIdle feedback may end idle polling.
            idle: false,
        };
    }
    SequenceFeedback::Pending
}

fn validate_gate(base: &LiftState, view: &CommissionView) -> anyhow::Result<()> {
    if !view.available || view.highest_subindex != HIGHEST_SUBINDEX || view.abi != ABI_VERSION {
        anyhow::bail!(
            "device is not exact {DEVICE_NAME} commissioning ABI2 (highest={}, ABI={})",
            view.highest_subindex,
            view.abi
        );
    }
    if !base.online {
        anyhow::bail!("lift heartbeat is stale");
    }
    if base.nmt_state != NMT_OPERATIONAL {
        anyhow::bail!("lift is not NMT Operational");
    }
    if !view.tpdo3_fresh || !view.tpdo4_fresh || !view.pair_fresh {
        anyhow::bail!(
            "commissioning telemetry is stale (TPDO3={}, TPDO4={}, paired={})",
            view.tpdo3_fresh,
            view.tpdo4_fresh,
            view.pair_fresh
        );
    }
    if view.epoch_status != EPOCH_STATUS_READY || view.boot_epoch == 0 {
        anyhow::bail!(
            "commissioning anti-replay epoch is not ready (status={}, epoch={})",
            view.epoch_status,
            view.boot_epoch
        );
    }
    if view.hard_cap_permille == 0 {
        anyhow::bail!("firmware commissioning hard cap is zero");
    }
    if view.lease_ms <= COMMAND_PERIOD.as_millis() as u16 {
        anyhow::bail!(
            "firmware commissioning lease {} ms is not longer than the {} ms host period",
            view.lease_ms,
            COMMAND_PERIOD.as_millis()
        );
    }
    if view.max_pulse_ms == 0 {
        anyhow::bail!("firmware commissioning max pulse is zero");
    }
    if view.state == STATE_FAULT_LATCHED || view.flags & FLAG_FAULT != 0 {
        anyhow::bail!(
            "firmware commissioning fault is latched (reason={})",
            view.stop_reason
        );
    }
    Ok(())
}

async fn command_loop(
    bus: Arc<dyn CanBus>,
    node_id: u8,
    view: Arc<StdMutex<CommissionView>>,
    demand: Arc<StdMutex<Demand>>,
    rpdo_gate: Arc<AsyncMutex<()>>,
    freshness: Arc<StdMutex<Freshness>>,
    base_state: Arc<StdMutex<LiftState>>,
    running: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(COMMAND_PERIOD);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while running.load(Ordering::SeqCst) {
        interval.tick().await;
        sync_freshness(&freshness, &view);

        let now = Instant::now();
        let sync_expired = {
            let demand = demand.lock().unwrap();
            sequence_sync_expired(&demand, now)
        };
        if sync_expired {
            cancel_shared_demand(&demand);
            let _ = send_nmt(&bus, 0x02, node_id).await;
            base_state.lock().unwrap().last_error = Some(
                "firmware pulse-sequence feedback exceeded 150 ms; directed NMT Stop sent".into(),
            );
            continue;
        }

        let frame = {
            let mut demand = demand.lock().unwrap();
            demand_frame(&mut demand, now)
        };
        let Some(frame) = frame else {
            continue;
        };

        let blocker = {
            let base = base_state.lock().unwrap();
            let view = view.lock().unwrap();
            if !base.online {
                Some("heartbeat became stale")
            } else if base.nmt_state != NMT_OPERATIONAL {
                Some("NMT left Operational")
            } else if !view.available {
                Some("commissioning ABI became unavailable")
            } else if !view.tpdo3_fresh || !view.tpdo4_fresh || !view.pair_fresh {
                Some("commissioning telemetry became stale")
            } else {
                None
            }
        };
        if let Some(reason) = blocker {
            cancel_shared_demand(&demand);
            let _ = send_nmt(&bus, 0x02, node_id).await;
            base_state.lock().unwrap().last_error = Some(format!(
                "commissioning stream stopped ({reason}); directed NMT Stop sent"
            ));
            continue;
        }

        if frame.event != DemandEvent::None {
            let reason = match frame.event {
                DemandEvent::OperatorLeaseExpired => "operator lease expired; zero sent",
                DemandEvent::HostPulseExpired => "host pulse deadline reached; zero sent",
                DemandEvent::None => unreachable!(),
            };
            base_state.lock().unwrap().last_error = Some(reason.into());
        }

        let send_result = {
            let _send_guard = rpdo_gate.lock().await;
            if frame.duty_permille != 0
                && !nonzero_frame_is_current(&demand.lock().unwrap(), frame, Instant::now())
            {
                continue;
            }
            send_rpdo3(
                &bus,
                node_id,
                frame.session,
                frame.pulse_id,
                frame.duty_permille,
            )
            .await
        };
        if let Err(error) = send_result {
            cancel_shared_demand(&demand);
            let _ = send_nmt(&bus, 0x02, node_id).await;
            base_state.lock().unwrap().last_error = Some(format!(
                "commissioning RPDO3 failed; directed NMT Stop sent: {error}"
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn sequence_sync_loop(
    bus: Arc<dyn CanBus>,
    node_id: u8,
    sdo_timeout: Option<Duration>,
    sdo_gate: Arc<AsyncMutex<()>>,
    view: Arc<StdMutex<CommissionView>>,
    demand: Arc<StdMutex<Demand>>,
    base_state: Arc<StdMutex<LiftState>>,
    running: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while running.load(Ordering::SeqCst) {
        interval.tick().await;
        let request = {
            let demand = demand.lock().unwrap();
            (demand.sequence_sync_from.is_some() || demand.wait_for_idle).then_some((
                demand.generation,
                demand.session,
                demand.sequence_sync_from,
                demand.sequence_sync_deadline,
                demand.pulse_id,
            ))
        };
        let Some((generation, session, sync_from, deadline, current_pulse)) = request else {
            continue;
        };

        let budget = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|| sdo_timeout.unwrap_or(CONFIRM_TIMEOUT));
        if sync_from.is_some() && budget.is_zero() {
            sequence_sync_fail_safe(
                &bus,
                node_id,
                &demand,
                &base_state,
                "firmware pulse-sequence feedback timed out before the 200 ms gap ended",
            )
            .await;
            continue;
        }

        let read = async {
            let _guard = sdo_gate.lock().await;
            read_sequence_snapshot(&*bus, node_id, sdo_timeout).await
        };
        let snapshot = match tokio::time::timeout(budget, read).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => {
                if sync_from
                    .is_some_and(|_| deadline.is_some_and(|deadline| Instant::now() >= deadline))
                {
                    sequence_sync_fail_safe(
                        &bus,
                        node_id,
                        &demand,
                        &base_state,
                        &format!("firmware pulse-sequence feedback failed: {error}"),
                    )
                    .await;
                }
                continue;
            }
            Err(_) => {
                sequence_sync_fail_safe(
                    &bus,
                    node_id,
                    &demand,
                    &base_state,
                    "firmware pulse-sequence feedback timed out before the 200 ms gap ended",
                )
                .await;
                continue;
            }
        };

        {
            let mut view = view.lock().unwrap();
            view.active_session = snapshot.active_session;
            view.state = snapshot.state;
            view.active_pulse = snapshot.active_pulse;
            view.gap_remaining_ms = snapshot.gap_remaining_ms;
            view.expected_pulse_id = snapshot.expected_pulse_id;
        }

        match sequence_feedback(snapshot, session) {
            SequenceFeedback::Ready { pulse_id, idle } => {
                let mut demand = demand.lock().unwrap();
                if demand.generation != generation || demand.session != session {
                    continue;
                }
                if sync_from.is_some() && demand.sequence_sync_from != sync_from {
                    continue;
                }
                if sync_from.is_none() && current_pulse != demand.pulse_id {
                    continue;
                }
                apply_ready_sequence(&mut demand, sync_from, pulse_id, idle);
            }
            SequenceFeedback::Active(pulse_id) => {
                let mut demand = demand.lock().unwrap();
                if demand.generation != generation
                    || demand.session != session
                    || demand.sequence_sync_from != sync_from
                {
                    continue;
                }
                demand.pulse_id = pulse_id;
                demand.sequence_sync_from = Some(pulse_id);
            }
            SequenceFeedback::Pending => {}
            SequenceFeedback::SessionEnded => {
                cancel_shared_demand(&demand);
                base_state.lock().unwrap().last_error = Some(
                    "commissioning session ended while synchronizing firmware pulse sequence"
                        .into(),
                );
            }
            SequenceFeedback::Unsafe => {
                sequence_sync_fail_safe(
                    &bus,
                    node_id,
                    &demand,
                    &base_state,
                    "unsafe commissioning state while synchronizing firmware pulse sequence",
                )
                .await;
            }
        }
    }
}

async fn read_sequence_snapshot(
    bus: &(impl CanBus + ?Sized),
    node_id: u8,
    timeout: Option<Duration>,
) -> anyhow::Result<SequenceSnapshot> {
    let active_session = sdo::upload_u32(bus, node_id, OD, ACTIVE_SESSION_SUB, timeout).await?;
    let state = sdo::upload_u8(bus, node_id, OD, STATE_SUB, timeout).await?;
    let active_pulse = sdo::upload_u16(bus, node_id, OD, 0x0e, timeout).await?;
    let gap_remaining_ms = sdo::upload_u16(bus, node_id, OD, 0x12, timeout).await?;
    let expected_pulse_id = sdo::upload_u16(bus, node_id, OD, EXPECTED_PULSE_SUB, timeout).await?;
    Ok(SequenceSnapshot {
        active_session,
        state,
        active_pulse,
        gap_remaining_ms,
        expected_pulse_id,
    })
}

async fn sequence_sync_fail_safe(
    bus: &Arc<dyn CanBus>,
    node_id: u8,
    demand: &StdMutex<Demand>,
    base_state: &StdMutex<LiftState>,
    reason: &str,
) {
    cancel_shared_demand(demand);
    let _ = send_nmt(bus, 0x02, node_id).await;
    base_state.lock().unwrap().last_error = Some(format!("{reason}; directed NMT Stop sent"));
}

fn cancel_shared_demand(demand: &StdMutex<Demand>) {
    let mut demand = demand.lock().unwrap();
    let generation = demand.generation.wrapping_add(1);
    *demand = Demand {
        generation,
        ..Default::default()
    };
}

#[allow(clippy::too_many_arguments)]
async fn tpdo3_loop(
    mut rx: Box<dyn CanRx>,
    node_id: u8,
    view: Arc<StdMutex<CommissionView>>,
    demand: Arc<StdMutex<Demand>>,
    telemetry: Arc<StdMutex<Telemetry>>,
    freshness: Arc<StdMutex<Freshness>>,
    base_state: Arc<StdMutex<LiftState>>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::SeqCst) {
        match tokio::time::timeout(TPDO_TIMEOUT, rx.recv()).await {
            Ok(Ok(frame)) if frame.kind() == FrameKind::Data => {
                if let Some(sample) = parse_tpdo3(frame.data()) {
                    freshness.lock().unwrap().tpdo3 = Some(Instant::now());
                    let joined = telemetry.lock().unwrap().joiner.push_tpdo3(sample);
                    if let Some(joined) = joined {
                        record_joined(
                            node_id,
                            joined,
                            &view,
                            &demand,
                            &telemetry,
                            &freshness,
                            &base_state,
                        );
                    }
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                freshness.lock().unwrap().tpdo3 = None;
                base_state.lock().unwrap().last_error =
                    Some(format!("commissioning TPDO3 receive: {error}"));
                break;
            }
            Err(_) => {
                freshness.lock().unwrap().tpdo3 = None;
            }
        }
    }
    log::info!("Lift 0x{node_id:02X}: commissioning TPDO3 loop stopped");
}

#[allow(clippy::too_many_arguments)]
async fn tpdo4_loop(
    mut rx: Box<dyn CanRx>,
    node_id: u8,
    view: Arc<StdMutex<CommissionView>>,
    demand: Arc<StdMutex<Demand>>,
    telemetry: Arc<StdMutex<Telemetry>>,
    freshness: Arc<StdMutex<Freshness>>,
    base_state: Arc<StdMutex<LiftState>>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::SeqCst) {
        match tokio::time::timeout(TPDO_TIMEOUT, rx.recv()).await {
            Ok(Ok(frame)) if frame.kind() == FrameKind::Data => {
                if let Some(sample) = parse_tpdo4(frame.data()) {
                    freshness.lock().unwrap().tpdo4 = Some(Instant::now());
                    let joined = telemetry.lock().unwrap().joiner.push_tpdo4(sample);
                    if let Some(joined) = joined {
                        record_joined(
                            node_id,
                            joined,
                            &view,
                            &demand,
                            &telemetry,
                            &freshness,
                            &base_state,
                        );
                    }
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                freshness.lock().unwrap().tpdo4 = None;
                base_state.lock().unwrap().last_error =
                    Some(format!("commissioning TPDO4 receive: {error}"));
                break;
            }
            Err(_) => {
                freshness.lock().unwrap().tpdo4 = None;
            }
        }
    }
    log::info!("Lift 0x{node_id:02X}: commissioning TPDO4 loop stopped");
}

fn record_joined(
    node_id: u8,
    joined: Joined,
    view: &StdMutex<CommissionView>,
    demand: &StdMutex<Demand>,
    telemetry: &StdMutex<Telemetry>,
    freshness: &StdMutex<Freshness>,
    base_state: &StdMutex<LiftState>,
) {
    let now = Instant::now();
    freshness.lock().unwrap().pair = Some(now);
    let demand = *demand.lock().unwrap();
    let (bus_voltage_v, state, flags, stop_reason) = {
        let base = base_state.lock().unwrap();
        let view = view.lock().unwrap();
        (base.bus_voltage_v, view.state, view.flags, view.stop_reason)
    };
    {
        let mut view = view.lock().unwrap();
        view.tick = joined.tick;
        view.raw_count = joined.raw_count;
        view.current_a = joined.current_a;
        view.requested_duty_permille = joined.requested_duty_permille;
        view.applied_duty_permille = joined.applied_duty_permille;
    }

    let host_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let mut telemetry = telemetry.lock().unwrap();
    if telemetry.samples.len() == TELEMETRY_CAPACITY {
        telemetry.samples.pop_front();
    }
    telemetry.samples.push_back(CsvSample {
        host_unix_ms,
        node_id,
        active_session: demand.session,
        pulse_id: demand.pulse_id,
        tick: joined.tick,
        raw_count: joined.raw_count,
        current_a: joined.current_a,
        requested_duty_permille: joined.requested_duty_permille,
        applied_duty_permille: joined.applied_duty_permille,
        bus_voltage_v,
        state,
        flags,
        stop_reason,
    });
}

fn sync_freshness(freshness: &StdMutex<Freshness>, view: &StdMutex<CommissionView>) {
    let now = Instant::now();
    let (tpdo3, tpdo4, pair) = {
        let freshness = freshness.lock().unwrap();
        (
            is_fresh(freshness.tpdo3, now),
            is_fresh(freshness.tpdo4, now),
            is_fresh(freshness.pair, now),
        )
    };
    let mut view = view.lock().unwrap();
    view.tpdo3_fresh = tpdo3;
    view.tpdo4_fresh = tpdo4;
    view.pair_fresh = pair;
}

fn is_fresh(last: Option<Instant>, now: Instant) -> bool {
    last.is_some_and(|last| now.saturating_duration_since(last) < TPDO_TIMEOUT)
}

fn parse_tpdo3(data: &[u8]) -> Option<Tpdo3> {
    if data.len() != 8 {
        return None;
    }
    Some(Tpdo3 {
        tick: u16::from_le_bytes(data[0..2].try_into().ok()?),
        raw_count: i32::from_le_bytes(data[2..6].try_into().ok()?),
        applied_duty_permille: i16::from_le_bytes(data[6..8].try_into().ok()?),
    })
}

fn parse_tpdo4(data: &[u8]) -> Option<Tpdo4> {
    if data.len() != 8 {
        return None;
    }
    let current_a = f32::from_le_bytes(data[2..6].try_into().ok()?);
    if !current_a.is_finite() {
        return None;
    }
    Some(Tpdo4 {
        tick: u16::from_le_bytes(data[0..2].try_into().ok()?),
        current_a,
        requested_duty_permille: i16::from_le_bytes(data[6..8].try_into().ok()?),
    })
}

fn encode_rpdo3(session: u32, pulse_id: u16, duty_permille: i16) -> [u8; 8] {
    let mut data = [0u8; 8];
    data[0..4].copy_from_slice(&session.to_le_bytes());
    data[4..6].copy_from_slice(&pulse_id.to_le_bytes());
    data[6..8].copy_from_slice(&duty_permille.to_le_bytes());
    data
}

async fn send_rpdo3(
    bus: &Arc<dyn CanBus>,
    node_id: u8,
    session: u32,
    pulse_id: u16,
    duty_permille: i16,
) -> anyhow::Result<()> {
    let data = encode_rpdo3(session, pulse_id, duty_permille);
    let frame = CanFrame::new_data(rpdo3_cob_id(node_id), &data)
        .map_err(|e| anyhow::anyhow!("build commissioning RPDO3: {e}"))?;
    bus.send(frame)
        .await
        .map_err(|e| anyhow::anyhow!("send commissioning RPDO3: {e}"))
}

async fn send_rpdo3_serialized(
    gate: &AsyncMutex<()>,
    bus: &Arc<dyn CanBus>,
    node_id: u8,
    session: u32,
    pulse_id: u16,
    duty_permille: i16,
) -> anyhow::Result<()> {
    let _guard = gate.lock().await;
    send_rpdo3(bus, node_id, session, pulse_id, duty_permille).await
}

async fn send_nmt(bus: &Arc<dyn CanBus>, cs: u8, node_id: u8) -> anyhow::Result<()> {
    let frame = CanFrame::new_data(0x000u16, &[cs, node_id])
        .map_err(|e| anyhow::anyhow!("build NMT frame: {e}"))?;
    bus.send(frame)
        .await
        .map_err(|e| anyhow::anyhow!("send NMT: {e}"))
}

async fn read_i16(
    bus: &(impl CanBus + ?Sized),
    nid: u8,
    index: u16,
    sub: u8,
    timeout: Option<Duration>,
) -> anyhow::Result<i16> {
    let raw = sdo::upload(bus, nid, index, sub, timeout).await?;
    if raw.len() != 2 {
        anyhow::bail!(
            "0x{index:04X}:{sub:02X}: expected exact i16 width, got {} bytes",
            raw.len()
        );
    }
    Ok(i16::from_le_bytes(raw[..2].try_into().unwrap()))
}

const fn tpdo3_cob_id(node_id: u8) -> u16 {
    0x380 + node_id as u16
}

const fn tpdo4_cob_id(node_id: u8) -> u16 {
    0x480 + node_id as u16
}

const fn rpdo3_cob_id(node_id: u8) -> u16 {
    0x400 + node_id as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;
    use can_transport::{CanCapabilities, CanIoError};

    struct AlwaysFailBus {
        sends: AtomicUsize,
    }

    #[async_trait]
    impl CanBus for AlwaysFailBus {
        async fn send(&self, _frame: CanFrame) -> Result<(), CanIoError> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Err(CanIoError::Disconnected)
        }

        async fn subscribe(&self, _filter: CanFilter) -> Result<Box<dyn CanRx>, CanIoError> {
            Err(CanIoError::Disconnected)
        }

        fn capabilities(&self) -> CanCapabilities {
            CanCapabilities {
                fd: false,
                max_dlen: 8,
            }
        }
    }

    fn valid_base() -> LiftState {
        LiftState {
            online: true,
            nmt_state: NMT_OPERATIONAL,
            ..Default::default()
        }
    }

    fn valid_view() -> CommissionView {
        CommissionView {
            available: true,
            highest_subindex: HIGHEST_SUBINDEX,
            abi: ABI_VERSION,
            boot_epoch: 1,
            epoch_status: EPOCH_STATUS_READY,
            tpdo3_fresh: true,
            tpdo4_fresh: true,
            pair_fresh: true,
            hard_cap_permille: 100,
            lease_ms: 100,
            max_pulse_ms: 250,
            ..Default::default()
        }
    }

    #[test]
    fn identity_gate_is_exact() {
        assert!(identity_matches(
            "hexmeow-lift-commission",
            HIGHEST_SUBINDEX,
            ABI_VERSION
        ));
        assert!(!identity_matches(
            "hexmeow-lift-commission",
            HIGHEST_SUBINDEX - 1,
            ABI_VERSION
        ));
        assert!(!identity_matches(
            "hexmeow-lift-commission",
            HIGHEST_SUBINDEX,
            1
        ));
        assert!(!identity_matches(
            "hexmeow-lift-commission ",
            HIGHEST_SUBINDEX,
            ABI_VERSION
        ));
    }

    #[test]
    fn rpdo3_encoding_is_exact_little_endian() {
        assert_eq!(
            encode_rpdo3(0x1234_5678, 0x9abc, -321),
            [0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a, 0xbf, 0xfe]
        );
        assert_eq!(rpdo3_cob_id(20), 0x414);
        assert_eq!(tpdo3_cob_id(20), 0x394);
        assert_eq!(tpdo4_cob_id(20), 0x494);
    }

    #[test]
    fn parses_and_pairs_frozen_tpdos_by_tick() {
        let mut raw3 = [0u8; 8];
        raw3[0..2].copy_from_slice(&0xfffeu16.to_le_bytes());
        raw3[2..6].copy_from_slice(&(-123i32).to_le_bytes());
        raw3[6..8].copy_from_slice(&45i16.to_le_bytes());
        let mut raw4 = [0u8; 8];
        raw4[0..2].copy_from_slice(&0xfffeu16.to_le_bytes());
        raw4[2..6].copy_from_slice(&4.25f32.to_le_bytes());
        raw4[6..8].copy_from_slice(&50i16.to_le_bytes());

        let tpdo3 = parse_tpdo3(&raw3).unwrap();
        let tpdo4 = parse_tpdo4(&raw4).unwrap();
        let mut joiner = PairJoiner::default();
        assert!(joiner.push_tpdo4(tpdo4).is_none());
        assert_eq!(
            joiner.push_tpdo3(tpdo3),
            Some(Joined {
                tick: 0xfffe,
                raw_count: -123,
                applied_duty_permille: 45,
                current_a: 4.25,
                requested_duty_permille: 50,
            })
        );
        assert!(parse_tpdo3(&raw3[..7]).is_none());
        assert!(parse_tpdo4(&raw4[..7]).is_none());
        raw4[2..6].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(parse_tpdo4(&raw4).is_none());
    }

    #[test]
    fn joiner_drops_old_tick_and_handles_u16_wrap() {
        let mut joiner = PairJoiner::default();
        assert!(joiner
            .push_tpdo3(Tpdo3 {
                tick: u16::MAX,
                raw_count: 0,
                applied_duty_permille: 0,
            })
            .is_none());
        assert!(joiner
            .push_tpdo4(Tpdo4 {
                tick: 0,
                current_a: 0.0,
                requested_duty_permille: 0,
            })
            .is_none());
        assert!(joiner
            .push_tpdo3(Tpdo3 {
                tick: 0,
                raw_count: 1,
                applied_duty_permille: 2,
            })
            .is_some());
        assert_eq!(joiner.dropped, 1);
    }

    #[test]
    fn gate_does_not_depend_on_formal_motion_sensor_bits() {
        let base = valid_base();
        let view = valid_view();
        assert!(validate_gate(&base, &view).is_ok());

        let mut bad = view.clone();
        bad.abi = 1;
        assert!(validate_gate(&base, &bad).is_err());
        bad = view.clone();
        bad.pair_fresh = false;
        assert!(validate_gate(&base, &bad).is_err());
        bad = view.clone();
        bad.lease_ms = COMMAND_PERIOD.as_millis() as u16;
        assert!(validate_gate(&base, &bad).is_err());
        bad = view;
        bad.flags = FLAG_FAULT;
        assert!(validate_gate(&base, &bad).is_err());
    }

    #[test]
    fn operator_lease_and_host_pulse_expiry_emit_zero_but_keep_arm() {
        let now = Instant::now();
        let mut demand = Demand {
            session: 7,
            pulse_id: 9,
            duty_permille: -50,
            armed: true,
            holding: true,
            lease_deadline: Some(now + Duration::from_millis(100)),
            hold_started: Some(now),
            max_pulse_ms: 250,
            lease_ms: 100,
            ..Default::default()
        };
        let frame = demand_frame(&mut demand, now + Duration::from_millis(99)).unwrap();
        assert_eq!(frame.duty_permille, -50);
        assert_eq!(frame.event, DemandEvent::None);
        assert!(demand.holding);

        let frame = demand_frame(&mut demand, now + Duration::from_millis(100)).unwrap();
        assert_eq!(frame.duty_permille, 0);
        assert_eq!(frame.event, DemandEvent::OperatorLeaseExpired);
        assert!(demand.armed);
        assert!(!demand.holding);
        assert_eq!(demand.sequence_sync_from, Some(9));
        assert!(demand.sequence_sync_deadline.is_some());
        assert!(!sequence_sync_expired(
            &demand,
            now + Duration::from_millis(249)
        ));
        assert!(sequence_sync_expired(
            &demand,
            now + Duration::from_millis(250)
        ));

        demand.sequence_sync_from = None;
        demand.sequence_sync_deadline = None;
        demand.wait_for_idle = false;
        demand.holding = true;
        demand.duty_permille = 50;
        demand.lease_deadline = Some(now + Duration::from_secs(1));
        demand.hold_started = Some(now - Duration::from_millis(250));
        let frame = demand_frame(&mut demand, now).unwrap();
        assert_eq!(frame.duty_permille, 0);
        assert_eq!(frame.event, DemandEvent::HostPulseExpired);
        assert!(demand.armed);
    }

    #[test]
    fn release_invalidates_a_stale_nonzero_before_serialized_send() {
        let now = Instant::now();
        let mut demand = Demand {
            session: 7,
            pulse_id: 9,
            duty_permille: 50,
            armed: true,
            holding: true,
            lease_deadline: Some(now + Duration::from_millis(100)),
            hold_started: Some(now),
            max_pulse_ms: 250,
            lease_ms: 100,
            ..Default::default()
        };
        let stale = demand_frame(&mut demand, now).unwrap();
        assert_eq!(stale.duty_permille, 50);
        assert!(nonzero_frame_is_current(&demand, stale, now));

        // release_local performs this transition before waiting for rpdo_gate.
        // If its zero acquires the gate first, command_loop must reject the
        // previously captured non-zero when it later acquires that same gate.
        let zero = release_demand(&mut demand, now + Duration::from_millis(1)).unwrap();
        assert_eq!(zero.duty_permille, 0);
        assert!(!nonzero_frame_is_current(
            &demand,
            stale,
            now + Duration::from_millis(1)
        ));
    }

    #[test]
    fn challenge_must_be_nonzero_and_kind_coherent() {
        assert_eq!(
            coherent_challenge(1, 0x1234_5678, 1, CHALLENGE_KIND_ARM),
            Some(0x1234_5678)
        );
        assert_eq!(coherent_challenge(1, 0, 1, CHALLENGE_KIND_ARM), None);
        assert_eq!(coherent_challenge(1, 7, 2, CHALLENGE_KIND_ARM), None);
        assert_eq!(coherent_challenge(2, 7, 2, CHALLENGE_KIND_ARM), None);
    }

    #[test]
    fn pulse_sequence_switches_only_from_firmware_feedback() {
        let session = 0x1234_5678;
        let pending = SequenceSnapshot {
            active_session: session,
            state: 2,
            active_pulse: 9,
            gap_remaining_ms: 0,
            expected_pulse_id: 10,
        };
        assert_eq!(
            sequence_feedback(pending, session),
            SequenceFeedback::Active(9)
        );

        let gap = SequenceSnapshot {
            active_session: session,
            state: 4,
            active_pulse: u16::MAX,
            gap_remaining_ms: 125,
            expected_pulse_id: 10,
        };
        assert_eq!(
            sequence_feedback(gap, session),
            SequenceFeedback::Ready {
                pulse_id: 10,
                idle: false
            }
        );

        let idle = SequenceSnapshot {
            state: STATE_ARMED_IDLE,
            gap_remaining_ms: 0,
            ..gap
        };
        assert_eq!(
            sequence_feedback(idle, session),
            SequenceFeedback::Ready {
                pulse_id: 10,
                idle: true
            }
        );
    }

    #[test]
    fn final_pulse_keeps_the_hard_sync_deadline_until_session_end() {
        let now = Instant::now();
        let mut demand = Demand {
            session: 7,
            pulse_id: u16::MAX,
            armed: true,
            sequence_sync_from: Some(u16::MAX),
            sequence_sync_deadline: Some(now + SEQUENCE_SYNC_TIMEOUT),
            wait_for_idle: true,
            ..Default::default()
        };
        apply_ready_sequence(&mut demand, Some(u16::MAX), u16::MAX, false);
        assert_eq!(demand.sequence_sync_from, Some(u16::MAX));
        assert!(demand.sequence_sync_deadline.is_some());
        assert!(demand.wait_for_idle);
    }

    #[test]
    fn final_pulse_sequence_waits_for_explicit_session_end() {
        let session = 0x1234_5678;
        let ambiguous_wait_release = SequenceSnapshot {
            active_session: session,
            state: 4,
            active_pulse: u16::MAX,
            gap_remaining_ms: 0,
            expected_pulse_id: u16::MAX,
        };
        assert_eq!(
            sequence_feedback(ambiguous_wait_release, session),
            SequenceFeedback::Ready {
                pulse_id: u16::MAX,
                idle: false
            }
        );

        let ended = SequenceSnapshot {
            active_session: 0,
            state: STATE_DISARMED,
            active_pulse: u16::MAX,
            gap_remaining_ms: 0,
            expected_pulse_id: 0,
        };
        assert_eq!(
            sequence_feedback(ended, session),
            SequenceFeedback::SessionEnded
        );
    }

    #[test]
    fn stage_a_service_and_fault_safe_off_are_fail_closed() {
        let mut snapshot = EpochServiceSnapshot {
            active_session: 0,
            state: STATE_DISARMED,
            flags: 0,
            boot_epoch: 0,
            epoch_status: EPOCH_STATUS_MISSING_OR_UNREADABLE,
        };
        assert!(validate_epoch_service_gate(&snapshot).is_ok());
        snapshot.epoch_status = EPOCH_STATUS_CORRUPT;
        assert!(validate_epoch_service_gate(&snapshot).is_ok());
        snapshot.epoch_status = EPOCH_STATUS_EXHAUSTED;
        assert!(validate_epoch_service_gate(&snapshot).is_err());
        snapshot.epoch_status = EPOCH_STATUS_WRITE_FAILED;
        assert!(validate_epoch_service_gate(&snapshot).is_err());
        snapshot.epoch_status = EPOCH_STATUS_READY;
        assert!(validate_epoch_service_gate(&snapshot).is_err());

        assert!(safe_off_confirmed(0, STATE_DISARMED, 0));
        assert!(safe_off_confirmed(0, STATE_FAULT_LATCHED, FLAG_FAULT));
        assert!(!safe_off_confirmed(
            0,
            STATE_FAULT_LATCHED,
            FLAG_FAULT | FLAG_OUTPUT_ACTIVE
        ));
        assert!(!safe_off_confirmed(1, STATE_DISARMED, 0));
    }

    #[tokio::test]
    async fn immediate_stop_cancels_local_demand_when_all_can_sends_fail() {
        let bus = Arc::new(AlwaysFailBus {
            sends: AtomicUsize::new(0),
        });
        let demand = Arc::new(StdMutex::new(Demand {
            session: 7,
            pulse_id: 9,
            duty_permille: 50,
            armed: true,
            holding: true,
            lease_deadline: Some(Instant::now() + Duration::from_millis(100)),
            hold_started: Some(Instant::now()),
            max_pulse_ms: 250,
            lease_ms: 100,
            ..Default::default()
        }));
        let commissioning = Commissioning {
            node_id: 20,
            bus: bus.clone(),
            sdo_timeout: None,
            sdo_gate: Arc::new(AsyncMutex::new(())),
            rpdo_gate: Arc::new(AsyncMutex::new(())),
            base_state: Arc::new(StdMutex::new(LiftState::default())),
            view: Arc::new(StdMutex::new(CommissionView::default())),
            demand: demand.clone(),
            telemetry: Arc::new(StdMutex::new(Telemetry::default())),
            freshness: Arc::new(StdMutex::new(Freshness::default())),
            running: Arc::new(AtomicBool::new(false)),
            tasks: StdMutex::new(Vec::new()),
        };

        commissioning.immediate_stop().await;

        let demand = *demand.lock().unwrap();
        assert!(!demand.armed);
        assert!(!demand.holding);
        assert_eq!(demand.session, 0);
        assert_eq!(bus.sends.load(Ordering::SeqCst), 2);
    }
}
