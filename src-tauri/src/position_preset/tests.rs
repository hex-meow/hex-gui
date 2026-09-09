use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use can_transport::{CanCapabilities, CanFilter, CanFrame, CanId, CanIoError, CanRx};
use tokio::sync::mpsc;

struct Exchange {
    request: [u8; 8],
    response: [u8; 8],
    changes_session: bool,
}

fn upload(index: u16, sub: u8, value: &[u8]) -> Exchange {
    let [lo, hi] = index.to_le_bytes();
    let request = [0x40, lo, hi, sub, 0, 0, 0, 0];
    let mut response = [
        0x43 | ((4 - value.len()) as u8) << 2,
        lo,
        hi,
        sub,
        0,
        0,
        0,
        0,
    ];
    response[4..4 + value.len()].copy_from_slice(value);
    Exchange {
        request,
        response,
        changes_session: false,
    }
}

fn download(index: u16, sub: u8, value: &[u8]) -> Exchange {
    let [lo, hi] = index.to_le_bytes();
    let mut request = [
        0x23 | ((4 - value.len()) as u8) << 2,
        lo,
        hi,
        sub,
        0,
        0,
        0,
        0,
    ];
    request[4..4 + value.len()].copy_from_slice(value);
    Exchange {
        request,
        response: [0x60, lo, hi, sub, 0, 0, 0, 0],
        changes_session: false,
    }
}

fn identity() -> MotorIdentity {
    MotorIdentity {
        node_id: 7,
        vendor_id: crate::device_registry::MEOW_MOTOR_VENDOR_ID,
        product_code: crate::device_registry::MEOW_MOTOR_4310_PRODUCT_CODE,
        revision_number: 42,
        serial_number: 1234,
        product_name: None,
    }
}

fn identity_reads() -> Vec<Exchange> {
    identity_reads_for(&identity())
}

fn identity_reads_for(id: &MotorIdentity) -> Vec<Exchange> {
    [
        id.vendor_id,
        id.product_code,
        id.revision_number,
        id.serial_number,
    ]
    .into_iter()
    .enumerate()
    .map(|(sub, value)| upload(0x1018, sub as u8 + 1, &value.to_le_bytes()))
    .collect()
}

struct ScriptedBus {
    exchanges: Mutex<VecDeque<Exchange>>,
    tx: Mutex<Option<mpsc::UnboundedSender<CanFrame>>>,
    session_changed: AtomicBool,
}

impl ScriptedBus {
    fn new(exchanges: Vec<Exchange>) -> Self {
        Self {
            exchanges: Mutex::new(exchanges.into()),
            tx: Mutex::new(None),
            session_changed: AtomicBool::new(false),
        }
    }

    fn check_session(&self) -> Result<()> {
        if self.session_changed.load(Ordering::SeqCst) {
            Err("device session changed".into())
        } else {
            Ok(())
        }
    }

    fn assert_finished(&self) {
        assert!(self.exchanges.lock().unwrap().is_empty());
    }
}

#[async_trait]
impl CanBus for ScriptedBus {
    async fn send(&self, frame: CanFrame) -> std::result::Result<(), CanIoError> {
        assert_eq!(frame.id(), CanId::Standard(0x607));
        let exchange = self
            .exchanges
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected CAN request (including an unwanted retry or write)");
        assert_eq!(frame.data(), exchange.request);
        if exchange.changes_session {
            self.session_changed.store(true, Ordering::SeqCst);
        }
        self.tx
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(CanFrame::new_data(0x587u16, &exchange.response)?)
            .map_err(|_| CanIoError::Disconnected)
    }

    async fn subscribe(
        &self,
        _filter: CanFilter,
    ) -> std::result::Result<Box<dyn CanRx>, CanIoError> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.tx.lock().unwrap() = Some(tx);
        Ok(Box::new(Receiver(rx)))
    }

    fn capabilities(&self) -> CanCapabilities {
        CanCapabilities {
            fd: true,
            max_dlen: 64,
        }
    }
}

struct Receiver(mpsc::UnboundedReceiver<CanFrame>);
#[async_trait]
impl CanRx for Receiver {
    async fn recv(&mut self) -> std::result::Result<CanFrame, CanIoError> {
        self.0.recv().await.ok_or(CanIoError::Disconnected)
    }
    fn try_recv(&mut self) -> std::result::Result<Option<CanFrame>, CanIoError> {
        match self.0.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(CanIoError::Disconnected),
        }
    }
}

#[tokio::test]
async fn both_motor_types_use_the_same_original_preset_transaction() {
    let mut cia = identity();
    cia.vendor_id = crate::device_registry::CIA402_MOTOR_VENDOR_ID;
    cia.product_code = crate::device_registry::CIA402_MOTOR_4310_PRODUCT_CODE;
    for id in [identity(), cia] {
        for (input, expected) in [
            (0.0_f32, 0.0_f32),
            (-0.25, -0.25),
            (0.5, 0.5),
            (1.0, 0.5),
            (-1.0, -0.5),
        ] {
            let mut exchanges = identity_reads_for(&id);
            if classify(id.vendor_id, id.product_code) == DeviceKind::Cia402Motor {
                exchanges.extend([
                    download(0x6040, 0, &0u16.to_le_bytes()),
                    upload(0x6041, 0, &0x0027u16.to_le_bytes()),
                    upload(0x6041, 0, &0x0250u16.to_le_bytes()),
                ]);
            } else {
                exchanges.extend([
                    download(0x4401, 0, &[0]),
                    upload(0x4402, 0, &[4]),
                    upload(0x4402, 0, &[0]),
                ]);
            }
            exchanges.extend([
                download(0x3001, 1, &expected.to_le_bytes()),
                download(0x3001, 2, b"pres"),
            ]);
            let bus = ScriptedBus::new(exchanges);
            preset_on_bus(&bus, &id, input, || bus.check_session(), DISABLE_TIMEOUT)
                .await
                .unwrap();
            bus.assert_finished();
        }
    }
}

#[tokio::test]
async fn cia402_keeps_original_disabled_mask_and_timeout() {
    let mut id = identity();
    id.vendor_id = crate::device_registry::CIA402_MOTOR_VENDOR_ID;
    id.product_code = crate::device_registry::CIA402_MOTOR_4310_PRODUCT_CODE;
    for status in [0x0250_u16, 0x0270, 0x0027] {
        let mut exchanges = identity_reads_for(&id);
        exchanges.extend([
            download(0x6040, 0, &0u16.to_le_bytes()),
            upload(0x6041, 0, &status.to_le_bytes()),
        ]);
        if status != 0x0027 {
            exchanges.extend([
                download(0x3001, 1, &0.0_f32.to_le_bytes()),
                download(0x3001, 2, b"pres"),
            ]);
        }
        let bus = ScriptedBus::new(exchanges);
        let result = preset_on_bus(&bus, &id, 0.0, || bus.check_session(), Duration::ZERO).await;
        assert_eq!(result.is_ok(), status != 0x0027);
        bus.assert_finished();
    }
}

#[tokio::test]
async fn read_uses_signed_q8_24_and_never_writes() {
    for (raw, expected) in [(0_i32, 0.0_f32), (-4_194_304, -0.25), (20_971_520, 1.25)] {
        let mut exchanges = identity_reads();
        exchanges.push(upload(0x4564, 0, &raw.to_le_bytes()));
        let bus = ScriptedBus::new(exchanges);
        assert_eq!(
            read_on_bus(&bus, &identity(), || bus.check_session())
                .await
                .unwrap(),
            expected
        );
        bus.assert_finished();
    }
}

#[tokio::test]
async fn replacement_identity_aborts_before_disable_or_position_read() {
    for preset in [true, false] {
        let mut exchanges = identity_reads();
        exchanges[3] = upload(0x1018, 4, &9999_u32.to_le_bytes());
        let bus = ScriptedBus::new(exchanges);
        let result = if preset {
            preset_on_bus(
                &bus,
                &identity(),
                0.0,
                || bus.check_session(),
                DISABLE_TIMEOUT,
            )
            .await
        } else {
            read_on_bus(&bus, &identity(), || bus.check_session())
                .await
                .map(|_| ())
        };
        assert!(result.unwrap_err().contains("identity changed"));
        bus.assert_finished();
    }
}

#[tokio::test]
async fn disable_error_or_timeout_never_writes_preset() {
    for display in [4, 0xAF] {
        let mut exchanges = identity_reads();
        exchanges.extend([download(0x4401, 0, &[0]), upload(0x4402, 0, &[display])]);
        let bus = ScriptedBus::new(exchanges);
        let error = preset_on_bus(
            &bus,
            &identity(),
            0.0,
            || bus.check_session(),
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(error.contains(if display == 4 {
            "timed out"
        } else {
            "error mode"
        }));
        bus.assert_finished();
    }
}

#[tokio::test]
async fn session_change_between_preset_value_and_save_aborts_commit() {
    let mut exchanges = identity_reads();
    let mut value = download(0x3001, 1, &0.25_f32.to_le_bytes());
    value.changes_session = true;
    exchanges.extend([download(0x4401, 0, &[0]), upload(0x4402, 0, &[0]), value]);
    let bus = ScriptedBus::new(exchanges);
    assert!(preset_on_bus(
        &bus,
        &identity(),
        0.25,
        || bus.check_session(),
        DISABLE_TIMEOUT
    )
    .await
    .unwrap_err()
    .contains("session changed"));
    bus.assert_finished();
}

#[tokio::test]
async fn sdo_abort_does_not_commit_or_retry() {
    let mut exchanges = identity_reads();
    let mut value = download(0x3001, 1, &0.0_f32.to_le_bytes());
    value.response = [0x80, 0x01, 0x30, 1, 0, 0, 2, 6];
    exchanges.extend([download(0x4401, 0, &[0]), upload(0x4402, 0, &[0]), value]);
    let bus = ScriptedBus::new(exchanges);
    assert!(preset_on_bus(
        &bus,
        &identity(),
        0.0,
        || bus.check_session(),
        DISABLE_TIMEOUT
    )
    .await
    .is_err());
    bus.assert_finished();
}

#[tokio::test]
async fn non_finite_preset_is_rejected_without_bus_traffic() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let bus = ScriptedBus::new(vec![]);
        assert!(
            preset_on_bus(&bus, &identity(), value, || Ok(()), DISABLE_TIMEOUT)
                .await
                .is_err()
        );
        bus.assert_finished();
    }
}
