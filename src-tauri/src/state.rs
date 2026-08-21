//! Tauri-managed app state.
//!
//! Holds at most one [`Cia402Manager`] at a time (one CAN bus per app
//! lifetime). Commands clone its `Arc` and release the manager mutex before
//! awaiting motor I/O. Persistent settings/position operations are the narrow
//! exception: they share a separate gate with disconnect.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use can_transport::CanBus;
use hex_motor::cia402::Cia402Manager;
use hex_motor::meow_motor::MeowMotorManager;
use tokio::sync::{Mutex, MutexGuard};

use crate::hopea3::{Hopea3, InitProgress};
use crate::lift::LiftSession;
use crate::logging::LogHandle;
use crate::unified_smartknob::ActiveSmartKnob;

/// Serializes persistent communication settings and motor position operations
/// with disconnect. The counter includes both the current holder and queued
/// callers, so a window close cannot slip past a command waiting for the lock.
#[derive(Default)]
pub(crate) struct DeviceSettingsOperationGate {
    mutex: Mutex<()>,
    pending_or_active: AtomicUsize,
}

impl DeviceSettingsOperationGate {
    pub(crate) async fn acquire(&self) -> DeviceSettingsOperationGuard<'_> {
        self.pending_or_active.fetch_add(1, Ordering::AcqRel);
        // This token is created before awaiting the mutex. If the command
        // future is cancelled while queued, Drop repairs the active count.
        let count = OperationCount {
            counter: &self.pending_or_active,
        };
        let lock = self.mutex.lock().await;
        DeviceSettingsOperationGuard {
            _lock: lock,
            _count: count,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.pending_or_active.load(Ordering::Acquire) != 0
    }
}

struct OperationCount<'a> {
    counter: &'a AtomicUsize,
}

impl Drop for OperationCount<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct DeviceSettingsOperationGuard<'a> {
    // Field order matters: release the mutex before the active-count token.
    _lock: MutexGuard<'a, ()>,
    _count: OperationCount<'a>,
}

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
    /// Independent manager for the new protocol, sharing the same CAN transport. It never
    /// broadcasts a second host heartbeat and performs identification only on explicit GUI use.
    pub meow_manager: Mutex<Option<Arc<MeowMotorManager>>>,
    /// Shared transport retained for the calibration worker's strictly
    /// serialized raw SDO snapshot/restore of 0x1016 and runtime limits.
    pub calibration_bus: Mutex<Option<Arc<dyn CanBus>>>,
    /// Host NID selected in the connection bar. Calibration workers use it
    /// for operation-scoped heartbeat traffic and stop it before becoming
    /// ready for motor hot-swap.
    pub calibration_host_node_id: Mutex<Option<u8>>,
    /// Read-only source-proof snapshots keyed by `(node-id, heartbeat session
    /// epoch)`. Public registration re-reads the live device before use.
    pub authenticity: crate::authenticity::AuthenticityState,
    /// Factory torque factors (`0x4001` v1) for the Motor Control App, keyed by
    /// `(node-id, heartbeat session epoch)`. Read once per session; the snapshot
    /// poll only ever reads this cache and never touches the bus.
    pub meow_calibration: crate::meow_calibration::MeowCalibrationState,
    /// Developer-only attended user recalibration. Unlike the transient proof
    /// cache, this remains in memory across a CAN disconnect so the operator
    /// can power-cycle/reconnect and verify the persisted readback.
    pub calibration_update: crate::calibration_update::CalibrationUpdateState,
    /// Serializes the cross-tool check/start transition so friction and torque
    /// calibration cannot race into concurrent ownership of one motor bus.
    pub calibration_start_gate: Mutex<()>,
    /// Developer-only unloaded friction calibration. Its Rust task owns the
    /// complete bounded motion sequence and safe cleanup.
    pub friction_calibration: crate::friction_calibration::FrictionCalibrationState,
    pub torque_calibration: crate::torque_calibration::TorqueCalibrationState,
    /// Global settings/position/disconnect lock. Commands always acquire this
    /// before cloning or locking the manager to keep lock ordering acyclic.
    pub(crate) device_settings_operation: DeviceSettingsOperationGate,
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
    pub zenoh_lift: Mutex<Option<crate::zenoh_lift::ZenohLiftConn>>,
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

    pub async fn meow_manager(&self) -> Option<Arc<MeowMotorManager>> {
        self.meow_manager.lock().await.clone()
    }

    pub async fn calibration_bus(&self) -> Option<Arc<dyn CanBus>> {
        self.calibration_bus.lock().await.clone()
    }

    pub async fn calibration_host_node_id(&self) -> Option<u8> {
        *self.calibration_host_node_id.lock().await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn settings_gate_is_active_for_holder_until_drop() {
        let gate = DeviceSettingsOperationGate::default();
        assert!(!gate.is_active());

        let guard = gate.acquire().await;
        assert!(gate.is_active());
        assert_eq!(gate.pending_or_active.load(Ordering::Acquire), 1);

        drop(guard);
        assert!(!gate.is_active());
        assert_eq!(gate.pending_or_active.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn settings_gate_counts_waiter_and_acquires_strictly_in_order() {
        let gate = Arc::new(DeviceSettingsOperationGate::default());
        let first = gate.acquire().await;
        let (acquired_tx, mut acquired_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();

        let waiter_gate = Arc::clone(&gate);
        let waiter = tokio::spawn(async move {
            let _second = waiter_gate.acquire().await;
            let _ = acquired_tx.send(());
            let _ = release_rx.await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while gate.pending_or_active.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued operation was not counted");
        assert!(gate.is_active());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut acquired_rx)
                .await
                .is_err(),
            "the queued operation acquired before the first guard was dropped"
        );

        drop(first);
        tokio::time::timeout(Duration::from_secs(1), &mut acquired_rx)
            .await
            .expect("queued operation did not acquire after release")
            .expect("queued operation exited before reporting acquisition");
        assert!(gate.is_active());
        assert_eq!(gate.pending_or_active.load(Ordering::Acquire), 1);

        let _ = release_tx.send(());
        waiter.await.expect("waiter task panicked");
        assert!(!gate.is_active());
        assert_eq!(gate.pending_or_active.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn cancelling_queued_settings_operation_repairs_active_count() {
        let gate = Arc::new(DeviceSettingsOperationGate::default());
        let first = gate.acquire().await;
        let waiter_gate = Arc::clone(&gate);
        let waiter = tokio::spawn(async move {
            let _second = waiter_gate.acquire().await;
            std::future::pending::<()>().await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while gate.pending_or_active.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued operation was not counted");

        waiter.abort();
        assert!(waiter
            .await
            .expect_err("aborted waiter unexpectedly completed")
            .is_cancelled());
        assert!(gate.is_active());
        assert_eq!(gate.pending_or_active.load(Ordering::Acquire), 1);

        drop(first);
        assert!(!gate.is_active());
        assert_eq!(gate.pending_or_active.load(Ordering::Acquire), 0);
    }
}
