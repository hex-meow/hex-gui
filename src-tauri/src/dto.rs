//! Serde-able mirrors of the `hex_motor` types we hand to the frontend.
//!
//! Kept intentionally flat / string-tagged so the JS side can pattern-match
//! on string fields instead of parsing rust Debug output.

use serde::{Deserialize, Serialize};

use hex_motor::canopen::nmt::NmtState;
use hex_motor::cia402::{
    Connection as CoreConnection, LiveState as CoreLiveState, Logic as CoreLogic,
    Measurements as CoreMeasurements, MotorInfo as CoreMotorInfo,
    MotorLifecycle as CoreMotorLifecycle, ReinitReason as CoreReinitReason,
};
use hex_motor::meow_motor::{
    MeowMotorCanSettingsStatus as CoreMeowCanSettingsStatus, MeowMotorInfo as CoreMeowMotorInfo,
    MeowMotorLifecycle as CoreMeowLifecycle, MeowMotorLiveState as CoreMeowLiveState,
    MeowMotorLogic as CoreMeowLogic, MeowMotorReinitReason as CoreMeowReinitReason,
};
use hex_motor::types::{
    DeviceCanConfig as CoreDeviceCanConfig, DeviceCanConfigStatus as CoreDeviceCanConfigStatus,
    DeviceSettingsResult as CoreDeviceSettingsResult,
    DeviceSettingsUpdate as CoreDeviceSettingsUpdate,
};
use hex_motor::types::{MotorErrorKind, MotorIdentity, MotorMode, MotorTarget};

#[derive(Debug, Clone, Serialize)]
pub struct CanBitTimingDto {
    pub bitrate: Option<u32>,
    pub sample_point_per_mille: Option<u16>,
}

impl From<can_transport::CanBitTiming> for CanBitTimingDto {
    fn from(timing: can_transport::CanBitTiming) -> Self {
        Self {
            bitrate: timing.bitrate,
            sample_point_per_mille: timing.sample_point_per_mille,
        }
    }
}

/// Read-only snapshot returned when the manager connection is established.
///
/// `inspection_error` is diagnostic only: opening a SocketCAN interface must
/// not fail merely because its timing cannot be inspected or is outside the
/// fleet profile.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionInfoDto {
    pub backend: String,
    pub fd_enabled: Option<bool>,
    pub nominal: Option<CanBitTimingDto>,
    pub data: Option<CanBitTimingDto>,
    pub inspection_error: Option<String>,
}

impl ConnectionInfoDto {
    pub fn new(
        backend: &str,
        config: Option<can_transport::CanLinkConfig>,
        inspection_error: Option<String>,
    ) -> Self {
        let config = config.unwrap_or_default();
        Self {
            backend: backend.to_owned(),
            fd_enabled: config.fd_enabled,
            nominal: config.nominal.map(Into::into),
            data: config.data.map(Into::into),
            inspection_error,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceCanConfigDto {
    pub stored_node_id: u8,
    pub nominal_bitrate: u32,
    pub data_bitrate: Option<u32>,
    pub transmit_pdo_brs: Option<bool>,
}

impl From<&CoreDeviceCanConfig> for DeviceCanConfigDto {
    fn from(config: &CoreDeviceCanConfig) -> Self {
        Self {
            stored_node_id: config.stored_node_id,
            nominal_bitrate: config.nominal_bitrate,
            data_bitrate: config.data_bitrate,
            transmit_pdo_brs: config.transmit_pdo_brs,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeviceCanConfigStatusDto {
    Pending,
    Available { config: DeviceCanConfigDto },
    Unsupported,
    ReadFailed { reason: String },
}

impl From<&CoreDeviceCanConfigStatus> for DeviceCanConfigStatusDto {
    fn from(status: &CoreDeviceCanConfigStatus) -> Self {
        match status {
            CoreDeviceCanConfigStatus::Pending => Self::Pending,
            CoreDeviceCanConfigStatus::Available(config) => Self::Available {
                config: config.into(),
            },
            CoreDeviceCanConfigStatus::Unsupported => Self::Unsupported,
            CoreDeviceCanConfigStatus::ReadFailed { reason } => Self::ReadFailed {
                reason: reason.clone(),
            },
        }
    }
}

/// One explicit, user-triggered settings transaction.
///
/// The expected identity is part of the request so the backend can force-read
/// `0x1018` and reject a device that was unplugged and replaced after the
/// sidebar snapshot was rendered.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSettingsRequestDto {
    pub node_id: u8,
    pub expected_vendor_id: u32,
    pub expected_product_code: u32,
    pub new_node_id: u8,
    pub nominal_bitrate: u32,
    pub data_bitrate: Option<u32>,
    pub transmit_pdo_brs: Option<bool>,
}

impl DeviceSettingsRequestDto {
    pub fn update(self) -> CoreDeviceSettingsUpdate {
        CoreDeviceSettingsUpdate {
            new_node_id: self.new_node_id,
            nominal_bitrate: self.nominal_bitrate,
            data_bitrate: self.data_bitrate,
            transmit_pdo_brs: self.transmit_pdo_brs,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct DeviceSettingsResultDto {
    pub changed: bool,
    pub restart_required: bool,
    pub persistence_pending: bool,
    pub brs_applied_immediately: bool,
}

impl From<CoreDeviceSettingsResult> for DeviceSettingsResultDto {
    fn from(result: CoreDeviceSettingsResult) -> Self {
        Self {
            changed: result.changed,
            restart_required: result.restart_required,
            persistence_pending: result.persistence_pending,
            brs_applied_immediately: result.brs_applied_immediately,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MotorInfoDto {
    pub node_id: u8,
    /// Changes on BootUp and on an online→offline liveness edge. Frontends use
    /// it to reject cached values from an earlier physical device session.
    pub session_epoch: u64,
    pub friendly_name: String,
    pub identity: Option<MotorIdentityDto>,
    pub can_config: DeviceCanConfigStatusDto,
    pub lifecycle: MotorLifecycleDto,
    pub online: bool,
    pub logic: Option<LogicDto>,
    pub nmt_state: Option<NmtStateDto>,
    /// `true` iff the motor is in a state where `set_mode` / `set_target`
    /// will be accepted (`Initialized` && `online`).
    pub is_ready: bool,
    /// `true` iff `initialize` is meaningful right now (lifecycle is
    /// `Identified` or `NeedsReinit`).
    pub can_initialize: bool,
    /// Peak torque (Nm) read from `0x6076` during init. Lets the UI render
    /// the `0x6072` permille input as an approximate Nm value. `None` until
    /// initialized (or if the motor doesn't expose it).
    pub peak_torque_nm: Option<f32>,
    /// Host device kind resolved from the exact `0x1018` tuple via the GUI's
    /// registry: `"cia402_motor"`, `"meow_motor"`, `"imu"`, `"lift"`, or
    /// `"unknown"`. New and unknown tuples never fall through to legacy motor
    /// controls.
    pub device_type: String,
}

impl From<&CoreMotorInfo> for MotorInfoDto {
    fn from(m: &CoreMotorInfo) -> Self {
        let lifecycle_allows_init = matches!(
            m.lifecycle,
            CoreMotorLifecycle::Identified | CoreMotorLifecycle::NeedsReinit { .. }
        );
        // Resolve the host device kind from the exact 0x1018 tuple.
        let device_kind = match &m.identity {
            Some(id) => crate::device_registry::classify(id.vendor_id, id.product_code),
            None => crate::device_registry::DeviceKind::Unknown,
        };
        let can_initialize = device_kind.supports_cia402_controls() && lifecycle_allows_init;
        Self {
            node_id: m.node_id,
            session_epoch: m.session_epoch,
            friendly_name: m
                .identity
                .as_ref()
                .and_then(|identity| {
                    crate::device_registry::display_name(identity.vendor_id, identity.product_code)
                })
                .map(str::to_owned)
                .unwrap_or_else(|| m.friendly_name()),
            identity: m.identity.as_ref().map(MotorIdentityDto::from),
            can_config: (&m.can_config).into(),
            lifecycle: (&m.lifecycle).into(),
            online: m.online,
            logic: m.logic.as_ref().map(LogicDto::from),
            nmt_state: m.nmt_state.map(NmtStateDto::from),
            is_ready: device_kind.supports_cia402_controls() && m.is_ready(),
            can_initialize,
            peak_torque_nm: m.peak_torque_nm,
            device_type: device_kind.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MeowMotorSnapshotDto {
    pub node_id: u8,
    pub session_epoch: u64,
    pub friendly_name: String,
    pub identity: Option<MotorIdentityDto>,
    pub can_config: DeviceCanConfigStatusDto,
    pub lifecycle: MeowMotorLifecycleDto,
    pub online: bool,
    pub nmt_state: Option<NmtStateDto>,
    pub logic: Option<LogicDto>,
    pub is_ready: bool,
    pub peak_torque_nm: Option<f32>,
    pub mit_kp_kd_factor: Option<f32>,
    pub measurements: MeowMotorMeasurementsDto,
}

impl MeowMotorSnapshotDto {
    pub fn new(info: &CoreMeowMotorInfo, live: &CoreMeowLiveState) -> Self {
        Self {
            node_id: info.node_id,
            session_epoch: info.session_epoch,
            friendly_name: info.friendly_name(),
            identity: info.identity.as_ref().map(MotorIdentityDto::from),
            can_config: (&info.can_settings).into(),
            lifecycle: (&info.lifecycle).into(),
            online: info.online,
            nmt_state: info.nmt_state.map(NmtStateDto::from),
            logic: info.logic.as_ref().map(LogicDto::from),
            is_ready: info.is_ready(),
            peak_torque_nm: info.peak_torque_nm,
            mit_kp_kd_factor: info.mit_kp_kd_factor,
            measurements: (&live.measurements).into(),
        }
    }
}

impl From<&CoreMeowCanSettingsStatus> for DeviceCanConfigStatusDto {
    fn from(status: &CoreMeowCanSettingsStatus) -> Self {
        match status {
            CoreMeowCanSettingsStatus::Pending => Self::Pending,
            CoreMeowCanSettingsStatus::Available(config) => Self::Available {
                config: DeviceCanConfigDto {
                    stored_node_id: config.node_id,
                    nominal_bitrate: config.nominal_bitrate,
                    data_bitrate: Some(config.data_bitrate),
                    transmit_pdo_brs: Some(config.transmit_pdo_brs),
                },
            },
            CoreMeowCanSettingsStatus::ReadFailed { reason } => Self::ReadFailed {
                reason: reason.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum MeowMotorLifecycleDto {
    Unknown,
    UnsupportedIdentity,
    Identified,
    Initializing,
    Initialized,
    NeedsReinit { reason: String },
    NeedsRestart,
}

impl From<&CoreMeowLifecycle> for MeowMotorLifecycleDto {
    fn from(lifecycle: &CoreMeowLifecycle) -> Self {
        match lifecycle {
            CoreMeowLifecycle::Unknown => Self::Unknown,
            CoreMeowLifecycle::UnsupportedIdentity => Self::UnsupportedIdentity,
            CoreMeowLifecycle::Identified => Self::Identified,
            CoreMeowLifecycle::Initializing => Self::Initializing,
            CoreMeowLifecycle::Initialized => Self::Initialized,
            CoreMeowLifecycle::NeedsReinit { reason } => Self::NeedsReinit {
                reason: match reason {
                    CoreMeowReinitReason::LeftOperational => "LeftOperational",
                    CoreMeowReinitReason::TelemetryStale => "TelemetryStale",
                }
                .into(),
            },
            CoreMeowLifecycle::NeedsRestart => Self::NeedsRestart,
        }
    }
}

impl From<&CoreMeowLogic> for LogicDto {
    fn from(logic: &CoreMeowLogic) -> Self {
        match logic {
            CoreMeowLogic::Disabled => Self::Disabled,
            CoreMeowLogic::Enabled(mode) => Self::Enabled {
                mode: (*mode).into(),
            },
            CoreMeowLogic::Error {
                mode_display,
                detailed_error,
            } => Self::Error {
                kind: format!("ModeDisplay0x{mode_display:02X}"),
                raw_code: *detailed_error,
            },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MeowMotorMeasurementsDto {
    pub position_rev: Option<f64>,
    pub accumulated_position_rev: Option<f64>,
    pub accumulation_valid: bool,
    pub accumulation_segment: u64,
    pub velocity_rev_per_s: Option<f64>,
    pub torque_permille: Option<i16>,
    pub driver_temp_c: Option<f32>,
    pub motor_temp_c: Option<f32>,
    pub mode_display: Option<u8>,
    pub detailed_error: Option<u16>,
    pub timestamp_us: Option<u16>,
    pub tpdo1_generation: u64,
    pub tpdo2_generation: u64,
}

impl From<&hex_motor::meow_motor::MeowMotorMeasurements> for MeowMotorMeasurementsDto {
    fn from(measurements: &hex_motor::meow_motor::MeowMotorMeasurements) -> Self {
        Self {
            position_rev: measurements
                .position
                .map(|position| position.to_revolutions()),
            accumulated_position_rev: measurements.accumulated_position_rev,
            accumulation_valid: measurements.position_accumulation_valid,
            accumulation_segment: measurements.position_accumulation_segment,
            velocity_rev_per_s: measurements.velocity_rev_per_s,
            torque_permille: measurements.torque_permille,
            driver_temp_c: measurements.driver_temp_c,
            motor_temp_c: measurements.motor_temp_c,
            mode_display: measurements.mode_display,
            detailed_error: measurements.detailed_error,
            timestamp_us: measurements.timestamp_us,
            tpdo1_generation: measurements.tpdo1_generation,
            tpdo2_generation: measurements.tpdo2_generation,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MotorIdentityDto {
    pub node_id: u8,
    pub vendor_id: u32,
    pub product_code: u32,
    pub revision_number: u32,
    pub serial_number: u32,
    pub product_name: Option<String>,
}

impl From<&MotorIdentity> for MotorIdentityDto {
    fn from(id: &MotorIdentity) -> Self {
        Self {
            node_id: id.node_id,
            vendor_id: id.vendor_id,
            product_code: id.product_code,
            revision_number: id.revision_number,
            serial_number: id.serial_number,
            product_name: id.product_name.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum MotorLifecycleDto {
    Unknown,
    Identified,
    Initializing,
    Initialized,
    NeedsReinit { reason: String },
}

impl From<&CoreMotorLifecycle> for MotorLifecycleDto {
    fn from(l: &CoreMotorLifecycle) -> Self {
        match l {
            CoreMotorLifecycle::Unknown => Self::Unknown,
            CoreMotorLifecycle::Identified => Self::Identified,
            CoreMotorLifecycle::Initializing => Self::Initializing,
            CoreMotorLifecycle::Initialized => Self::Initialized,
            CoreMotorLifecycle::NeedsReinit { reason } => Self::NeedsReinit {
                reason: match reason {
                    CoreReinitReason::LeftOperational => "LeftOperational".into(),
                },
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state")]
pub enum LogicDto {
    Disabled,
    Enabled { mode: MotorModeDto },
    Error { kind: String, raw_code: u16 },
}

impl From<&CoreLogic> for LogicDto {
    fn from(l: &CoreLogic) -> Self {
        match l {
            CoreLogic::Disabled => Self::Disabled,
            CoreLogic::Enabled(m) => Self::Enabled { mode: (*m).into() },
            CoreLogic::Error { kind, raw_code } => Self::Error {
                kind: motor_error_kind_name(*kind).into(),
                raw_code: *raw_code,
            },
        }
    }
}

fn motor_error_kind_name(k: MotorErrorKind) -> &'static str {
    match k {
        MotorErrorKind::OverCurrent => "OverCurrent",
        MotorErrorKind::OverVoltage => "OverVoltage",
        MotorErrorKind::UnderVoltage => "UnderVoltage",
        MotorErrorKind::DriverOverTemp => "DriverOverTemp",
        MotorErrorKind::MotorOverTemp => "MotorOverTemp",
        MotorErrorKind::HeartbeatLost => "HeartbeatLost",
        MotorErrorKind::EncoderError => "EncoderError",
        MotorErrorKind::HallError => "HallError",
        MotorErrorKind::MotorStall => "MotorStall",
        MotorErrorKind::StartupDifficult => "StartupDifficult",
        MotorErrorKind::VelocityError => "VelocityError",
        MotorErrorKind::PositionError => "PositionError",
        MotorErrorKind::Other => "Other",
    }
}

/// Modes are exposed as plain string variants so the JS side can store the
/// raw value of a `<select>` directly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MotorModeDto {
    ProfilePosition,
    ProfileVelocity,
    Torque,
    Mit,
}

impl From<MotorMode> for MotorModeDto {
    fn from(m: MotorMode) -> Self {
        match m {
            MotorMode::ProfilePosition => Self::ProfilePosition,
            MotorMode::ProfileVelocity => Self::ProfileVelocity,
            MotorMode::Torque => Self::Torque,
            MotorMode::Mit => Self::Mit,
        }
    }
}

impl From<hex_motor::meow_motor::MeowMotorMode> for MotorModeDto {
    fn from(mode: hex_motor::meow_motor::MeowMotorMode) -> Self {
        match mode {
            hex_motor::meow_motor::MeowMotorMode::ProfilePosition => Self::ProfilePosition,
            hex_motor::meow_motor::MeowMotorMode::ProfileVelocity => Self::ProfileVelocity,
            hex_motor::meow_motor::MeowMotorMode::Torque => Self::Torque,
            hex_motor::meow_motor::MeowMotorMode::Mit => Self::Mit,
        }
    }
}

impl From<MotorModeDto> for hex_motor::meow_motor::MeowMotorMode {
    fn from(mode: MotorModeDto) -> Self {
        match mode {
            MotorModeDto::ProfilePosition => Self::ProfilePosition,
            MotorModeDto::ProfileVelocity => Self::ProfileVelocity,
            MotorModeDto::Torque => Self::Torque,
            MotorModeDto::Mit => Self::Mit,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum MeowMotorTargetDto {
    ProfilePosition {
        position_rev: f64,
    },
    ProfileVelocity {
        velocity_rev_per_s: f32,
    },
    Torque {
        torque_permille: i16,
    },
    Mit {
        position_rev: f32,
        velocity_rev_per_s: f32,
        torque_nm: f32,
        kp: u16,
        kd: u16,
        kp_kd_limit_permille: u16,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeowProfileLimitsDto {
    pub velocity_rev_per_s: f32,
    pub acceleration_rev_per_s2: f32,
    pub deceleration_rev_per_s2: f32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeowCanSettingsRequestDto {
    pub node_id: u8,
    pub nominal_bitrate: u32,
    pub data_bitrate: u32,
    pub transmit_pdo_brs: bool,
}

impl From<MotorModeDto> for MotorMode {
    fn from(m: MotorModeDto) -> Self {
        match m {
            MotorModeDto::ProfilePosition => MotorMode::ProfilePosition,
            MotorModeDto::ProfileVelocity => MotorMode::ProfileVelocity,
            MotorModeDto::Torque => MotorMode::Torque,
            MotorModeDto::Mit => MotorMode::Mit,
        }
    }
}

/// Internally-tagged so JS sends `{"kind":"Velocity","rev_per_s":0.3}` etc.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum MotorTargetDto {
    Disable,
    Position {
        rev: f32,
    },
    Velocity {
        rev_per_s: f32,
    },
    Torque {
        nm: f32,
    },
    Mit {
        pos: f32,
        vel: f32,
        tor: f32,
        kp: f32,
        kd: f32,
    },
}

impl From<MotorTargetDto> for MotorTarget {
    fn from(t: MotorTargetDto) -> Self {
        match t {
            MotorTargetDto::Disable => MotorTarget::Disable,
            MotorTargetDto::Position { rev } => MotorTarget::Position { rev },
            MotorTargetDto::Velocity { rev_per_s } => MotorTarget::Velocity { rev_per_s },
            MotorTargetDto::Torque { nm } => MotorTarget::Torque { nm },
            MotorTargetDto::Mit {
                pos,
                vel,
                tor,
                kp,
                kd,
            } => MotorTarget::Mit {
                pos,
                vel,
                tor,
                kp,
                kd,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum NmtStateDto {
    BootUp,
    Stopped,
    Operational,
    PreOperational,
}

impl From<NmtState> for NmtStateDto {
    fn from(s: NmtState) -> Self {
        match s {
            NmtState::BootUp => Self::BootUp,
            NmtState::Stopped => Self::Stopped,
            NmtState::Operational => Self::Operational,
            NmtState::PreOperational => Self::PreOperational,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveStateDto {
    pub connection: ConnectionDto,
    pub logic: Option<LogicDto>,
    pub measurements: MeasurementsDto,
}

impl From<&CoreLiveState> for LiveStateDto {
    fn from(s: &CoreLiveState) -> Self {
        Self {
            connection: (&s.connection).into(),
            logic: s.logic.as_ref().map(LogicDto::from),
            measurements: (&s.measurements).into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionDto {
    pub online: bool,
    pub nmt_state: Option<NmtStateDto>,
    /// `Instant`s aren't serializable; we surface only "has it ever arrived"
    /// as a boolean for the UI's purposes.
    pub has_heartbeat: bool,
    pub has_tpdo: bool,
}

impl From<&CoreConnection> for ConnectionDto {
    fn from(c: &CoreConnection) -> Self {
        Self {
            online: c.online,
            nmt_state: c.nmt_state.map(NmtStateDto::from),
            has_heartbeat: c.last_heartbeat.is_some(),
            has_tpdo: c.last_tpdo.is_some(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MeasurementsDto {
    pub position_rev: Option<f32>,
    pub velocity_rev_per_s: Option<f32>,
    pub torque_nm: Option<f32>,
    pub driver_temp_c: Option<f32>,
    pub motor_temp_c: Option<f32>,
    pub status_word: Option<u16>,
    pub mode_display: Option<u8>,
    pub error_register: Option<u8>,
    /// Motor's `0x1013` high-res timestamp in µs (wraps ~every 71 min).
    pub timestamp_us: Option<u32>,
}

impl From<&CoreMeasurements> for MeasurementsDto {
    fn from(m: &CoreMeasurements) -> Self {
        Self {
            position_rev: m.position_rev,
            velocity_rev_per_s: m.velocity_rev_per_s,
            torque_nm: m.torque_nm,
            driver_temp_c: m.driver_temp_c,
            motor_temp_c: m.motor_temp_c,
            status_word: m.status_word,
            mode_display: m.mode_display,
            error_register: m.error_register,
            timestamp_us: m.timestamp_us,
        }
    }
}
