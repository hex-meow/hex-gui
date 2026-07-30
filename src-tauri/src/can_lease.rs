//! Process-wide ownership gate for physical CAN adapters.
//!
//! Every runtime path that opens a CAN transport acquires this gate before it
//! awaits the backend open.  The returned RAII permit represents `Opening`
//! until the caller marks it held; dropping it on an error, cancellation, or a
//! normal disconnect releases the gate.

use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use can_transport::{
    CanBus, CanBusState, CanCapabilities, CanFilter, CanFrame, CanIoError, CanLinkConfig, CanRx,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanOwner {
    Manager,
    Analyzer,
    Stm32Discovery,
    Stm32Dfu,
}

impl fmt::Display for CanOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Manager => "the normal CAN session",
            Self::Analyzer => "CAN Analyzer",
            Self::Stm32Discovery => "STM32 CAN discovery",
            Self::Stm32Dfu => "STM32 CAN update",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeasePhase {
    Opening,
    Held,
}

impl fmt::Display for LeasePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Opening => "opening",
            Self::Held => "active",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveLease {
    owner: CanOwner,
    phase: LeasePhase,
    generation: u64,
}

#[derive(Debug, Default)]
struct LeaseState {
    generation: u64,
    active: Option<ActiveLease>,
}

#[derive(Debug, Default)]
pub struct CanTransportGate {
    inner: Arc<Mutex<LeaseState>>,
}

impl CanTransportGate {
    pub fn try_acquire(&self, owner: CanOwner) -> Result<CanLease, String> {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(active) = state.active {
            return Err(format!(
                "cannot open {owner}: {} is already {}",
                active.owner, active.phase
            ));
        }
        state.generation = state.generation.wrapping_add(1);
        if state.generation == 0 {
            state.generation = 1;
        }
        let generation = state.generation;
        state.active = Some(ActiveLease {
            owner,
            phase: LeasePhase::Opening,
            generation,
        });
        Ok(CanLease {
            inner: self.inner.clone(),
            owner,
            generation,
        })
    }

    #[cfg(test)]
    fn active(&self) -> Option<(CanOwner, &'static str)> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active
            .map(|active| {
                (
                    active.owner,
                    match active.phase {
                        LeasePhase::Opening => "opening",
                        LeasePhase::Held => "held",
                    },
                )
            })
    }
}

#[derive(Debug)]
pub struct CanLease {
    inner: Arc<Mutex<LeaseState>>,
    owner: CanOwner,
    generation: u64,
}

impl CanLease {
    fn mark_held(&mut self) -> Result<(), String> {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        match state.active.as_mut() {
            Some(active) if active.owner == self.owner && active.generation == self.generation => {
                active.phase = LeasePhase::Held;
                Ok(())
            }
            _ => Err(format!(
                "the process-wide CAN lease for {} became stale",
                self.owner
            )),
        }
    }
}

impl Drop for CanLease {
    fn drop(&mut self) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if state.active.is_some_and(|active| {
            active.owner == self.owner && active.generation == self.generation
        }) {
            state.active = None;
        }
    }
}

/// Attach a successfully opened bus to its process-wide ownership permit.
///
/// The permit lives inside the decorator, so commands that cloned the bus
/// before a disconnect keep excluding a competing owner until their last
/// `Arc<dyn CanBus>` is gone.
pub fn hold_open_bus(
    inner: Arc<dyn CanBus>,
    mut lease: CanLease,
) -> Result<Arc<dyn CanBus>, String> {
    lease.mark_held()?;
    let anchor = Arc::new(LeaseAnchor { _lease: lease });
    Ok(Arc::new(LeasedCanBus { inner, anchor }))
}

struct LeaseAnchor {
    _lease: CanLease,
}

struct LeasedCanBus {
    // Keep the raw bus ahead of the anchor so backend handles close before the
    // ownership token is released when the last holder is dropped.
    inner: Arc<dyn CanBus>,
    anchor: Arc<LeaseAnchor>,
}

#[async_trait]
impl CanBus for LeasedCanBus {
    async fn send(&self, frame: CanFrame) -> Result<(), CanIoError> {
        self.inner.send(frame).await
    }

    async fn subscribe(&self, filter: CanFilter) -> Result<Box<dyn CanRx>, CanIoError> {
        let inner = self.inner.subscribe(filter).await?;
        Ok(Box::new(LeasedCanRx {
            inner,
            _anchor: self.anchor.clone(),
        }))
    }

    fn capabilities(&self) -> CanCapabilities {
        self.inner.capabilities()
    }

    async fn bus_state(&self) -> Result<Option<CanBusState>, CanIoError> {
        self.inner.bus_state().await
    }

    async fn link_config(&self) -> Result<Option<CanLinkConfig>, CanIoError> {
        self.inner.link_config().await
    }
}

struct LeasedCanRx {
    // Drop the real subscription before releasing the final lease anchor.
    inner: Box<dyn CanRx>,
    _anchor: Arc<LeaseAnchor>,
}

#[async_trait]
impl CanRx for LeasedCanRx {
    async fn recv(&mut self) -> Result<CanFrame, CanIoError> {
        self.inner.recv().await
    }

    fn try_recv(&mut self) -> Result<Option<CanFrame>, CanIoError> {
        self.inner.try_recv()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    struct TestBus;
    struct TestRx;

    #[async_trait]
    impl CanBus for TestBus {
        async fn send(&self, _frame: CanFrame) -> Result<(), CanIoError> {
            Ok(())
        }

        async fn subscribe(&self, _filter: CanFilter) -> Result<Box<dyn CanRx>, CanIoError> {
            Ok(Box::new(TestRx))
        }

        fn capabilities(&self) -> CanCapabilities {
            CanCapabilities {
                fd: false,
                max_dlen: 8,
            }
        }

        async fn link_config(&self) -> Result<Option<CanLinkConfig>, CanIoError> {
            Ok(Some(CanLinkConfig {
                fd_enabled: Some(false),
                nominal: None,
                data: None,
            }))
        }
    }

    #[async_trait]
    impl CanRx for TestRx {
        async fn recv(&mut self) -> Result<CanFrame, CanIoError> {
            Err(CanIoError::Disconnected)
        }

        fn try_recv(&mut self) -> Result<Option<CanFrame>, CanIoError> {
            Ok(None)
        }
    }

    #[test]
    fn opening_is_atomic_and_drop_releases() {
        let gate = CanTransportGate::default();
        let opening = gate.try_acquire(CanOwner::Manager).unwrap();
        assert_eq!(gate.active(), Some((CanOwner::Manager, "opening")));
        assert!(gate.try_acquire(CanOwner::Analyzer).is_err());

        drop(opening);
        let _next = gate.try_acquire(CanOwner::Analyzer).unwrap();
        assert_eq!(gate.active(), Some((CanOwner::Analyzer, "opening")));
    }

    #[test]
    fn failed_acquire_reports_and_does_not_release_the_owner() {
        let gate = CanTransportGate::default();
        let _opening = gate.try_acquire(CanOwner::Manager).unwrap();
        let error = gate.try_acquire(CanOwner::Analyzer).unwrap_err();
        assert!(error.contains("normal CAN session"));
        assert!(error.contains("opening"));
        assert_eq!(gate.active(), Some((CanOwner::Manager, "opening")));
    }

    #[test]
    fn simultaneous_acquire_has_exactly_one_winner() {
        let gate = Arc::new(CanTransportGate::default());
        let start = Arc::new(Barrier::new(3));
        let finish = Arc::new(Barrier::new(2));
        let workers = [CanOwner::Manager, CanOwner::Analyzer].map(|owner| {
            let gate = gate.clone();
            let start = start.clone();
            let finish = finish.clone();
            thread::spawn(move || {
                start.wait();
                let permit = gate.try_acquire(owner).ok();
                finish.wait();
                permit.is_some()
            })
        });
        start.wait();
        let won = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(won, 1);
    }

    #[test]
    fn held_permit_excludes_every_other_owner_until_drop() {
        let gate = CanTransportGate::default();
        let mut permit = gate.try_acquire(CanOwner::Stm32Dfu).unwrap();
        permit.mark_held().unwrap();
        assert_eq!(gate.active(), Some((CanOwner::Stm32Dfu, "held")));
        assert!(gate.try_acquire(CanOwner::Manager).is_err());
        assert!(gate.try_acquire(CanOwner::Analyzer).is_err());

        drop(permit);
        assert_eq!(gate.active(), None);
    }

    #[test]
    fn bus_clones_retain_the_lease_after_session_drop() {
        let gate = CanTransportGate::default();
        let permit = gate.try_acquire(CanOwner::Manager).unwrap();
        let bus = hold_open_bus(Arc::new(TestBus), permit).unwrap();
        let in_flight = bus.clone();
        drop(bus);

        assert_eq!(gate.active(), Some((CanOwner::Manager, "held")));
        assert!(gate.try_acquire(CanOwner::Stm32Discovery).is_err());
        drop(in_flight);
        assert_eq!(gate.active(), None);
    }

    #[tokio::test]
    async fn receive_subscription_retains_the_lease_after_bus_drop() {
        let gate = CanTransportGate::default();
        let permit = gate.try_acquire(CanOwner::Analyzer).unwrap();
        let bus = hold_open_bus(Arc::new(TestBus), permit).unwrap();
        assert_eq!(
            bus.capabilities(),
            CanCapabilities {
                fd: false,
                max_dlen: 8,
            }
        );
        assert_eq!(bus.bus_state().await.unwrap(), None);
        assert_eq!(
            bus.link_config().await.unwrap(),
            Some(CanLinkConfig {
                fd_enabled: Some(false),
                nominal: None,
                data: None,
            })
        );
        let rx = bus.subscribe(CanFilter::pass_all_standard()).await.unwrap();
        drop(bus);

        assert_eq!(gate.active(), Some((CanOwner::Analyzer, "held")));
        assert!(gate.try_acquire(CanOwner::Manager).is_err());
        drop(rx);
        assert_eq!(gate.active(), None);
    }
}
