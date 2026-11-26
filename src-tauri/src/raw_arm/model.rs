use hex_arm::{
    hardware::{feedback_model_positions, BusConfig},
    nalgebra::UnitQuaternion,
    ArmModel, AxisLimits, CartesianTarget, Description, IkOptions, Isometry3, JointSpec,
    JointState, Point3, ToolModel,
};
use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;

pub(super) const AXES: usize = 6;
pub(super) type Q = [f64; AXES];

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ToolKind {
    Gp80,
    Gr80,
}

#[derive(Clone, Serialize)]
pub(crate) struct ToolInfo {
    pub(super) kind: ToolKind,
    joint: JointInfo,
}

pub(super) fn load(arm_xml: &str, tool_xml: Option<&str>) -> anyhow::Result<(ArmModel, ModelInfo)> {
    let mut model = ArmModel::new(Description::from_urdf(arm_xml, "base_link", "link_6")?)?;
    let expected = [
        "joint_1", "joint_2", "joint_3", "joint_4", "joint_5", "joint_6",
    ];
    anyhow::ensure!(
        model.joints().iter().map(|j| j.name.as_str()).eq(expected),
        "Expected Firefly Y6 joints joint_1 through joint_6"
    );
    let tool = if let Some(xml) = tool_xml {
        let tool = ToolModel::new(
            Description::from_urdf(xml, "base_link", "tcp")?,
            Isometry3::identity(),
            &[0.0],
        )?;
        let [joint] = tool.joints() else {
            anyhow::bail!("Expected one independent GP80 or GR80 joint");
        };
        anyhow::ensure!(
            joint.lower.is_finite() && joint.upper.is_finite(),
            "Expected finite gripper joint limits"
        );
        // The driver's gripper commands identify the product by robot name,
        // independent joint name and joint type, including for imported URDFs.
        let robot = urdf_rs::read_from_string(xml)?;
        let joint_type = robot
            .joints
            .iter()
            .find(|j| j.name == joint.name)
            .map(|j| &j.joint_type);
        let kind = match (robot.name.as_str(), joint.name.as_str(), joint_type) {
            ("gp80", "finger_joint1", Some(urdf_rs::JointType::Prismatic)) => ToolKind::Gp80,
            ("gr80", "jaw_joint1", Some(urdf_rs::JointType::Revolute)) => ToolKind::Gr80,
            _ => anyhow::bail!(
                "Expected a GP80 with prismatic finger_joint1 or a GR80 with revolute jaw_joint1"
            ),
        };
        let info = ToolInfo {
            kind,
            joint: joint.into(),
        };
        model.set_tool(Some(tool));
        Some(info)
    } else {
        None
    };
    let home = home(&model);
    let info = ModelInfo {
        joints: model.joints().iter().map(JointInfo::from).collect(),
        home,
        tcp: pose(&model, &home)?,
        tool,
    };
    Ok((model, info))
}

#[derive(Clone, Serialize)]
pub(crate) struct JointInfo {
    name: String,
    lower: f64,
    upper: f64,
}

impl From<&JointSpec> for JointInfo {
    fn from(joint: &JointSpec) -> Self {
        Self {
            name: joint.name.clone(),
            lower: joint.lower,
            upper: joint.upper,
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct ModelInfo {
    joints: Vec<JointInfo>,
    home: Q,
    tcp: Pose,
    pub(super) tool: Option<ToolInfo>,
}

pub(super) fn home(model: &ArmModel) -> Q {
    let nominal = [0., 0., std::f64::consts::FRAC_PI_2, 0., 0., 0.];
    std::array::from_fn(|i| nominal[i].clamp(model.joints()[i].lower, model.joints()[i].upper))
}

#[derive(Clone, Serialize)]
pub(crate) struct Pose {
    xyz: [f64; 3],
    rpy: [f64; 3],
}

pub(super) fn pose(model: &ArmModel, q: &[f64]) -> hex_arm::Result<Pose> {
    let tcp = model.tcp_pose(q)?;
    let (r, p, y) = tcp.rotation.euler_angles();
    Ok(Pose {
        xyz: tcp.translation.vector.into(),
        rpy: [r, p, y],
    })
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(super) enum PreviewSeed {
    Ready { q: Vec<f64>, tcp: Pose },
    Invalid { error: String },
}

impl PreviewSeed {
    pub(super) fn start(
        &self,
        velocity: &[f64],
        acceleration: &[f64],
    ) -> Result<JointState, String> {
        match self {
            Self::Ready { q, .. } => Ok(JointState {
                position: q.clone(),
                velocity: velocity.to_vec(),
                acceleration: acceleration.to_vec(),
            }),
            Self::Invalid { error } => Err(error.clone()),
        }
    }
}

/// Keep the measured TCP available for display and calibration even if planning is blocked.
pub(super) fn feedback_poses(
    model: &ArmModel,
    measured_q: &[f64],
) -> hex_arm::Result<(Pose, PreviewSeed)> {
    let measured_tcp = pose(model, measured_q)?;
    let seed = feedback_model_positions(model.joints(), measured_q)
        .and_then(|q| {
            let tcp = if q == measured_q {
                measured_tcp.clone()
            } else {
                pose(model, &q)?
            };
            Ok(PreviewSeed::Ready { q, tcp })
        })
        .unwrap_or_else(|e| PreviewSeed::Invalid {
            error: e.to_string(),
        });
    Ok((measured_tcp, seed))
}

#[derive(Clone, Copy, Deserialize)]
pub(crate) struct Limits {
    velocity: f64,
    acceleration: f64,
    jerk: f64,
}

impl Limits {
    pub(super) fn axes(self) -> anyhow::Result<[AxisLimits; AXES]> {
        anyhow::ensure!(
            [self.velocity, self.acceleration, self.jerk]
                .iter()
                .all(|v| v.is_finite() && *v > 0.),
            "Velocity, acceleration and jerk must be positive"
        );
        let map = BusConfig::firefly_y6(None).mapping;
        let wire_speed = TAU * f64::from(map.velocity_max.min(-map.velocity_min));
        Ok([AxisLimits {
            velocity: self.velocity.min(wire_speed),
            acceleration: self.acceleration,
            jerk: self.jerk,
        }; AXES])
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Target {
    Joints {
        q: Q,
    },
    Tcp {
        xyz: [f64; 3],
        rpy: Option<[f64; 3]>,
    },
}

#[derive(Serialize)]
pub(crate) struct Preview {
    pub(super) id: u64,
    pub(super) goal: Vec<f64>,
    duration: f64,
    tcp: Pose,
    joints: Vec<Vec<f64>>,
    path: Vec<[f64; 3]>,
}

pub(super) fn plan(
    model: &ArmModel,
    start: JointState,
    target: Target,
    limits: Limits,
    id: u64,
) -> anyhow::Result<Preview> {
    let limits = limits.axes()?;
    let trajectory = match target {
        Target::Joints { q } => model.plan_joints(&start, &q, &limits)?,
        Target::Tcp { xyz, rpy } => {
            let target = if let Some([r, p, y]) = rpy {
                CartesianTarget::Pose(Isometry3::from_parts(
                    xyz.into(),
                    UnitQuaternion::from_euler_angles(r, p, y),
                ))
            } else {
                CartesianTarget::Position(Point3::from(xyz))
            };
            model.plan_to(&start, target, &limits, IkOptions::default())?
        }
    };
    let duration = trajectory.duration_s();
    let steps = (duration * 40.0).ceil().clamp(2., 240.) as usize;
    let mut joints = Vec::with_capacity(steps + 1);
    let mut path = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let q = trajectory
            .sample(duration * (i as f64 / steps as f64))?
            .position;
        path.push(model.tcp_pose(&q)?.translation.vector.into());
        joints.push(q);
    }
    let goal = joints[steps].clone();
    Ok(Preview {
        id,
        duration,
        tcp: pose(model, &goal)?,
        goal,
        joints,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(name: &str) -> String {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../../../src/generated/urdf-assets.json")).unwrap();
        let url = manifest["products"][name]["urdfUrl"].as_str().unwrap();
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../public")
                .join(url.trim_start_matches('/')),
        )
        .unwrap()
    }

    fn limits() -> Limits {
        Limits {
            velocity: 0.5,
            acceleration: 1.,
            jerk: 4.,
        }
    }

    #[test]
    fn ghost_samples_and_ik_endpoint_use_the_mounted_tcp() {
        let arm = product("firefly_y6");
        let gripper = product("gp80");
        let (bare, bare_info) = load(&arm, None).unwrap();
        let (mounted, gp_info) = load(&arm, Some(&gripper)).unwrap();
        let (rotary, gr_info) = load(&arm, Some(&product("gr80"))).unwrap();
        assert!(bare_info.tool.is_none());
        assert_eq!(gp_info.tool.unwrap().kind, ToolKind::Gp80);
        let jaw = gr_info.tool.unwrap();
        assert_eq!(jaw.kind, ToolKind::Gr80);
        assert_eq!(jaw.joint.name, "jaw_joint1");
        assert_eq!((jaw.joint.lower, jaw.joint.upper), (0., 0.6));
        let q = [0.2, -0.35, 1.45, 0.1, -0.25, 0.2];
        for m in [&mounted, &rotary] {
            let tool_delta = bare.flange_pose(&q).unwrap().inverse() * m.tcp_pose(&q).unwrap();
            assert!((tool_delta.translation.vector.z - 0.13).abs() < 1e-6);
            assert_ne!(
                bare.gravity_effort(&q).unwrap(),
                m.gravity_effort(&q).unwrap()
            );
        }
        for m in [&bare, &mounted, &rotary] {
            let start = JointState::at_rest(&q);
            let mut goal = q;
            goal[0] += 0.2;
            let fk = plan(m, start.clone(), Target::Joints { q: goal }, limits(), 1).unwrap();
            assert_eq!(fk.joints.len(), fk.path.len());
            assert!(fk.duration > 0. && fk.joints.len() > 2);
            assert_eq!(&fk.joints[0], &start.position);
            for (sample, xyz) in fk.joints.iter().zip(&fk.path) {
                assert_eq!(&pose(m, sample).unwrap().xyz, xyz);
            }
            for rpy in [None, Some(fk.tcp.rpy)] {
                let target = Target::Tcp {
                    xyz: fk.tcp.xyz,
                    rpy,
                };
                let ik = plan(m, start.clone(), target, limits(), 2).unwrap();
                let actual = m.tcp_pose(&ik.goal).unwrap();
                let wanted = m.tcp_pose(&goal).unwrap();
                assert!((actual.translation.vector - wanted.translation.vector).norm() < 1e-5);
                if rpy.is_some() {
                    assert!(actual.rotation.angle_to(&wanted.rotation) < 1e-4);
                }
            }
            let slow = Limits {
                velocity: 0.05,
                ..limits()
            };
            assert!(
                plan(m, start.clone(), Target::Joints { q: goal }, slow, 3)
                    .unwrap()
                    .duration
                    > fk.duration
            );
            assert!(plan(m, start, Target::Joints { q: [20.; AXES] }, limits(), 4).is_err());
        }
    }

    #[test]
    fn rejects_a_gripper_whose_declared_product_or_joint_type_disagrees() {
        let arm = product("firefly_y6");
        let gr80 = product("gr80");
        for invalid in [
            gr80.replace("robot name=\"gr80\"", "robot name=\"gp80\""),
            gr80.replace(
                "name=\"jaw_joint1\" type=\"revolute\"",
                "name=\"jaw_joint1\" type=\"prismatic\"",
            ),
        ] {
            assert!(load(&arm, Some(&invalid)).is_err());
        }
    }

    #[test]
    fn parked_feedback_seeds_joint_and_tcp_previews_within_the_limits() {
        let (model, _) = load(&product("firefly_y6"), Some(&product("gp80"))).unwrap();
        let measured = [0., -1.5719, 3.1404, 0., 0., 0.];
        let boundary = [0., -1.57, 3.14, 0., 0., 0.];
        let (actual, seed) = feedback_poses(&model, &measured).unwrap();
        let PreviewSeed::Ready { q, tcp } = &seed else {
            panic!("expected a valid parked pose")
        };
        assert_eq!(actual.xyz, pose(&model, &measured).unwrap().xyz);
        assert_eq!(tcp.xyz, pose(&model, &boundary).unwrap().xyz);
        assert_eq!(q, &boundary);
        let rest = [0.; AXES];
        let start = seed.start(&rest, &rest).unwrap();
        for target in [
            Target::Joints {
                q: q.as_slice().try_into().unwrap(),
            },
            Target::Tcp {
                xyz: tcp.xyz,
                rpy: Some(tcp.rpy),
            },
        ] {
            let preview = plan(&model, start.clone(), target, limits(), 1).unwrap();
            assert_eq!(preview.joints[0], boundary);
            assert_eq!(preview.goal, boundary);
        }
        let mut inward = boundary;
        inward[1] = -1.5;
        assert!(
            plan(
                &model,
                start.clone(),
                Target::Joints { q: inward },
                limits(),
                2
            )
            .unwrap()
            .duration
                > 0.
        );
        assert!(plan(&model, start, Target::Joints { q: measured }, limits(), 3).is_err());

        let mut outside = measured;
        outside[1] = -1.5721;
        let (actual, blocked) = feedback_poses(&model, &outside).unwrap();
        assert_eq!(actual.xyz, pose(&model, &outside).unwrap().xyz);
        assert!(blocked.start(&rest, &rest).unwrap_err().contains("joint_2"));
    }

    #[test]
    fn initial_stationary_preview_has_a_visible_endpoint() {
        let (model, _) = load(&product("firefly_y6"), Some(&product("gp80"))).unwrap();
        let q = home(&model);
        let stationary = plan(
            &model,
            JointState::at_rest(&q),
            Target::Joints { q },
            limits(),
            1,
        )
        .unwrap();
        assert_eq!(stationary.joints.first(), stationary.joints.last());
        assert_eq!(stationary.goal, q);
    }
}
