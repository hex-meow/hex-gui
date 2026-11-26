use super::gripper::{GripperMotion, ToolFeedback};
use super::model::{feedback_poses, Limits, Pose, PreviewSeed, ToolKind, AXES, Q};
use anyhow::{anyhow, ensure, Context, Result};
use hex_arm::hardware::{
    ArmBus, AxisControl, BusConfig, CycleReport, Direction, MeowMotorLogic, Servo, ToolEncoder,
};
use hex_arm::{ArmModel, JointState};
use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, MissedTickBehavior};

const PERIOD: Duration = Duration::from_millis(4);
const DISPLAY_PERIOD: Duration = Duration::from_millis(40);
const TOOL: u8 = 0x0f;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Installation {
    direction: [i8; AXES],
    pub(super) offset: Q,
    factor: [Option<f64>; AXES],
    finger_m_per_rev: f64,
    finger_offset_m: f64,
    jaw_rad_per_rev: f64,
    jaw_offset_rad: f64,
}

impl Installation {
    pub(super) fn bus_config(&self, kind: Option<ToolKind>) -> Result<BusConfig> {
        ensure!(
            self.direction.iter().all(|d| matches!(d, -1 | 1))
                && self.offset.iter().all(|v| v.is_finite())
                && self
                    .factor
                    .iter()
                    .flatten()
                    .all(|v| v.is_finite() && *v > 0.),
            "Invalid joint direction, offset or torque factor"
        );
        let mut config = BusConfig::firefly_y6(kind.map(|_| TOOL));
        if let (Some(kind), Some(tool)) = (kind, &mut config.tool) {
            let (units_per_rev, offset) = match kind {
                ToolKind::Gp80 => (self.finger_m_per_rev, self.finger_offset_m),
                ToolKind::Gr80 => (self.jaw_rad_per_rev, self.jaw_offset_rad),
            };
            ensure!(
                units_per_rev.is_finite() && units_per_rev != 0. && offset.is_finite(),
                "Invalid {kind:?} encoder scale or offset"
            );
            tool.encoder = Some(ToolEncoder {
                joint_index: 0,
                units_per_rev,
                offset,
            });
        }
        Ok(config)
    }

    pub(super) async fn axes(
        &self,
        bus: &ArmBus,
        model: &ArmModel,
    ) -> (Vec<AxisControl>, Vec<Option<f64>>) {
        let mut axes = Vec::with_capacity(AXES);
        let mut factors = Vec::with_capacity(AXES);
        for (i, joint) in model.joints().iter().enumerate() {
            let node_id = (i + 1) as u8;
            let factory = bus
                .factory_calibration(node_id)
                .await
                .ok()
                .and_then(|c| c.valid().map(|v| v.torque.factor));
            factors.push(factory);
            axes.push(AxisControl {
                joint: joint.name.clone(),
                node_id,
                direction: if self.direction[i] == 1 {
                    Direction::Positive
                } else {
                    Direction::Negative
                },
                zero_offset_rad: self.offset[i],
                raw_nm_per_nm: self.factor[i].or(factory).unwrap_or(1.0),
                kp_nm_per_rad: 0.,
                kd_nm_s_per_rad: 0.,
            });
        }
        (axes, factors)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Control {
    Hold,
    Float,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    #[default]
    Monitor,
    Hold,
    Float,
    Recovery,
}

#[derive(Deserialize)]
pub(crate) struct Gains {
    kp: Q,
    kd: Q,
    damping: f64,
}

impl Gains {
    pub(super) fn apply(&self, axes: &[AxisControl], mode: Control) -> Result<Vec<AxisControl>> {
        let map = BusConfig::firefly_y6(None).mapping;
        let mut tuned = axes.to_vec();
        for (i, a) in tuned.iter_mut().enumerate() {
            let (kp, kd) = match mode {
                Control::Hold => (self.kp[i], self.kd[i]),
                Control::Float => (0., self.damping),
            };
            let scale = TAU * a.raw_nm_per_nm;
            ensure!(
                (0.0..=f64::from(map.kp_max)).contains(&(kp * scale))
                    && (0.0..=f64::from(map.kd_max)).contains(&(kd * scale)),
                "{} gains exceed the MIT encoding range",
                a.joint
            );
            a.kp_nm_per_rad = kp;
            a.kd_nm_s_per_rad = kd;
        }
        Ok(tuned)
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct Feedback {
    q: Vec<f64>,
    velocity: Vec<f64>,
    #[serde(skip)]
    acceleration: Vec<f64>,
    pub(super) tool: Option<ToolFeedback>,
    tcp: Pose,
    preview_seed: PreviewSeed,
    gravity_nm: Option<Vec<f64>>,
    moving: bool,
}

impl Feedback {
    fn from_cycle(servo: &Servo<'_>, tool: Option<ToolKind>, report: CycleReport) -> Result<Self> {
        let (tcp, preview_seed) = feedback_poses(servo.model(), &report.measured_q)?;
        Ok(Self {
            tcp,
            preview_seed,
            q: report.measured_q,
            velocity: report
                .measured_velocity_rad_s
                .into_iter()
                .map(|v| v.unwrap_or(0.))
                .collect(),
            acceleration: report.reference.acceleration,
            gravity_nm: Some(report.commanded_gravity_nm),
            moving: !report.at_trajectory_end,
            tool: tool
                .map(|kind| ToolFeedback::read(servo, kind))
                .transpose()?,
        })
    }

    pub(super) fn start(&self) -> Result<JointState, String> {
        self.preview_seed.start(&self.velocity, &self.acceleration)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(try_from = "Q")]
pub(crate) struct GravityScale(Q);

impl Default for GravityScale {
    fn default() -> Self {
        Self([1.; AXES])
    }
}

impl TryFrom<Q> for GravityScale {
    type Error = &'static str;

    fn try_from(scale: Q) -> Result<Self, Self::Error> {
        if scale.iter().all(|v| v.is_finite() && *v >= 0.) {
            Ok(Self(scale))
        } else {
            Err("Each gravity feedforward scale must be finite and nonnegative")
        }
    }
}

#[derive(Clone, Default, Serialize)]
pub(crate) struct Status {
    pub(super) mode: Mode,
    pub(super) feedback: Option<Feedback>,
    pub(super) error: Option<String>,
    gravity_scale: GravityScale,
}

pub(super) type Reply = oneshot::Sender<Result<(), String>>;
pub(super) enum Command {
    Activate {
        control: Control,
        axes: Vec<AxisControl>,
        reply: Reply,
    },
    SetGravityScale(GravityScale, Reply),
    Move(Vec<f64>, Limits, Reply),
    MoveGripper(GripperMotion, Reply),
    DisableGripper(Reply),
    Stop(Limits, Reply),
    Release(Reply),
    Offsets(Q, Reply),
    Close(Reply),
}

fn monitor(
    bus: &ArmBus,
    model: &mut ArmModel,
    axes: &[AxisControl],
    encoder: Option<ToolEncoder>,
) -> Result<Feedback> {
    let read = |id| -> Result<_> {
        let s = bus.node_status(id)?;
        ensure!(
            s.connection.online && s.lifecycle.is_ready(),
            "Node 0x{id:02x} has no live PDO feedback"
        );
        let rev = s
            .measurements
            .position
            .context("Missing output-shaft position")?
            .to_revolutions();
        Ok((rev, s.measurements))
    };
    let mut q = Vec::with_capacity(AXES);
    let mut velocity = Vec::with_capacity(AXES);
    for a in axes {
        let (rev, measurements) = read(a.node_id)?;
        q.push(a.joint_position(rev));
        let sign = match a.direction {
            Direction::Positive => 1.,
            Direction::Negative => -1.,
        };
        velocity.push(measurements.velocity_rev_per_s.unwrap_or(0.) * TAU * sign);
    }
    let tool = if let (Some(encoder), Some(tool)) = (encoder, model.tool_mut()) {
        let (rev, measurements) = read(TOOL)?;
        let q = encoder.units_per_rev.mul_add(rev, encoder.offset);
        let joint = &tool.joints()[0];
        let count = encoder.units_per_rev.abs() / (1_u32 << 24) as f64;
        ensure!(
            q >= joint.lower - count && q <= joint.upper + count,
            "{} position {q:.6} is outside its URDF limits. Check the gripper encoder scale and offset.", joint.name
        );
        let q = q.clamp(joint.lower, joint.upper);
        if tool.positions()[0] != q {
            tool.set_positions(&[q])?;
        }
        Some(ToolFeedback {
            position: q,
            velocity: measurements
                .velocity_rev_per_s
                .map(|v| v * encoder.units_per_rev),
            torque_permille: measurements.torque_permille,
            mode_display: measurements.mode_display,
        })
    } else {
        None
    };
    let (tcp, preview_seed) = feedback_poses(model, &q)?;
    Ok(Feedback {
        tcp,
        preview_seed,
        q,
        velocity,
        acceleration: vec![0.; AXES],
        gravity_nm: None,
        moving: false,
        tool,
    })
}

fn publish(tx: &watch::Sender<Status>, mode: Mode, result: Result<Feedback>) {
    tx.send_modify(|s| {
        s.mode = mode;
        (s.feedback, s.error) = match result {
            Ok(feedback) => (Some(feedback), None),
            Err(e) => (None, Some(e.to_string())),
        };
    });
}

fn set_gravity_scale(
    tx: &watch::Sender<Status>,
    scale: GravityScale,
    servo: Option<&mut Servo<'_>>,
) -> Result<()> {
    if let Some(servo) = servo {
        servo.set_gravity_scale(&scale.0)?;
    }
    tx.send_modify(|s| s.gravity_scale = scale);
    Ok(())
}

/// The servo stays on this task through preview, UI polling and operator pauses.
/// Only Release calls disable_arm. A failed tick never releases a loaded arm.
async fn serve(
    servo: &mut Servo<'_>,
    tool_kind: Option<ToolKind>,
    mode: Mode,
    rx: &mut mpsc::Receiver<Command>,
    tx: &watch::Sender<Status>,
) -> Option<Command> {
    let mut timer = interval(PERIOD);
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut previous_ui = Instant::now() - DISPLAY_PERIOD;
    let mut brake_after_gap = false;
    let mut limits: Option<[hex_arm::AxisLimits; AXES]> = None;
    loop {
        tokio::select! {
            _ = timer.tick() => {
                // After an interrupted move, resume with a measured-state brake.
                // Do not jump ahead to the point the old wall-clock profile reached.
                let result = async {
                    if brake_after_gap {
                        if let Some(l) = limits { servo.stop(&l)?; }
                        brake_after_gap = false;
                    }
                    servo.tick().await
                }.await;
                match result {
                    Ok(report) if previous_ui.elapsed() >= DISPLAY_PERIOD => {
                        if report.at_trajectory_end { limits = None; }
                        publish(tx, mode, Feedback::from_cycle(servo, tool_kind, report));
                        previous_ui = Instant::now();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        brake_after_gap = limits.is_some();
                        if previous_ui.elapsed() >= DISPLAY_PERIOD {
                            publish(tx, mode, Err(e.into()));
                            previous_ui = Instant::now();
                        }
                    }
                }
            }
            cmd = rx.recv() => match cmd? {
                Command::SetGravityScale(scale, reply) => {
                    let result = set_gravity_scale(tx, scale, Some(servo));
                    let _ = reply.send(result.map_err(|e| e.to_string()));
                }
                Command::MoveGripper(motion, reply) => {
                    let _ = reply.send(motion.execute(servo, PERIOD).await.map_err(|e| e.to_string()));
                }
                Command::DisableGripper(reply) => {
                    let _ = reply.send(servo.disable_gripper(PERIOD).await.map_err(|e| e.to_string()));
                }
                Command::Move(goal, l, reply) => {
                    let res = (|| -> Result<()> {
                        ensure!(mode == Mode::Hold, "Select position control before executing a trajectory");
                        let l = l.axes()?;
                        servo.move_joints(&goal, &l)?;
                        limits = Some(l);
                        brake_after_gap = false;
                        Ok(())
                    })();
                    let _ = reply.send(res.map_err(|e| e.to_string()));
                }
                Command::Stop(l, reply) => {
                    let res = (|| -> Result<()> {
                        ensure!(mode == Mode::Hold, "Trajectory braking requires position control");
                        let l = l.axes().or_else(|error| limits.ok_or(error))?;
                        servo.stop(&l)?;
                        limits = Some(l);
                        brake_after_gap = false;
                        Ok(())
                    })();
                    let _ = reply.send(res.map_err(|e| e.to_string()));
                }
                Command::Close(reply) => {
                    let _ = reply.send(Err("Support the arm and release the drives before disconnecting".into()));
                }
                Command::Offsets(_, reply) => { let _ = reply.send(Err("Change offsets in monitor mode".into())); }
                next => return Some(next),
            }
        }
    }
}

pub(super) async fn run(
    mut bus: ArmBus,
    mut model: ArmModel,
    mut axes: Vec<AxisControl>,
    tool: Option<(ToolKind, ToolEncoder)>,
    mut rx: mpsc::Receiver<Command>,
    tx: watch::Sender<Status>,
    active: Arc<AtomicBool>,
) {
    let mut timer = interval(DISPLAY_PERIOD);
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut next = None;
    let mut mode = Mode::Monitor;
    let encoder = tool.map(|(_, e)| e);
    loop {
        let cmd = match next.take() {
            Some(cmd) => cmd,
            None => tokio::select! {
                _ = timer.tick() => {
                    if mode == Mode::Recovery && axes.iter().map(|a| a.node_id).chain(tool.map(|_| TOOL)).all(|id| bus.node_status(id)
                        .is_ok_and(|s| !s.connection.online || (s.lifecycle.is_ready() && s.logic == Some(MeowMotorLogic::Disabled)))) {
                        active.store(false, Ordering::Release);
                        mode = Mode::Monitor;
                    }
                    publish(&tx, mode, monitor(&bus, &mut model, &axes, encoder));
                    continue;
                }
                cmd = rx.recv() => match cmd { Some(cmd) => cmd, None => break }
            },
        };
        match cmd {
            Command::Activate {
                control,
                axes: tuned,
                reply,
            } => {
                active.store(true, Ordering::Release);
                let scale = tx.borrow().gravity_scale;
                let enabled = bus
                    .enable_hold_with_gravity_scale(model.clone(), tuned, &scale.0)
                    .await;
                match enabled {
                    Ok(mut servo) => {
                        mode = match control {
                            Control::Hold => Mode::Hold,
                            Control::Float => Mode::Float,
                        };
                        tx.send_modify(|s| {
                            s.mode = mode;
                            s.error = None;
                        });
                        let _ = reply.send(Ok(()));
                        next =
                            serve(&mut servo, tool.map(|(kind, _)| kind), mode, &mut rx, &tx).await;
                        if next.is_none() {
                            break;
                        }
                    }
                    Err(error) => {
                        // Mode confirmation may have succeeded on a subset of the axes.
                        mode = Mode::Recovery;
                        publish(&tx, mode, Err(anyhow!(error.to_string())));
                        let _ = reply.send(Err(error.to_string()));
                    }
                }
            }
            Command::SetGravityScale(scale, reply) => {
                let result = set_gravity_scale(&tx, scale, None);
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            Command::Release(reply) => {
                let arm_release = bus.disable_arm().await;
                let tool_release = if tool.is_some() {
                    bus.disable_tool().await
                } else {
                    Ok(())
                };
                let res = arm_release.and(tool_release).map_err(|e| e.to_string());
                if res.is_ok() {
                    active.store(false, Ordering::Release);
                    mode = Mode::Monitor;
                } else {
                    mode = Mode::Recovery;
                }
                publish(&tx, mode, monitor(&bus, &mut model, &axes, encoder));
                let _ = reply.send(res);
            }
            Command::DisableGripper(reply) => {
                let _ = reply.send(bus.disable_tool().await.map_err(|e| e.to_string()));
            }
            Command::MoveGripper(_, reply) => {
                let _ = reply.send(Err(
                    "Enable hold or hand-guiding before moving the gripper".into()
                ));
            }
            Command::Offsets(offsets, reply) if mode == Mode::Monitor => {
                for (a, offset) in axes.iter_mut().zip(offsets) {
                    a.zero_offset_rad = offset;
                }
                publish(&tx, mode, monitor(&bus, &mut model, &axes, encoder));
                let _ = reply.send(Ok(()));
            }
            Command::Offsets(_, reply) => {
                let _ = reply.send(Err("Change offsets in monitor mode".into()));
            }
            Command::Close(reply) if !active.load(Ordering::Acquire) => {
                let _ = reply.send(Ok(()));
                break;
            }
            Command::Close(reply) => {
                let _ = reply.send(Err("Release the drives before disconnecting".into()));
            }
            Command::Move(_, _, reply) | Command::Stop(_, reply) => {
                let _ = reply.send(Err("Select position control first".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_keeps_gravity_scales_across_modes_feedback_and_rejected_edits() {
        let (tx, status) = watch::channel(Status::default());
        assert_eq!(status.borrow().gravity_scale.0, [1.; AXES]);
        let scales: GravityScale = serde_json::from_str("[0,0.85,1.1,1,0.95,1]").unwrap();
        set_gravity_scale(&tx, scales, None).unwrap();
        for mode in [Mode::Hold, Mode::Float, Mode::Monitor] {
            publish(&tx, mode, Err(anyhow!("waiting for feedback")));
            assert_eq!(status.borrow().gravity_scale, scales);
            assert_eq!(status.borrow().mode, mode);
        }
        for value in [-1., f64::NAN, f64::INFINITY] {
            let mut invalid = [2.; AXES];
            invalid[5] = value;
            assert!(GravityScale::try_from(invalid).is_err());
        }
        assert!(serde_json::from_str::<GravityScale>("[1,1,1,1,1]").is_err());
        assert!(serde_json::from_str::<GravityScale>("[1,1,1,1,1,-1]").is_err());
        assert_eq!(status.borrow().gravity_scale, scales);
    }

    #[test]
    fn selecting_a_tool_uses_only_its_signed_encoder_calibration() {
        let mut installation = Installation {
            direction: [1; AXES],
            offset: [0.; AXES],
            factor: [None; AXES],
            finger_m_per_rev: 0.04,
            finger_offset_m: 0.01,
            jaw_rad_per_rev: -2.4,
            jaw_offset_rad: 0.6,
        };
        for (kind, scale, offset) in [(ToolKind::Gp80, 0.04, 0.01), (ToolKind::Gr80, -2.4, 0.6)] {
            let node = installation.bus_config(Some(kind)).unwrap().tool.unwrap();
            let encoder = node.encoder.unwrap();
            assert_eq!(node.node_id, TOOL);
            assert_eq!(
                (encoder.joint_index, encoder.units_per_rev, encoder.offset),
                (0, scale, offset)
            );
        }
        installation.jaw_rad_per_rev = 0.;
        assert!(installation.bus_config(Some(ToolKind::Gr80)).is_err());
        assert!(installation.bus_config(Some(ToolKind::Gp80)).is_ok());
        assert!(installation.bus_config(None).unwrap().tool.is_none());
    }

    #[test]
    fn hand_guiding_keeps_installation_and_has_only_velocity_feedback() {
        let axes: Vec<_> = (0..AXES)
            .map(|i| AxisControl {
                joint: format!("joint_{}", i + 1),
                node_id: (i + 1) as u8,
                direction: Direction::Negative,
                zero_offset_rad: 0.2,
                raw_nm_per_nm: 1.1,
                kp_nm_per_rad: 0.,
                kd_nm_s_per_rad: 0.,
            })
            .collect();
        let gains = Gains {
            kp: [50.; AXES],
            kd: [3.; AXES],
            damping: 0.75,
        };
        let hold = gains.apply(&axes, Control::Hold).unwrap();
        let float = gains.apply(&axes, Control::Float).unwrap();
        for (h, f) in hold.iter().zip(float) {
            assert_eq!((h.kp_nm_per_rad, h.kd_nm_s_per_rad), (50., 3.));
            assert_eq!((f.kp_nm_per_rad, f.kd_nm_s_per_rad), (0., 0.75));
            assert_eq!(f.joint_position(0.25), h.joint_position(0.25));
            assert_eq!(f.raw_nm_per_nm, h.raw_nm_per_nm);
        }
    }
}
