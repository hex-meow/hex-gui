//! Protocol-neutral SmartKnob command DTOs and active-session marker.

use serde::{Deserialize, Serialize};

use crate::smartknob::{KnobConfig, SmartKnob, SmartKnobState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmartKnobKind {
    Canopen,
    Rollercan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartKnobTarget {
    pub kind: SmartKnobKind,
    pub node_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SmartKnobControlSide {
    Host,
    Firmware,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SmartKnobEffortUnit {
    #[serde(rename = "Nm")]
    Nm,
    #[serde(rename = "A")]
    Ampere,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartKnobDevice {
    pub target: SmartKnobTarget,
    pub name: String,
    pub online: bool,
    pub control_side: SmartKnobControlSide,
    pub effort_unit: SmartKnobEffortUnit,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartKnobProfile {
    pub target: SmartKnobTarget,
    pub configs: Vec<KnobConfig>,
    pub control_side: SmartKnobControlSide,
    pub effort_unit: SmartKnobEffortUnit,
    pub supports_temperature: bool,
    pub supports_telemetry: bool,
    pub effort_limit_max: f64,
    pub max_output_permille: u16,
    pub telemetry_enabled: Option<bool>,
    pub telemetry_rate_hz: Option<u16>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartKnobTuning {
    pub p_gain: f64,
    pub d_gain: f64,
    pub strength_scale: f64,
    pub effort_limit: f64,
    pub max_output_permille: u16,
    pub friction_compensation: f64,
    pub click_effort: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartKnobTelemetry {
    pub enabled: bool,
    pub rate_hz: u16,
}

impl Default for SmartKnobTelemetry {
    fn default() -> Self {
        Self {
            enabled: true,
            rate_hz: 50,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartKnobStartRequest {
    pub target: SmartKnobTarget,
    pub config_index: usize,
    pub custom_config: Option<KnobConfig>,
    pub tuning: Option<SmartKnobTuning>,
    pub telemetry: Option<SmartKnobTelemetry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedSmartKnobState {
    #[serde(flatten)]
    pub knob: SmartKnobState,
    pub target: Option<SmartKnobTarget>,
    pub control_side: SmartKnobControlSide,
    pub effort_unit: SmartKnobEffortUnit,
    pub applied_effort: f64,
    pub measured_effort: Option<f32>,
    pub effort_limit: f64,
    pub max_output_permille: u16,
    pub telemetry_enabled: Option<bool>,
    pub telemetry_rate_hz: Option<u16>,
}

impl Default for UnifiedSmartKnobState {
    fn default() -> Self {
        Self::from_knob(
            SmartKnobState::default(),
            None,
            SmartKnobControlSide::Host,
            SmartKnobEffortUnit::Nm,
            None,
        )
    }
}

impl UnifiedSmartKnobState {
    pub fn from_knob(
        knob: SmartKnobState,
        target: Option<SmartKnobTarget>,
        control_side: SmartKnobControlSide,
        effort_unit: SmartKnobEffortUnit,
        telemetry: Option<SmartKnobTelemetry>,
    ) -> Self {
        Self {
            applied_effort: knob.applied_torque_nm,
            measured_effort: knob.measured_torque_nm,
            effort_limit: knob.torque_limit_nm,
            max_output_permille: knob.max_torque_permille,
            knob,
            target,
            control_side,
            effort_unit,
            telemetry_enabled: telemetry.map(|v| v.enabled),
            telemetry_rate_hz: telemetry.map(|v| v.rate_hz),
        }
    }
}

pub enum ActiveSmartKnob {
    Canopen(SmartKnob),
    Rollercan { node_id: u8 },
}

impl ActiveSmartKnob {
    pub fn target(&self) -> SmartKnobTarget {
        match self {
            Self::Canopen(app) => SmartKnobTarget {
                kind: SmartKnobKind::Canopen,
                node_id: app.node_id(),
            },
            Self::Rollercan { node_id } => SmartKnobTarget {
                kind: SmartKnobKind::Rollercan,
                node_id: *node_id,
            },
        }
    }
}
