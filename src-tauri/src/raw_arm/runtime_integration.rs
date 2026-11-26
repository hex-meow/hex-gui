//! Wire-level regression for the app command loop with a stationary Y6 and GP80.
use super::*;
use crate::raw_arm::{commands, model};
use async_trait::async_trait;
use can_transport::{CanBus, CanCapabilities, CanFilter, CanFrame, CanIoError, CanRx};
use hex_motor::meow_motor::{PRODUCT_CODE_4310, VENDOR_ID};
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::{mpsc, oneshot};

const NODES: [u8; 7] = [1, 2, 3, 4, 5, 6, TOOL];
const CAL_AXIS: u8 = 3;
// Published 0x4001 v1 test vector for a 4310 with this serial, factor 1.0,
// no friction calibration. The CRC also binds the vendor and product IDs.
const SERIAL: u32 = 0x0111_b49a;
const FACTORY_WORDS: [u32; 7] = [0x0106_61b2, 0x89ab_cdef, 0x0123_4567, 0x1ccd_3800, 0, 0, 0];

#[derive(Default)]
struct Drives {
    nmt: [u8; 16],
    mode: [u8; 16],
    dictionary: HashMap<(u8, u16, u8), Vec<u8>>,
    calibration_reads: [u32; 16],
    fail_next_j3_read: bool,
    // Withhold the target readback after the gripper has already entered PP.
    delay_target: Option<oneshot::Sender<CanFrame>>,
}

impl Drives {
    fn assert_initialized(&self, node: u8, pdo_host: u32, slot: u8) {
        let check_entry = |index, sub, expected: &[u8]| {
            assert_eq!(
                self.dictionary.get(&(node, index, sub)).map(Vec::as_slice),
                Some(expected),
                "node {node:#04x} object {index:#06x}:{sub:02x}"
            );
        };
        assert_eq!(self.mode[usize::from(node)], 0, "node {node:#04x} mode");
        check_entry(0x1800, 5, &4u16.to_le_bytes());
        check_entry(0x1016, 1, &[250, 0, 10, 0]);
        check_entry(0x1400, 1, &(0x180 + pdo_host).to_le_bytes());
        check_entry(0x1401, 1, &(0x280 + pdo_host).to_le_bytes());
        // Each slot maps two 32-bit MIT words and one 8-bit mode command.
        check_entry(0x1600, 2 * slot + 1, &0x4103_0220u32.to_le_bytes());
        check_entry(0x1600, 2 * slot + 2, &0x4103_0320u32.to_le_bytes());
        check_entry(0x1601, slot + 1, &0x4401_0008u32.to_le_bytes());
    }

    fn sdo(&mut self, node: u8, data: &[u8]) -> Option<CanFrame> {
        let index = u16::from_le_bytes([data[1], data[2]]);
        let sub = data[3];
        let slot = usize::from(node);
        let mut reply = [0; 8];
        reply[1..4].copy_from_slice(&data[1..4]);
        if data[0] == 0x40 {
            let value = match (index, sub) {
                (0x1018, 1) => VENDOR_ID.to_le_bytes().to_vec(),
                (0x1018, 2) => PRODUCT_CODE_4310.to_le_bytes().to_vec(),
                (0x1018, 3) => 1u32.to_le_bytes().to_vec(),
                (0x1018, 4) => (if node == CAL_AXIS {
                    SERIAL
                } else {
                    u32::from(node)
                })
                .to_le_bytes()
                .to_vec(),
                (0x1008, 0) => b"test".to_vec(),
                (0x2001, 0) => vec![4],
                (0x2001, 1) => vec![node],
                (0x2001, 2) => 1_000_000u32.to_le_bytes().to_vec(),
                (0x2001, 3) => 5_000_000u32.to_le_bytes().to_vec(),
                (0x2001, 4) => vec![1],
                (0x4402, 0) => vec![self.mode[slot]],
                (0x453f | 0x4572, 0) if !self.dictionary.contains_key(&(node, index, sub)) => {
                    vec![0; 2]
                }
                (0x4001, 0) => {
                    self.calibration_reads[slot] += 1;
                    if node == CAL_AXIS && std::mem::take(&mut self.fail_next_j3_read) {
                        reply[0] = 0x80;
                        reply[4..].copy_from_slice(&0x0800_0020u32.to_le_bytes());
                        return Some(CanFrame::new_data(0x580 + u16::from(node), &reply).unwrap());
                    }
                    vec![if node == CAL_AXIS { 7 } else { 0 }]
                }
                (0x4001, 1..=7) if node == CAL_AXIS => {
                    FACTORY_WORDS[usize::from(sub - 1)].to_le_bytes().to_vec()
                }
                _ => self
                    .dictionary
                    .get(&(node, index, sub))
                    .cloned()
                    .unwrap_or_else(|| vec![0; 4]),
            };
            reply[0] = 0x43 | ((4 - value.len()) as u8) << 2;
            reply[4..4 + value.len()].copy_from_slice(&value);
        } else {
            let len = 4 - usize::from((data[0] >> 2) & 3);
            self.dictionary
                .insert((node, index, sub), data[4..4 + len].to_vec());
            if index == 0x4401 {
                self.mode[slot] = data[4];
            }
            reply[0] = 0x60;
        }
        let frame = CanFrame::new_data(0x580 + u16::from(node), &reply).unwrap();
        if node == TOOL && index == 0x457a && data[0] == 0x40 {
            if let Some(seen) = self.delay_target.take() {
                seen.send(frame).unwrap();
                return None;
            }
        }
        Some(frame)
    }
}

#[derive(Default)]
struct Network {
    drives: Mutex<Drives>,
    subscribers: Mutex<Vec<(CanFilter, mpsc::UnboundedSender<CanFrame>)>>,
}

impl Network {
    fn emit(&self, frame: CanFrame) {
        self.subscribers.lock().unwrap().retain(|(filter, tx)| {
            !tx.is_closed() && (!filter.matches(&frame) || tx.send(frame).is_ok())
        });
    }

    async fn feedback(self: Arc<Self>) {
        let mut timer = interval(PERIOD);
        let mut stamp = 0u16;
        loop {
            timer.tick().await;
            stamp = stamp.wrapping_add(4000);
            let drives = self.drives.lock().unwrap();
            for node in NODES {
                let slot = usize::from(node);
                let id = u16::from(node);
                self.emit(CanFrame::new_data(0x700 + id, &[drives.nmt[slot]]).unwrap());
                if drives.nmt[slot] == 5 {
                    let mut position = [0; 8];
                    position[4..6].copy_from_slice(&stamp.to_le_bytes());
                    self.emit(CanFrame::new_fd(0x180 + id, &position, true).unwrap());
                    self.emit(
                        CanFrame::new_fd(
                            0x280 + id,
                            &[drives.mode[slot], 0, 0, 30, 0, 30, 0],
                            true,
                        )
                        .unwrap(),
                    );
                }
            }
        }
    }
}

struct Receiver(mpsc::UnboundedReceiver<CanFrame>);

#[async_trait]
impl CanRx for Receiver {
    async fn recv(&mut self) -> Result<CanFrame, CanIoError> {
        self.0.recv().await.ok_or(CanIoError::Disconnected)
    }
    fn try_recv(&mut self) -> Result<Option<CanFrame>, CanIoError> {
        match self.0.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(CanIoError::Disconnected),
        }
    }
}

#[async_trait]
impl CanBus for Network {
    fn capabilities(&self) -> CanCapabilities {
        CanCapabilities {
            fd: true,
            max_dlen: 64,
        }
    }
    async fn subscribe(&self, filter: CanFilter) -> Result<Box<dyn CanRx>, CanIoError> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.lock().unwrap().push((filter, tx));
        Ok(Box::new(Receiver(rx)))
    }
    async fn send(&self, frame: CanFrame) -> Result<(), CanIoError> {
        let data = frame.data();
        let mut drives = self.drives.lock().unwrap();
        match frame.id().raw() {
            0 => {
                let state = match data[0] {
                    0x80 => 127,
                    1 => 5,
                    _ => panic!("unexpected NMT command"),
                };
                drives.nmt[usize::from(data[1])] = state;
                self.emit(CanFrame::new_data(0x700 + u16::from(data[1]), &[state]).unwrap());
            }
            0x28a => drives.mode[1..=6].copy_from_slice(data),
            id @ 0x601..=0x67f => {
                if let Some(reply) = drives.sdo((id - 0x600) as u8, data) {
                    self.emit(reply);
                }
            }
            0x18a | 0x70a => {}
            id => panic!("unexpected CAN ID {id:#x}"),
        }
        Ok(())
    }
}

fn product(name: &str) -> String {
    let assets: serde_json::Value =
        serde_json::from_str(include_str!("../../../src/generated/urdf-assets.json")).unwrap();
    let path = assets["products"][name]["urdfUrl"]
        .as_str()
        .unwrap()
        .trim_start_matches('/');
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../public")
            .join(path),
    )
    .unwrap()
}

#[tokio::test]
async fn reread_one_axis_then_brake_and_release_during_delayed_gripper_readback() {
    let can = Arc::new(Network::default());
    {
        let mut drives = can.drives.lock().unwrap();
        drives.nmt.fill(127);
        drives.fail_next_j3_read = true;
    }
    let feedback_task = tokio::spawn(can.clone().feedback());
    let (model, _) = model::load(&product("firefly_y6"), Some(&product("gp80"))).unwrap();
    let installation = Installation {
        direction: [1; AXES],
        offset: model::home(&model),
        factor: [None; AXES],
        max_torque_permille: [None; AXES],
        finger_m_per_rev: 0.04,
        finger_offset_m: 0.,
        jaw_rad_per_rev: 1.,
        jaw_offset_rad: 0.,
    };
    let config = installation.bus_config(Some(ToolKind::Gp80)).unwrap();
    let tool = (ToolKind::Gp80, config.tool.unwrap().encoder.unwrap());
    let mut arm = ArmBus::new(can.clone(), config).unwrap();
    arm.initialize().await.unwrap();
    {
        let drives = can.drives.lock().unwrap();
        for node in 1..=6 {
            drives.assert_initialized(node, 0x0a, node - 1);
        }
        drives.assert_initialized(TOOL, 0x11, 0);
    }
    let (axes, factory) = installation.axes(&arm, &model).await;
    assert!(matches!(
        factory[2],
        FactoryReading::Error {
            last_valid: None,
            ..
        }
    ));
    let (tx, rx) = commands::channel();
    let (publisher, mut status) = watch::channel(Status::new(
        Default::default(),
        Default::default(),
        installation.offset,
        factory,
        None,
        None,
    ));
    let active = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(run(
        arm,
        model,
        axes,
        installation.factor,
        Some(tool),
        rx,
        publisher,
        active.clone(),
    ));
    let gains = Gains {
        kp: [0.; AXES],
        kd: [0.; AXES],
        damping: 0.,
    };
    let limits: Limits =
        serde_json::from_str(r#"{"velocity":0.5,"acceleration":1,"jerk":4}"#).unwrap();

    assert!(tx
        .request(|r| Command::Activate(Control::Hold, gains, r))
        .await
        .unwrap_err()
        .contains("J3"));
    let mut expected_reads = can.drives.lock().unwrap().calibration_reads;
    expected_reads[usize::from(CAL_AXIS)] += 1;
    assert_eq!(
        tx.request(|r| Command::ReadFactory(CAL_AXIS, r))
            .await
            .unwrap(),
        FactoryReading::Valid { factor: 1. }
    );
    assert_eq!(can.drives.lock().unwrap().calibration_reads, expected_reads);
    tx.request(|r| Command::Activate(Control::Hold, gains, r))
        .await
        .unwrap();

    let mut goal = installation.offset;
    goal[0] += 0.2;
    tx.request(|r| Command::Move(goal.to_vec(), limits, r))
        .await
        .unwrap();
    for release in [false, true] {
        let (seen, readback) = oneshot::channel();
        can.drives.lock().unwrap().delay_target = Some(seen);
        let motion = serde_json::from_str(r#"{"kind":"gp80","target":"open","speed":0.01,"acceleration":0.05,"max_torque_permille":120}"#).unwrap();
        let gripper = tx.request(|r| Command::MoveGripper(motion, r));
        let pending_reply = tokio::time::timeout(Duration::from_secs(2), readback)
            .await
            .unwrap()
            .unwrap();
        let queued_move = tx.request(|r| Command::Move(goal.to_vec(), limits, r));
        if release {
            tx.request(Command::Release).await.unwrap();
        } else {
            tx.request(|r| Command::Stop(limits, r)).await.unwrap();
        }
        // "interrupted" proves priority processing beat the real SDO timeout.
        assert!(gripper.await.unwrap_err().contains("interrupted"));
        assert!(queued_move.await.unwrap_err().contains("superseded"));
        can.emit(pending_reply);
        if release {
            assert_eq!(status.borrow().mode, Mode::Monitor);
            assert!(!active.load(Ordering::Acquire));
            for node in NODES {
                assert_eq!(can.drives.lock().unwrap().mode[usize::from(node)], 0);
            }
        } else {
            // The stationary encoders could never reach the original 0.2 rad goal.
            tokio::time::timeout(
                Duration::from_secs(1),
                status.wait_for(|s| {
                    s.mode == Mode::Hold && s.feedback.as_ref().is_some_and(|f| f.at_goal)
                }),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(&can.drives.lock().unwrap().mode[1..=6], &[4; AXES]);
        }
    }
    tx.request(Command::Close).await.unwrap();
    task.await.unwrap();
    feedback_task.abort();
}
