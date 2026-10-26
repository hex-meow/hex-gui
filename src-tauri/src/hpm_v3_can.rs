//! Classic CAN only: wait for announcement, optional CiA-305 LSS, then SDO.
use async_trait::async_trait;
use can_transport::{
    CanBus, CanCapabilities, CanFilter, CanFrame, CanId, CanIoError, CanRx, FrameKind,
};
use canopen_sdo::asynch::{download_bytes, upload_bytes};
use hpm_usb_dfu::{v3, DfuError};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

fn failure(s: impl Into<String>) -> DfuError {
    DfuError::Protocol(s.into())
}
fn io_error(s: &str) -> CanIoError {
    CanIoError::backend(std::io::Error::other(s))
}

struct ClassicBus {
    inner: Arc<dyn CanBus>,
    announced: Arc<AtomicBool>,
}
#[async_trait]
impl CanBus for ClassicBus {
    async fn send(&self, f: CanFrame) -> Result<(), CanIoError> {
        if !self.announced.load(Ordering::SeqCst) {
            return Err(io_error("HPM TX before target announcement"));
        }
        if !classic(&f) {
            return Err(io_error(
                "HPM upgrade requires standard 11-bit Classic CAN, <=8 bytes",
            ));
        }
        self.inner.send(f).await
    }
    async fn subscribe(&self, f: CanFilter) -> Result<Box<dyn CanRx>, CanIoError> {
        Ok(Box::new(ClassicRx {
            inner: self.inner.subscribe(f).await?,
        }))
    }
    fn capabilities(&self) -> CanCapabilities {
        CanCapabilities {
            fd: false,
            max_dlen: 8,
        }
    }
}
struct ClassicRx {
    inner: Box<dyn CanRx>,
}
fn classic(f: &CanFrame) -> bool {
    matches!(f.id(), CanId::Standard(id) if id <= 0x7ff)
        && f.kind() == FrameKind::Data
        && f.data().len() <= 8
}
fn check_sdo_size(f: &CanFrame) -> Result<(), CanIoError> {
    let d = f.data();
    if d.len() == 8 && d[0] & 0xe0 == 0x40 && d[0] & 2 == 0 && d[1..4] == [0x10, 0x21, 2] {
        let n = u32::from_le_bytes(d[4..8].try_into().unwrap());
        if d[0] & 1 == 0 || n > 136 {
            return Err(io_error("oversized/unbounded HPM SDO upload"));
        }
    }
    Ok(())
}
#[async_trait]
impl CanRx for ClassicRx {
    async fn recv(&mut self) -> Result<CanFrame, CanIoError> {
        loop {
            let f = self.inner.recv().await?;
            if classic(&f) {
                check_sdo_size(&f)?;
                return Ok(f);
            }
        }
    }
    fn try_recv(&mut self) -> Result<Option<CanFrame>, CanIoError> {
        loop {
            let Some(f) = self.inner.try_recv()? else {
                return Ok(None);
            };
            if classic(&f) {
                check_sdo_size(&f)?;
                return Ok(Some(f));
            }
        }
    }
}
struct Mailbox {
    bus: Arc<dyn CanBus>,
    node: u8,
    runtime: tokio::runtime::Handle,
}
impl v3::Transport for Mailbox {
    fn owner(&self) -> u8 {
        2
    }
    fn node_id(&self) -> Option<u8> {
        Some(self.node)
    }
    fn exchange(&mut self, packet: &[u8], end: Instant) -> hpm_usb_dfu::Result<Vec<u8>> {
        self.runtime.block_on(async {
            let timeout = tokio::time::Instant::from_std(end);
            tokio::time::timeout_at(timeout, async {
                // Retry the entire mailbox with the exact same envelope/RID.
                // The SDO library itself does not replay individual segments.
                for attempt in 0..2 {
                    let result = async {
                        download_bytes(
                            self.bus.as_ref(),
                            self.node,
                            v3::MAILBOX,
                            1,
                            packet,
                            Some(Duration::from_millis(700)),
                        )
                        .await?;
                        upload_bytes(
                            self.bus.as_ref(),
                            self.node,
                            v3::MAILBOX,
                            2,
                            Some(Duration::from_millis(700)),
                        )
                        .await
                    }
                    .await;
                    match result {
                        Ok(b) => return Ok(b),
                        Err(e) if attempt == 1 => {
                            return Err(failure(format!("HPM SDO mailbox: {e}")))
                        }
                        Err(_) => {}
                    }
                }
                unreachable!()
            })
            .await
            .map_err(|_| failure("HPM CAN deadline expired"))?
        })
    }
}
async fn send(bus: &dyn CanBus, id: u16, data: &[u8], end: Instant) -> hpm_usb_dfu::Result<()> {
    let frame = CanFrame::new_data(
        CanId::new_standard(id).map_err(|e| failure(e.to_string()))?,
        data,
    )
    .map_err(|e| failure(e.to_string()))?;
    tokio::time::timeout_at(tokio::time::Instant::from_std(end), bus.send(frame))
        .await
        .map_err(|_| failure("CAN send deadline"))?
        .map_err(|e| failure(e.to_string()))
}
async fn lss(
    bus: &dyn CanBus,
    data: [u8; 8],
    expect: u8,
    end: Instant,
    cancel: &AtomicBool,
) -> hpm_usb_dfu::Result<Option<[u8; 8]>> {
    if cancel.load(Ordering::SeqCst) {
        return Err(failure("CAN capture cancelled"));
    }
    let mut rx = bus
        .subscribe(CanFilter::exact_standard(0x7e4))
        .await
        .map_err(|e| failure(e.to_string()))?;
    send(bus, 0x7e5, &data, end).await?;
    let timeout = if data[0] == 0x17 {
        Duration::from_millis(700)
    } else {
        Duration::from_millis(40)
    };
    let until = std::cmp::min(end, Instant::now() + timeout);
    loop {
        match tokio::time::timeout_at(tokio::time::Instant::from_std(until), rx.recv()).await {
            Err(_) => return Ok(None),
            Ok(Err(e)) => return Err(failure(e.to_string())),
            Ok(Ok(f)) if f.data().len() == 8 && f.data()[0] == expect => {
                return Ok(Some(f.data().try_into().unwrap()))
            }
            _ => {}
        }
    }
}
async fn fast(
    bus: &dyn CanBus,
    value: u32,
    bit: u8,
    sub: u8,
    next: u8,
    end: Instant,
    cancel: &AtomicBool,
) -> hpm_usb_dfu::Result<bool> {
    let mut data = [0u8; 8];
    data[0] = 0x51;
    data[1..5].copy_from_slice(&value.to_le_bytes());
    data[5] = bit;
    data[6] = sub;
    data[7] = next;
    Ok(lss(bus, data, 0x4f, end, cancel).await?.is_some())
}
async fn assign(
    bus: &dyn CanBus,
    id: u8,
    vendor: u32,
    product: u32,
    uid: Option<[u8; 16]>,
    end: Instant,
    cancel: &AtomicBool,
) -> hpm_usb_dfu::Result<u32> {
    if !(1..=127).contains(&id) || [100, 101].contains(&id) {
        return Err(failure(
            "choose a free Node-ID 1..127, excluding reserved 100/101",
        ));
    }
    if !fast(bus, 0, 128, 0, 0, end, cancel).await? {
        return Err(failure("no unconfigured LSS device responded"));
    }
    // Known vendor/product/revision are exact field checks; only serial scans bits.
    for (sub, value) in [vendor, product, v3::VERSION as u32]
        .into_iter()
        .enumerate()
    {
        if !fast(bus, value, 0, sub as u8, sub as u8 + 1, end, cancel).await? {
            return Err(failure(
                "no unconfigured V3 device matches vendor/product/revision",
            ));
        }
    }
    let mut serial = 0;
    for bit in (0..32).rev() {
        if !fast(bus, serial, bit, 3, 3, end, cancel).await? {
            serial |= 1 << bit;
        }
    }
    if !fast(bus, serial, 0, 3, 0, end, cancel).await? {
        return Err(failure("LSS serial confirmation failed"));
    }
    if let Some(uid) = uid {
        if u32::from_le_bytes(Sha256::digest(uid)[..4].try_into().unwrap()) != serial {
            return Err(failure(
                "LSS serial differs from requested UID; no address stored",
            ));
        }
    }
    for (cmd, argument) in [(0x11, id), (0x17, 0)] {
        let mut data = [0; 8];
        data[0] = cmd;
        data[1] = argument;
        let ack = lss(bus, data, cmd, end, cancel)
            .await?
            .ok_or_else(|| failure("LSS configuration/store timed out"))?;
        if ack[1] != 0 || ack[2] != 0 {
            return Err(failure(format!(
                "LSS {cmd:02X} rejected: {} {}",
                ack[1], ack[2]
            )));
        }
    }
    send(bus, 0x7e5, &[4, 0, 0, 0, 0, 0, 0, 0], end).await?;
    Ok(serial)
}

pub async fn capture(
    bus: Arc<dyn CanBus>,
    node: Option<u8>,
    assign_id: Option<u8>,
    vendor: u32,
    product: u32,
    uid: Option<[u8; 16]>,
    cancel: Arc<AtomicBool>,
    expected_identity: Option<hexmeow_stm32_can_dfu::IdentitySnapshot>,
) -> hpm_usb_dfu::Result<v3::Session> {
    if node.is_some_and(|v| crate::can_node_policy::check(v).is_err()) {
        return Err(failure("invalid Node-ID"));
    }
    let end = Instant::now() + Duration::from_secs(30);
    let announced = Arc::new(AtomicBool::new(false));
    let bus: Arc<dyn CanBus> = Arc::new(ClassicBus {
        inner: bus,
        announced: announced.clone(),
    });
    let mut hb = bus
        .subscribe(CanFilter::standard(0x700, 0x780))
        .await
        .map_err(|e| failure(e.to_string()))?;
    let mut expected_serial = None;
    let chosen = loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(failure("CAN capture cancelled"));
        }
        let until = std::cmp::min(end, Instant::now() + Duration::from_millis(100));
        let f =
            match tokio::time::timeout_at(tokio::time::Instant::from_std(until), hb.recv()).await {
                Err(_) if Instant::now() < end => continue,
                Err(_) => {
                    return Err(failure(
                        "no target heartbeat; power on/reset after starting capture",
                    ))
                }
                Ok(Err(e)) => return Err(failure(e.to_string())),
                Ok(Ok(f)) => f,
            };
        let CanId::Standard(cob) = f.id() else {
            continue;
        };
        if f.data().len() != 1 || ![0, 4, 5, 127].contains(&f.data()[0]) {
            continue;
        }
        let nid = (cob & 127) as u8;
        if crate::can_node_policy::RESERVED.contains(&nid) {
            return Err(failure(crate::can_node_policy::check(nid).unwrap_err()));
        }
        if node.is_some_and(|n| n != nid) {
            continue;
        }
        announced.store(true, Ordering::SeqCst);
        if nid == 0 {
            let allocated = assign_id.ok_or_else(|| {
                failure("unconfigured device announced 0x700; select a free LSS Node-ID")
            })?;
            expected_serial =
                Some(assign(bus.as_ref(), allocated, vendor, product, uid, end, &cancel).await?);
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Err(failure("CAN capture cancelled"));
                }
                let until = std::cmp::min(end, Instant::now() + Duration::from_millis(100));
                let f =
                    match tokio::time::timeout_at(tokio::time::Instant::from_std(until), hb.recv())
                        .await
                    {
                        Err(_) if Instant::now() < end => continue,
                        Err(_) => return Err(failure("no heartbeat at saved LSS Node-ID")),
                        Ok(Err(e)) => return Err(failure(e.to_string())),
                        Ok(Ok(f)) => f,
                    };
                if f.id() == CanId::Standard(0x700 + allocated as u16) && f.data().len() == 1 {
                    break;
                }
            }
            break allocated;
        }
        break nid;
    };
    use hexmeow_stm32_can_dfu::{observe_identity, CanBusSdo};
    crate::can_node_policy::check(chosen).map_err(failure)?;
    let identity = observe_identity(
        &CanBusSdo::new(bus.as_ref()),
        chosen,
        Duration::from_millis(750),
    )
    .await
    .map_err(|e| failure(e.to_string()))?;
    if !crate::hpm_can_profiles::known(identity.vendor_id(), identity.product_code()) {
        return Err(failure(
            "unknown 0x1018 identity; no HPM mailbox or CLAIM was sent",
        ));
    }
    if expected_identity.is_some_and(|expected| expected != identity) {
        return Err(failure(
            "complete 0x1018 changed after discovery; no CLAIM was sent",
        ));
    }
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let mut mailbox = Mailbox {
            bus,
            node: chosen,
            runtime,
        };
        let info = v3::parse_info(&v3::query(&mut mailbox, 1, 1, &[], end)?)?;
        if info.product != identity.product_code()
            || u32::from_le_bytes(Sha256::digest(info.uid)[..4].try_into().unwrap())
                != identity.serial_number()
        {
            return Err(failure("V3 INFO disagrees with 0x1018 identity"));
        }
        if info.capabilities & v3::CAP_CAN == 0 {
            return Err(failure("device does not advertise CAN V3"));
        }
        if expected_serial.is_some() && info.product != product {
            return Err(failure("SDO product differs from the LSS-selected product"));
        }
        if uid.is_some_and(|u| u != info.uid) {
            return Err(failure("CAN target UID mismatch"));
        }
        if expected_serial.is_some_and(|s| {
            s != u32::from_le_bytes(Sha256::digest(info.uid)[..4].try_into().unwrap())
        }) {
            return Err(failure("LSS serial/full UID mismatch"));
        }
        v3::Session::claim(Box::new(mailbox), info, end)
    })
    .await
    .map_err(|e| failure(format!("CAN capture worker: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
    #[derive(Default)]
    struct Server {
        sent: Vec<CanFrame>,
        subscribers: Vec<UnboundedSender<CanFrame>>,
        domain: Vec<u8>,
        expected: usize,
        identity_vendor: Option<u32>,
        identity_serial: u32,
        heartbeat_node: Option<u8>,
    }
    #[derive(Default)]
    struct Bus(Mutex<Server>);
    struct Rx(UnboundedReceiver<CanFrame>);
    #[async_trait]
    impl CanRx for Rx {
        async fn recv(&mut self) -> Result<CanFrame, CanIoError> {
            self.0.recv().await.ok_or_else(|| io_error("closed"))
        }
        fn try_recv(&mut self) -> Result<Option<CanFrame>, CanIoError> {
            match self.0.try_recv() {
                Ok(f) => Ok(Some(f)),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
                Err(_) => Err(io_error("closed")),
            }
        }
    }
    #[async_trait]
    impl CanBus for Bus {
        async fn send(&self, f: CanFrame) -> Result<(), CanIoError> {
            let mut s = self.0.lock().unwrap();
            s.sent.push(f);
            let d = f.data();
            let mut reply = [0; 8];
            if let Some(vendor) = s.identity_vendor {
                assert_eq!(
                    d[0], 0x40,
                    "unknown/unrevalidated identity received an SDO download"
                );
                assert_eq!(
                    &d[1..3],
                    &[0x18, 0x10],
                    "identity probe touched a proprietary object"
                );
                reply[..4].copy_from_slice(&[
                    if d[3] == 0 { 0x4f } else { 0x43 },
                    0x18,
                    0x10,
                    d[3],
                ]);
                let value = match d[3] {
                    0 => 4,
                    1 => vendor,
                    2 => crate::hpm_can_profiles::EVK_PRODUCT,
                    3 => 0x0200,
                    _ => s.identity_serial,
                };
                reply[4..8].copy_from_slice(&value.to_le_bytes());
            } else if d[0] == 0x21 {
                s.domain.clear();
                s.expected = u32::from_le_bytes(d[4..8].try_into().unwrap()) as usize;
                reply[..4].copy_from_slice(&[0x60, d[1], d[2], d[3]]);
            } else if d[0] & 0xe0 == 0 {
                let n = if d[0] & 1 != 0 {
                    7 - ((d[0] >> 1) & 7) as usize
                } else {
                    7
                };
                s.domain.extend_from_slice(&d[1..1 + n]);
                reply[0] = 0x20 | (d[0] & 0x10);
                if d[0] & 1 != 0 {
                    assert_eq!(s.domain.len(), s.expected);
                }
            } else {
                return Err(io_error("unexpected command"));
            }
            let frame = CanFrame::new_data(CanId::Standard(0x59f), &reply)?;
            s.subscribers.retain(|tx| tx.send(frame).is_ok());
            Ok(())
        }
        async fn subscribe(&self, filter: CanFilter) -> Result<Box<dyn CanRx>, CanIoError> {
            let (tx, rx) = unbounded_channel();
            let hb = CanFrame::new_data(
                0x700u16 + u16::from(self.0.lock().unwrap().heartbeat_node.unwrap_or(31)),
                &[0x7f],
            )
            .unwrap();
            if filter.matches(&hb) {
                tx.send(hb).unwrap();
            }
            self.0.lock().unwrap().subscribers.push(tx);
            Ok(Box::new(Rx(rx)))
        }
        fn capabilities(&self) -> CanCapabilities {
            CanCapabilities {
                fd: true,
                max_dlen: 64,
            }
        }
    }
    #[tokio::test]
    async fn unknown_identity_receives_only_five_standard_identity_reads() {
        let bus = Arc::new(Bus::default());
        bus.0.lock().unwrap().identity_vendor = Some(0x12345678);
        let result = capture(
            bus.clone(),
            Some(31),
            None,
            0,
            0,
            None,
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await;
        assert!(result.is_err());
        let s = bus.0.lock().unwrap();
        assert_eq!(s.sent.len(), 5);
        assert!(s
            .sent
            .iter()
            .all(|f| f.data()[0] == 0x40 && f.data()[1..3] == [0x18, 0x10]));
    }
    #[tokio::test]
    async fn discovery_identity_change_prevents_mailbox_and_claim() {
        use hexmeow_stm32_can_dfu::{observe_identity, CanBusSdo};
        let bus = Arc::new(Bus::default());
        {
            let mut s = bus.0.lock().unwrap();
            s.identity_vendor = Some(crate::hpm_can_profiles::VENDOR);
            s.identity_serial = 123;
        }
        let expected = observe_identity(&CanBusSdo::new(bus.as_ref()), 31, Duration::from_secs(1))
            .await
            .unwrap();
        {
            let mut s = bus.0.lock().unwrap();
            s.sent.clear();
            s.identity_serial = 456;
        }
        let result = capture(
            bus.clone(),
            Some(31),
            None,
            0,
            0,
            None,
            Arc::new(AtomicBool::new(false)),
            Some(expected),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(bus.0.lock().unwrap().sent.len(), 5);
    }
    #[tokio::test]
    async fn reserved_current_node_is_rejected_before_any_bus_traffic() {
        for node in [100, 101] {
            let bus = Arc::new(Bus::default());
            assert!(capture(
                bus.clone(),
                Some(node),
                None,
                0,
                0,
                None,
                Arc::new(AtomicBool::new(false)),
                None
            )
            .await
            .is_err());
            assert!(bus.0.lock().unwrap().sent.is_empty());
        }
    }
    #[tokio::test]
    async fn observed_reserved_heartbeat_blocks_capture_without_transmission() {
        let bus = Arc::new(Bus::default());
        bus.0.lock().unwrap().heartbeat_node = Some(100);
        assert!(capture(
            bus.clone(),
            Some(31),
            None,
            0,
            0,
            None,
            Arc::new(AtomicBool::new(false)),
            None
        )
        .await
        .is_err());
        assert!(bus.0.lock().unwrap().sent.is_empty());
    }
    #[tokio::test]
    async fn opaque_128_and_full_308_byte_prepare_use_only_classic_sdo_segments() {
        for n in [128usize, 308] {
            let raw = Arc::new(Bus::default());
            let guarded = ClassicBus {
                inner: raw.clone(),
                announced: Arc::new(AtomicBool::new(true)),
            };
            let payload: Vec<u8> = (0..n).map(|i| i as u8).collect();
            download_bytes(
                &guarded,
                31,
                0x2110,
                1,
                &payload,
                Some(Duration::from_secs(1)),
            )
            .await
            .unwrap();
            let s = raw.0.lock().unwrap();
            assert_eq!(s.domain, payload);
            assert_eq!(s.sent.len(), 1 + n.div_ceil(7));
            assert!(s
                .sent
                .iter()
                .all(|f| classic(f) && f.id() == CanId::Standard(0x61f) && f.data().len() == 8));
        }
    }
    #[tokio::test]
    async fn no_transmit_before_announcement_and_no_fd_extended_or_remote_after_it() {
        let raw = Arc::new(Bus::default());
        let announced = Arc::new(AtomicBool::new(false));
        let bus = ClassicBus {
            inner: raw.clone(),
            announced: announced.clone(),
        };
        assert!(bus
            .send(CanFrame::new_data(0x61fu16, &[0; 8]).unwrap())
            .await
            .is_err());
        announced.store(true, Ordering::SeqCst);
        for frame in [
            CanFrame::new_fd(0x61fu16, &[0; 8], false).unwrap(),
            CanFrame::new_fd(0x61fu16, &[0; 16], true).unwrap(),
            CanFrame::new_data(CanId::Extended(0x61f), &[0; 8]).unwrap(),
            CanFrame::new_remote(0x61fu16, 8).unwrap(),
        ] {
            assert!(bus.send(frame).await.is_err());
        }
        assert!(raw.0.lock().unwrap().sent.is_empty());
    }
    #[test]
    fn oversized_or_unbounded_sdo_upload_is_rejected_before_allocation() {
        for (cmd, n) in [(0x41, 137u32), (0x40, 0)] {
            let mut d = [0; 8];
            d[..4].copy_from_slice(&[cmd, 0x10, 0x21, 2]);
            d[4..].copy_from_slice(&n.to_le_bytes());
            assert!(check_sdo_size(&CanFrame::new_data(0x59fu16, &d).unwrap()).is_err());
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "writes a signed APP to an explicitly selected HPM EVK; set HPM_V3_LIVE_CAN and HPM_V3_LIVE_ARTIFACT"]
    async fn evk_live_capture_silent_wait_then_flash() {
        let spec = std::env::var("HPM_V3_LIVE_CAN").expect("explicit CAN interface");
        let bytes = std::fs::read(
            std::env::var("HPM_V3_LIVE_ARTIFACT").expect("explicit signed artifact path"),
        )
        .unwrap();
        let gate = crate::can_lease::CanTransportGate::default();
        let lease = gate
            .try_acquire(crate::can_lease::CanOwner::DfuUpdate)
            .unwrap();
        let bus = crate::backend::open_classic_1m_bus(&spec, lease)
            .await
            .unwrap();
        println!("WAITING_FOR_CAN_TARGET");
        let assigned = std::env::var("HPM_V3_LIVE_ASSIGN_NODE")
            .ok()
            .map(|s| s.parse::<u8>().unwrap());
        let mut session = capture(
            bus,
            if assigned.is_some() { None } else { Some(31) },
            assigned,
            0x6865786d,
            0x45564b33,
            None,
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await
        .unwrap();
        let image = v3::Artifact::parse(bytes, session.info.clone()).unwrap();
        let silent = std::env::var("HPM_V3_LIVE_SILENT_SECONDS")
            .ok()
            .map(|s| s.parse::<u64>().unwrap())
            .unwrap_or(35);
        println!("CAPTURED_SILENT_WAIT_{silent}_SECONDS");
        tokio::time::sleep(Duration::from_secs(silent)).await;
        let outcome = tokio::task::spawn_blocking(move || {
            let r = session.flash(&image, &AtomicBool::new(false), |p| {
                println!("{:?} {}/{}", p.stage, p.completed, p.total)
            });
            session.close();
            r
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            outcome,
            hpm_usb_dfu::FlashOutcome::JumpAckedStartupUnconfirmed
        );
        println!("CAN_V3_SILENT_WAIT_AND_UPGRADE_PASS");
    }
}
