//! Traffic owned only for the lifetime of a developer calibration operation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use can_transport::CanBus;
use hex_motor::canopen::heartbeat::build_heartbeat_frame;
use hex_motor::canopen::nmt::NmtState;
use tokio::task::JoinHandle;

const HEARTBEAT_PERIOD: Duration = Duration::from_millis(50);

pub(crate) struct CalibrationHeartbeat {
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl CalibrationHeartbeat {
    pub(crate) fn start(bus: Arc<dyn CanBus>, host_node_id: u8) -> Result<Self, String> {
        if !(1..=127).contains(&host_node_id) {
            return Err(format!(
                "calibration host node ID must be 1..=127, got {host_node_id}"
            ));
        }
        let frame = build_heartbeat_frame(host_node_id, NmtState::Operational)
            .map_err(|error| error.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let task_stop = stop.clone();
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(HEARTBEAT_PERIOD);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while !task_stop.load(Ordering::Acquire) {
                ticker.tick().await;
                if task_stop.load(Ordering::Acquire) {
                    break;
                }
                if let Err(error) = bus.send(frame.clone()).await {
                    log::warn!("calibration heartbeat send failed: {error}");
                }
            }
        });
        Ok(Self {
            stop,
            task: Some(task),
        })
    }

    pub(crate) async fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for CalibrationHeartbeat {
    fn drop(&mut self) {
        // A panic/abort in the owning calibration task must not leave an
        // orphan producer heartbeat keeping the last motor target alive.
        self.stop.store(true, Ordering::Release);
    }
}
