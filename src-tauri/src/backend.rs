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

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
#[cfg(target_os = "linux")]
use can_transport::CanControllerState;
use can_transport::{
    CanBus, CanBusState, CanCapabilities, CanFilter, CanFrame, CanId, CanIoError, CanRx,
};

const TSDO_BASE: u32 = 0x580;
const TSDO_FAMILY_MASK: u32 = 0x780;

/// Tighten canopen-sdo's family-wide TSDO subscription to the node encoded in
/// the filter id. The dependency currently asks for mask 0x780 even though its
/// id field still contains the exact `0x580 + node-id` COB-ID.
fn exact_tsdo_filter(filter: CanFilter) -> Option<(CanFilter, u16)> {
    if filter.extended || filter.mask != TSDO_FAMILY_MASK {
        return None;
    }
    if !(TSDO_BASE + 1..=TSDO_BASE + 0x7F).contains(&filter.id) {
        return None;
    }
    let expected = filter.id as u16;
    Some((CanFilter::exact_standard(expected), expected))
}

/// App-wide CAN decorator that gives every SDO transaction a node-exact RX
/// queue. The validating receiver is deliberately retained after narrowing:
/// if a backend ever violates its filter contract, the unexpected frame is
/// visible in normal application logs instead of being silently ignored by
/// the SDO state machine.
struct ExactSdoBus {
    inner: Arc<dyn CanBus>,
}

#[async_trait]
impl CanBus for ExactSdoBus {
    async fn send(&self, frame: CanFrame) -> std::result::Result<(), CanIoError> {
        self.inner.send(frame).await
    }

    async fn subscribe(
        &self,
        filter: CanFilter,
    ) -> std::result::Result<Box<dyn CanRx>, CanIoError> {
        let Some((exact, expected)) = exact_tsdo_filter(filter) else {
            return self.inner.subscribe(filter).await;
        };
        let inner = self.inner.subscribe(exact).await?;
        Ok(Box::new(ValidatedSdoRx { inner, expected }))
    }

    fn capabilities(&self) -> CanCapabilities {
        self.inner.capabilities()
    }

    async fn bus_state(&self) -> std::result::Result<Option<CanBusState>, CanIoError> {
        self.inner.bus_state().await
    }
}

struct ValidatedSdoRx {
    inner: Box<dyn CanRx>,
    expected: u16,
}

impl ValidatedSdoRx {
    fn validate(&self, frame: &CanFrame) {
        if frame.id() == CanId::Standard(self.expected) {
            return;
        }
        log::warn!(
            "SDO RX filter violation: expected TSDO 0x{:03X} (node 0x{:02X}), got id={:?} kind={:?} dlc={} data={:02X?}",
            self.expected,
            self.expected - TSDO_BASE as u16,
            frame.id(),
            frame.kind(),
            frame.dlc(),
            frame.data(),
        );
    }
}

#[async_trait]
impl CanRx for ValidatedSdoRx {
    async fn recv(&mut self) -> std::result::Result<CanFrame, CanIoError> {
        let frame = self.inner.recv().await?;
        self.validate(&frame);
        Ok(frame)
    }

    fn try_recv(&mut self) -> std::result::Result<Option<CanFrame>, CanIoError> {
        let frame = self.inner.try_recv()?;
        if let Some(frame) = frame.as_ref() {
            self.validate(frame);
        }
        Ok(frame)
    }
}

fn with_exact_sdo_filter(bus: Arc<dyn CanBus>) -> Arc<dyn CanBus> {
    Arc::new(ExactSdoBus { inner: bus })
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
        return Ok((with_exact_sdo_filter(Arc::new(bus)), hw_ts));
    }

    let (kind, name) = match spec.split_once(':') {
        Some((k, n)) => (k, n),
        None => ("socketcan", spec),
    };
    match kind {
        #[cfg(target_os = "linux")]
        "socketcan" => {
            let bus = can_transport::socketcan::SocketCanBus::open(name)
                .with_context(|| format!("opening SocketCAN interface '{name}'"))?;
            ensure_socketcan_up(&bus, name).await?;
            // SocketCAN hardware timestamps would need SO_TIMESTAMPING,
            // which can-transport does not expose yet.
            Ok((with_exact_sdo_filter(Arc::new(bus)), false))
        }
        other => bail!(
            "backend '{other}' is not available on this build \
             (known: 'socketcan' on Linux, 'gs_usb<channel>' everywhere)"
        ),
    }
}

/// SocketCAN sockets can be opened while their netdev is administratively
/// down. Without this check the frontend reports a successful connection, but
/// no traffic can flow and the user gets no clue that `ip link set ... up` was
/// missed. State reporting is best-effort, so only a definite `Stopped`
/// blocks the open; unsupported devices (notably vcan) and transient netlink
/// query failures retain the previous behavior.
#[cfg(target_os = "linux")]
async fn ensure_socketcan_up(bus: &dyn CanBus, name: &str) -> Result<()> {
    match bus.bus_state().await {
        Ok(state) => match socketcan_down_hint(state, name) {
            Some(hint) => bail!("{hint}"),
            None => Ok(()),
        },
        Err(error) => {
            log::warn!(
                "could not query SocketCAN interface '{name}' state; \
                 continuing without an up/down check: {error}"
            );
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn socketcan_down_hint(state: Option<CanBusState>, name: &str) -> Option<String> {
    matches!(
        state,
        Some(CanBusState {
            state: Some(CanControllerState::Stopped),
            ..
        })
    )
    .then(|| {
        format!(
            "SocketCAN interface '{name}' is down; bring it up first with \
             `sudo ip link set dev {name} up`, then try again"
        )
    })
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
    use super::*;

    #[test]
    fn narrows_family_wide_tsdo_filter_to_encoded_node() {
        let broad = CanFilter::standard(0x594, TSDO_FAMILY_MASK as u16);
        let (exact, expected) = exact_tsdo_filter(broad).expect("TSDO filter");
        assert_eq!(expected, 0x594);
        assert_eq!(exact, CanFilter::exact_standard(0x594));
    }

    #[test]
    fn leaves_non_sdo_and_invalid_node_filters_unchanged() {
        assert!(exact_tsdo_filter(CanFilter::pass_all_standard()).is_none());
        assert!(exact_tsdo_filter(CanFilter::standard(0x180, 0x780)).is_none());
        assert!(exact_tsdo_filter(CanFilter::standard(0x580, 0x780)).is_none());
        assert!(exact_tsdo_filter(CanFilter::exact_standard(0x594)).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stopped_socketcan_state_explains_how_to_bring_the_link_up() {
        let stopped = CanBusState {
            state: Some(CanControllerState::Stopped),
            tx_errors: None,
            rx_errors: None,
        };
        let hint = socketcan_down_hint(Some(stopped), "can0").expect("down hint");
        assert!(hint.contains("SocketCAN interface 'can0' is down"));
        assert!(hint.contains("sudo ip link set dev can0 up"));

        let healthy = CanBusState {
            state: Some(CanControllerState::ErrorActive),
            ..Default::default()
        };
        assert_eq!(socketcan_down_hint(Some(healthy), "can0"), None);
        assert_eq!(socketcan_down_hint(None, "vcan0"), None);
    }
}
