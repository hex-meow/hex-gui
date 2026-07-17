//! CAN backend factory.
//!
//! Adding a backend is a single arm in [`open_bus`]; the rest of the app
//! keeps holding an `Arc<dyn CanBus>` and never knows the difference.
//!
//! Spec format is `"<backend>:<name>"`, falling back to bare `<name>` which
//! is treated as `socketcan:<name>` on Linux. gs_usb adapters use a
//! `gs_usb<channel>` spec. Examples:
//! - `"can0"` (Linux SocketCAN, default)
//! - `"socketcan:vcan0"`
//! - `"gs_usb"` / `"gs_usb0"` — first gs_usb adapter, channel 0
//! - `"gs_usb1"` — channel 1 of a multi-channel gs_usb adapter
//!   (candleLight over USB, CAN-FD; works on Linux/macOS/Windows)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{error::Error as StdError, fmt};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use can_transport::{CanBus, CanBusState, CanCapabilities, CanFilter, CanFrame, CanIoError, CanRx};
use tokio::sync::{mpsc, oneshot};

/// A raw backend send is allowed to occupy the one submission slot for at
/// most this long. Once it times out, the wrapper is permanently poisoned: a
/// late USB completion can no longer be confused with a later submission.
const SEND_TIMEOUT: Duration = Duration::from_millis(750);

struct SendRequest {
    frame: CanFrame,
    reply: oneshot::Sender<Result<(), CanIoError>>,
}

#[derive(Debug)]
struct CanSendTimeout(Duration);

impl fmt::Display for CanSendTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CAN send timed out after {} ms; disconnect and reconnect the adapter before sending again",
            self.0.as_millis()
        )
    }
}

impl StdError for CanSendTimeout {}

/// Cancellation-safe serialization boundary for all CAN transmissions.
///
/// Calling a backend's async `send` directly ties the USB submission future to
/// the caller. If that caller is cancelled, a backend completion may remain in
/// flight and be consumed by a later call. Here the caller only awaits a
/// one-shot reply; the independent worker owns every raw send until it either
/// completes or reaches the hard timeout.
struct ProtectedCanBus {
    inner: Arc<dyn CanBus>,
    send_tx: mpsc::UnboundedSender<SendRequest>,
    poisoned: Arc<AtomicBool>,
}

impl ProtectedCanBus {
    fn new(inner: Arc<dyn CanBus>, send_timeout: Duration) -> Arc<Self> {
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let poisoned = Arc::new(AtomicBool::new(false));
        tokio::spawn(send_worker(
            inner.clone(),
            send_rx,
            poisoned.clone(),
            send_timeout,
        ));
        Arc::new(Self {
            inner,
            send_tx,
            poisoned,
        })
    }
}

async fn send_worker(
    inner: Arc<dyn CanBus>,
    mut requests: mpsc::UnboundedReceiver<SendRequest>,
    poisoned: Arc<AtomicBool>,
    send_timeout: Duration,
) {
    while let Some(request) = requests.recv().await {
        // A caller cancelled while this request was still queued, so no raw
        // transfer has started and the frame is safe to discard. Once the
        // worker passes this boundary it deliberately owns the raw send to
        // completion/timeout even if the caller goes away later.
        if request.reply.is_closed() {
            continue;
        }
        if poisoned.load(Ordering::Acquire) {
            let _ = request.reply.send(Err(CanIoError::Disconnected));
            continue;
        }

        match tokio::time::timeout(send_timeout, inner.send(request.frame)).await {
            Ok(result) => {
                // The worker owns the raw send even if the receiving caller was
                // cancelled. A closed reply channel is therefore harmless.
                let _ = request.reply.send(result);
            }
            Err(_) => {
                poisoned.store(true, Ordering::Release);
                log::error!(
                    "CAN send timed out after {} ms; bus poisoned to prevent late-completion reuse; disconnect and reconnect the adapter",
                    send_timeout.as_millis()
                );
                let _ = request
                    .reply
                    .send(Err(CanIoError::backend(CanSendTimeout(send_timeout))));
            }
        }
    }
}

#[async_trait]
impl CanBus for ProtectedCanBus {
    async fn send(&self, frame: CanFrame) -> Result<(), CanIoError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(CanIoError::Disconnected);
        }

        let (reply, result) = oneshot::channel();
        self.send_tx
            .send(SendRequest { frame, reply })
            .map_err(|_| CanIoError::Disconnected)?;
        result.await.unwrap_or(Err(CanIoError::Disconnected))
    }

    async fn subscribe(&self, filter: CanFilter) -> Result<Box<dyn CanRx>, CanIoError> {
        self.inner.subscribe(filter).await
    }

    fn capabilities(&self) -> CanCapabilities {
        self.inner.capabilities()
    }

    async fn bus_state(&self) -> Result<Option<CanBusState>, CanIoError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(CanIoError::Disconnected);
        }
        self.inner.bus_state().await
    }
}

fn protect_bus(inner: Arc<dyn CanBus>) -> Arc<dyn CanBus> {
    ProtectedCanBus::new(inner, SEND_TIMEOUT)
}

/// Open a bus. `hw_timestamp` asks the backend to stamp received frames with
/// its hardware clock (gs_usb only, needs firmware support); the returned bool
/// reports whether that actually engaged.
pub async fn open_bus(spec: &str, hw_timestamp: bool) -> Result<(Arc<dyn CanBus>, bool)> {
    // gs_usb is cross-platform and selected by a `gs_usb<channel>` spec.
    if let Some(channel) = gs_usb_channel(spec) {
        use can_transport::gs_usb::{GsUsbBus, GsUsbConfig};
        // CAN-FD, 1 Mbit nominal / 5 Mbit data (80 MHz device clock).
        let bus = GsUsbBus::open(
            GsUsbConfig::fd_1m_5m()
                .with_channel(channel)
                .with_hw_timestamp(hw_timestamp),
        )
        .await
        .with_context(|| format!("opening gs_usb / candleLight channel {channel}"))?;
        let hw_ts = bus.hw_timestamps_active();
        log::info!(
            "gs_usb ch{channel} opened: {:?}, hw_ts={hw_ts}",
            bus.capabilities()
        );
        return Ok((protect_bus(Arc::new(bus)), hw_ts));
    }

    let (kind, _name) = match spec.split_once(':') {
        Some((k, n)) => (k, n),
        None => ("socketcan", spec),
    };
    match kind {
        #[cfg(target_os = "linux")]
        "socketcan" => {
            let bus = can_transport::socketcan::SocketCanBus::open(_name)
                .with_context(|| format!("opening SocketCAN interface '{_name}'"))?;
            // SocketCAN hardware timestamps would need SO_TIMESTAMPING,
            // which can-transport does not expose yet.
            Ok((protect_bus(Arc::new(bus)), false))
        }
        other => bail!(
            "backend '{other}' is not available on this build \
             (known: 'socketcan' on Linux, 'gs_usb<channel>' everywhere)"
        ),
    }
}

/// Parse a gs_usb interface spec into a channel number, or `None` if `spec`
/// is not a gs_usb spec. Accepts `gs_usb`, `gs_usb0`, `gs_usb1`, `gs_usb:1`,
/// and the underscore-less `gsusb2` variants.
fn gs_usb_channel(spec: &str) -> Option<u16> {
    let s = spec.trim().to_ascii_lowercase();
    let rest = s
        .strip_prefix("gs_usb")
        .or_else(|| s.strip_prefix("gsusb"))?;
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    if rest.is_empty() {
        Some(0)
    } else {
        rest.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::Semaphore;

    use super::*;

    fn frame(id: u16) -> CanFrame {
        CanFrame::new_data(id, &[id as u8]).unwrap()
    }

    struct HangingBus {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CanBus for HangingBus {
        async fn send(&self, _frame: CanFrame) -> Result<(), CanIoError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            pending().await
        }

        async fn subscribe(&self, _filter: CanFilter) -> Result<Box<dyn CanRx>, CanIoError> {
            Err(CanIoError::Disconnected)
        }

        fn capabilities(&self) -> CanCapabilities {
            CanCapabilities {
                fd: true,
                max_dlen: 64,
            }
        }
    }

    #[tokio::test]
    async fn timeout_poisons_bus_and_never_submits_again() {
        let inner = Arc::new(HangingBus {
            calls: AtomicUsize::new(0),
        });
        let bus = ProtectedCanBus::new(inner.clone(), Duration::from_millis(25));

        assert!(matches!(
            bus.send(frame(0x101)).await,
            Err(CanIoError::Backend(_))
        ));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);

        let second = tokio::time::timeout(Duration::from_millis(100), bus.send(frame(0x102)))
            .await
            .expect("a poisoned bus must reject immediately");
        assert!(matches!(second, Err(CanIoError::Disconnected)));
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "poisoned bus submitted a second raw send"
        );
        assert!(matches!(
            bus.bus_state().await,
            Err(CanIoError::Disconnected)
        ));
    }

    struct ControlledBus {
        started_tx: mpsc::UnboundedSender<u32>,
        release: Arc<Semaphore>,
    }

    #[async_trait::async_trait]
    impl CanBus for ControlledBus {
        async fn send(&self, frame: CanFrame) -> Result<(), CanIoError> {
            let _ = self.started_tx.send(frame.id().raw());
            let permit = self
                .release
                .acquire()
                .await
                .map_err(|_| CanIoError::Disconnected)?;
            permit.forget();
            Ok(())
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

    #[tokio::test]
    async fn cancelling_caller_does_not_cancel_raw_send_or_reorder_next_send() {
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let inner = Arc::new(ControlledBus {
            started_tx,
            release: release.clone(),
        });
        let bus = ProtectedCanBus::new(inner, SEND_TIMEOUT);

        let first_bus = bus.clone();
        let first = tokio::spawn(async move { first_bus.send(frame(0x201)).await });
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .unwrap(),
            Some(0x201)
        );

        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let second_bus = bus.clone();
        let second = tokio::spawn(async move { second_bus.send(frame(0x202)).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), started_rx.recv())
                .await
                .is_err(),
            "second raw send started while the cancelled caller's send was in flight"
        );

        release.add_permits(1);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .unwrap(),
            Some(0x202)
        );
        release.add_permits(1);
        assert!(second.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn cancelled_queued_send_is_discarded_before_next_live_send() {
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let inner = Arc::new(ControlledBus {
            started_tx,
            release: release.clone(),
        });
        let bus = ProtectedCanBus::new(inner, SEND_TIMEOUT);

        let first_bus = bus.clone();
        let first = tokio::spawn(async move { first_bus.send(frame(0x301)).await });
        assert_eq!(started_rx.recv().await, Some(0x301));

        // Enqueue a request behind the in-flight frame, then model its caller
        // being cancelled before the worker has started the raw send.
        let (stale_reply, stale_result) = oneshot::channel();
        bus.send_tx
            .send(SendRequest {
                frame: frame(0x302),
                reply: stale_reply,
            })
            .unwrap();
        drop(stale_result);

        let live_bus = bus.clone();
        let live = tokio::spawn(async move { live_bus.send(frame(0x303)).await });
        release.add_permits(1);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), started_rx.recv())
                .await
                .unwrap(),
            Some(0x303),
            "cancelled queued frame was submitted before the live safety command"
        );
        release.add_permits(1);
        assert!(first.await.unwrap().is_ok());
        assert!(live.await.unwrap().is_ok());
    }
}
