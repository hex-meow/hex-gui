use super::model::ToolKind;
use hex_arm::hardware::{
    Gr80Limits, Gr80Target, GripperLimits, GripperTarget, MeowMotorLogic, Servo,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Opening {
    Open,
    Closed,
    Position(f64),
}

/// GP80 uses full opening in metres, GR80 uses a single jaw angle in radians.
#[derive(Deserialize)]
pub(crate) struct GripperMotion {
    kind: ToolKind,
    target: Opening,
    speed: f64,
    acceleration: f64,
    max_torque_permille: u16,
}

impl GripperMotion {
    pub(super) async fn execute(
        self,
        servo: &mut Servo<'_>,
        period: Duration,
    ) -> hex_arm::Result<()> {
        match self.kind {
            ToolKind::Gp80 => {
                let target = match self.target {
                    Opening::Open => GripperTarget::Open,
                    Opening::Closed => GripperTarget::Closed,
                    Opening::Position(width) => GripperTarget::Width(width),
                };
                let limits = GripperLimits {
                    width_speed_m_s: self.speed,
                    width_acceleration_m_s2: self.acceleration,
                    max_torque_permille: self.max_torque_permille,
                };
                servo.command_gripper(target, limits, period).await?;
            }
            ToolKind::Gr80 => {
                let target = match self.target {
                    Opening::Open => Gr80Target::Open,
                    Opening::Closed => Gr80Target::Closed,
                    Opening::Position(angle) => Gr80Target::Angle(angle),
                };
                let limits = Gr80Limits {
                    speed_rad_s: self.speed,
                    acceleration_rad_s2: self.acceleration,
                    max_torque_permille: self.max_torque_permille,
                };
                servo.command_gr80(target, limits, period).await?;
            }
        }
        Ok(())
    }
}

/// Position and velocity in independent URDF joint coordinates (m or rad).
#[derive(Clone, Serialize)]
pub(super) struct ToolFeedback {
    pub(super) position: f64,
    pub(super) velocity: Option<f64>,
    pub(super) torque_permille: Option<i16>,
    pub(super) mode_display: Option<u8>,
}

impl ToolFeedback {
    pub(super) fn read(servo: &Servo<'_>, kind: ToolKind) -> hex_arm::Result<Self> {
        let mode_display = |mode| {
            Some(match mode {
                MeowMotorLogic::Disabled => 0,
                MeowMotorLogic::Enabled(mode) => mode.command_code(),
                MeowMotorLogic::Error { mode_display, .. } => mode_display,
            })
        };
        Ok(match kind {
            ToolKind::Gp80 => {
                let s = servo.gripper_state()?;
                Self {
                    position: s.width_m / 2.,
                    velocity: s.width_velocity_m_s.map(|v| v / 2.),
                    torque_permille: s.torque_permille,
                    mode_display: mode_display(s.mode),
                }
            }
            ToolKind::Gr80 => {
                let s = servo.gr80_state()?;
                Self {
                    position: s.angle_rad,
                    velocity: s.velocity_rad_s,
                    torque_permille: s.torque_permille,
                    mode_display: mode_display(s.mode),
                }
            }
        })
    }
}
