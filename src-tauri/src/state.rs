//! Tauri-managed app state.
//!
//! Holds at most one [`Cia402Manager`] at a time (one CAN bus per app
//! lifetime). All commands acquire the async mutex, clone the `Arc` out of
//! the guard, and drop the guard before awaiting any motor I/O so callers
//! can run concurrently.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};

use hex_motor::cia402::Cia402Manager;
use tokio::sync::Mutex;

use crate::hopea3::{Hopea3, InitProgress};
use crate::lift::LiftSession;
use crate::logging::LogHandle;
use crate::unified_smartknob::ActiveSmartKnob;

#[derive(Default)]
pub struct AppState {
    /// Set synchronously by the native close handler. Long SmartKnob startup
    /// transactions poll this flag between bounded bus operations so shutdown
    /// can roll them back before waiting for the lifecycle lock.
    pub shutdown_requested: AtomicBool,
    /// Serialises physical adapter open/close operations. Tauri commands can
    /// otherwise race a slow `connect` against `disconnect` and publish only
    /// half of the manager/monitor pair.
    pub connection_op: Mutex<()>,
    pub manager: Mutex<Option<Arc<Cia402Manager>>>,
    /// Active CSV recorders, keyed by node id. Inserted by `start_log`,
    /// removed by `stop_log` / `disconnect`. A `std` mutex is fine: we only
    /// ever insert/remove under it, never await while holding it.
    pub logs: StdMutex<HashMap<u8, LogHandle>>,
    /// The running HopeA3 Robot Application, if started. At most one at a time
    /// (it owns the 500 Hz control loop on the single bus).
    pub hopea3: Mutex<Option<Hopea3>>,
    /// Live init progress for the UI to poll while `hopea3_start` runs. A `std`
    /// mutex: only short, await-free updates happen under it.
    pub hopea3_init: StdMutex<InitProgress>,
    /// Direct-CANopen lift debug session. It owns heartbeat/TPDO subscriptions
    /// and the velocity watchdog stream for exactly one lift node.
    pub lift: Mutex<Option<Arc<LiftSession>>>,
    /// Base(Zenoh):到 hex-controller 的连接(至多一条)。
    pub zenoh: Mutex<Option<crate::zenoh_base::ZenohConn>>,
    /// Arm(Zenoh):到 hex-controller 机械臂的连接(至多一条)。
    pub zenoh_arm: Mutex<Option<crate::zenoh_arm::ZenohArmConn>>,
    /// Controller Config(Zenoh):到 hex-controller launcher 的连接(读写 launch.yaml,至多一条)。
    pub config: Mutex<Option<crate::zenoh_config::ZenohConfigConn>>,
    /// EE(Zenoh):到 hex-controller 末端执行器的连接(机器人控制台共用其全量发现,至多一条)。
    pub zenoh_ee: Mutex<Option<crate::zenoh_ee::ZenohEeConn>>,
    /// The running SmartKnob Robot Application, if started. At most one at a
    /// time (it owns the high-rate haptic loop on the single bus).
    pub smartknob: Mutex<Option<ActiveSmartKnob>>,
    /// The running IMU session, if started. At most one at a time; it streams
    /// the selected IMU's TPDO1 and publishes a snapshot for the UI to poll.
    pub imu: Mutex<Option<crate::imu::ImuManager>>,
    /// Direct DAMIAO protocol sessions keyed by motor CAN ID. All sessions
    /// borrow the same manager-owned CAN bus, so one adapter can control
    /// several DM-J4310-2EC V1.1 motors independently.
    pub damiao: Mutex<HashMap<u16, Arc<crate::damiao::DamiaoSession>>>,
    /// Lazy raw-CAN discovery monitor for the dedicated DAMIAO workspace.
    /// It scans the protocol's unambiguous 4-bit feedback ID space and is
    /// stopped together with the physical CAN connection.
    pub damiao_discovery: Mutex<Option<Arc<crate::damiao::DamiaoDiscovery>>>,
    /// Stock-firmware Unit RollerCAN control workspace. This stays separate
    /// from `rollercan`, which belongs to the independent SmartKnob firmware.
    /// It is created lazily and borrows the manager-owned CAN bus.
    pub rollercan_control: Mutex<Option<Arc<crate::rollercan_control::RollerCanControl>>>,
    /// The running CAN analyzer session, if started. Owns its *own* bus (opened
    /// directly, no `Cia402Manager`), so it is stopped unconditionally on
    /// `disconnect` / tool switch, independent of `manager`.
    pub analyzer: Mutex<Option<crate::analyzer::CanAnalyzer>>,
    /// Unit RollerCAN protocol monitor attached to the manager-owned `CanBus`.
    /// It does not open or own a second physical adapter in the product path.
    pub rollercan: Mutex<Option<crate::rollercan::RollerCanSession>>,
}

impl AppState {
    /// Convenience: clone the current manager Arc out of the mutex, or
    /// return `None` if not connected. The mutex is released before the
    /// caller awaits.
    pub async fn manager(&self) -> Option<Arc<Cia402Manager>> {
        self.manager.lock().await.clone()
    }

    /// Take a log handle out of the map (for stopping), if present.
    pub fn take_log(&self, nid: u8) -> Option<LogHandle> {
        self.logs.lock().unwrap().remove(&nid)
    }

    /// Drain all log handles (used on disconnect).
    pub fn drain_logs(&self) -> Vec<LogHandle> {
        self.logs.lock().unwrap().drain().map(|(_, h)| h).collect()
    }
}
