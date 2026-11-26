use super::{
    gripper::GripperMotion,
    model::{Limits, Q},
    runtime::{ArrivalTolerance, Control, FactoryReading, Gains, GravityScale},
};
use std::future::Future;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot};

pub(super) type Reply<T = ()> = oneshot::Sender<Result<T, String>>;
pub(super) enum Command {
    Activate(Control, Gains, Reply),
    SetGravityScale(GravityScale, Reply),
    SetGoalTolerance(ArrivalTolerance, Reply),
    Move(Vec<f64>, Limits, Reply),
    MoveGripper(GripperMotion, Reply),
    DisableGripper(Reply),
    Stop(Limits, Reply),
    Release(Reply),
    Offsets(Q, Reply),
    ReadFactory(u8, Reply<FactoryReading>),
    Close(Reply),
}

#[derive(Clone)]
pub(super) struct Sender {
    normal: mpsc::Sender<(u64, Command)>,
    priority: mpsc::UnboundedSender<Command>,
    epoch: Arc<AtomicU64>,
}

pub(super) struct Inbox {
    normal: mpsc::Receiver<(u64, Command)>,
    priority: mpsc::UnboundedReceiver<Command>,
    epoch: Arc<AtomicU64>,
}

pub(super) fn channel() -> (Sender, Inbox) {
    let (normal_tx, normal) = mpsc::channel(8);
    let (priority_tx, priority) = mpsc::unbounded_channel();
    let epoch = Arc::new(AtomicU64::new(0));
    (
        Sender {
            normal: normal_tx,
            priority: priority_tx,
            epoch: epoch.clone(),
        },
        Inbox {
            normal,
            priority,
            epoch,
        },
    )
}

impl Sender {
    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub(super) fn same_session(&self, other: &Self) -> bool {
        self.normal.same_channel(&other.normal)
    }

    // Enqueue before returning the future. Workspace guards only protect submission.
    pub(super) fn request<T>(
        &self,
        make: impl FnOnce(Reply<T>) -> Command,
    ) -> impl Future<Output = Result<T, String>> {
        self.request_at(self.epoch(), make)
    }

    pub(super) fn request_at<T>(
        &self,
        epoch: u64,
        make: impl FnOnce(Reply<T>) -> Command,
    ) -> impl Future<Output = Result<T, String>> {
        let (reply, result) = oneshot::channel();
        let cmd = make(reply);
        let sent = if matches!(cmd, Command::Stop(..) | Command::Release(_)) {
            self.epoch.fetch_add(1, Ordering::SeqCst);
            self.priority.send(cmd).map_err(|_| "Arm session ended")
        } else {
            self.normal.try_send((epoch, cmd)).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => "Arm command queue is busy",
                mpsc::error::TrySendError::Closed(_) => "Arm session ended",
            })
        };
        async move {
            sent?;
            result.await.map_err(|_| "Arm session ended")?
        }
    }
}

impl Inbox {
    pub(super) fn invalidate_motion(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) async fn recv(&mut self) -> Option<Command> {
        loop {
            let (epoch, cmd) = tokio::select! {
                biased;
                urgent = self.priority.recv() => return urgent,
                queued = self.normal.recv() => queued?,
            };
            if self.epoch.load(Ordering::SeqCst) != epoch {
                match cmd {
                    Command::Activate(_, _, reply)
                    | Command::Move(_, _, reply)
                    | Command::MoveGripper(_, reply) => {
                        let _ = reply.send(Err("Motion superseded by brake or release".into()));
                        continue;
                    }
                    _ => {}
                }
            }
            return Some(cmd);
        }
    }

    // The driver services arm ticks during gripper SDO transactions and supports
    // cancelling its future. A target already accepted by the gripper may still run.
    pub(super) async fn gripper(
        &mut self,
        operation: impl Future<Output = hex_arm::Result<()>>,
        reply: Reply,
    ) -> Option<Command> {
        tokio::select! {
            biased;
            urgent = self.priority.recv() => {
                let _ = reply.send(Err("Gripper request interrupted by brake or release. Check measured gripper state.".into()));
                urgent
            }
            result = operation => {
                let _ = reply.send(result.map_err(|e| e.to_string()));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn brake_and_release_interrupt_a_pending_gripper_and_supersede_queued_moves() {
        for release in [false, true] {
            let (sender, mut inbox) = channel();
            let epoch = sender.epoch();
            let limits: Limits =
                serde_json::from_str(r#"{"velocity":0.5,"acceleration":1,"jerk":4}"#).unwrap();
            let stale = sender.request(|r| Command::Move(vec![0.; 6], limits, r));
            let (reply, result) = oneshot::channel();
            let (started, running) = oneshot::channel();
            let worker = tokio::spawn(async move {
                let gripper = async {
                    started.send(()).unwrap();
                    std::future::pending().await
                };
                let urgent = inbox.gripper(gripper, reply).await.unwrap();
                match urgent {
                    Command::Release(r) if release => {
                        inbox.invalidate_motion();
                        r.send(Ok(())).unwrap();
                    }
                    Command::Stop(_, r) if !release => r.send(Ok(())).unwrap(),
                    _ => panic!("expected the submitted priority command"),
                }
                assert!(matches!(inbox.recv().await, Some(Command::Close(_))));
            });
            running.await.unwrap();
            if release {
                sender.request(Command::Release).await.unwrap();
            } else {
                sender.request(|r| Command::Stop(limits, r)).await.unwrap();
            }
            assert!(result.await.unwrap().unwrap_err().contains("interrupted"));
            // Simulate the continuation of an older execute still awaiting enable confirmation.
            let late = sender.request_at(epoch, |r| Command::Move(vec![1.; 6], limits, r));
            let _close = sender.request(Command::Close);
            worker.await.unwrap();
            assert!(stale.await.unwrap_err().contains("superseded"));
            assert!(late.await.unwrap_err().contains("superseded"));
        }
    }
}
