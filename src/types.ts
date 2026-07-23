// TS mirrors of the serde DTOs in src-tauri/src/dto.rs.

export type MotorMode =
  | "ProfilePosition"
  | "ProfileVelocity"
  | "Torque"
  | "Mit";

// DAMIAO DM-J4310-2EC V1.1 direct-CAN protocol (not CiA 402).
export type DamiaoMode = "Mit" | "PositionVelocity" | "Velocity";

export interface DamiaoConfig {
  motor_id: number;
  master_id: number;
  mode: DamiaoMode;
  p_max: number;
  v_max: number;
  t_max: number;
}

export interface DamiaoDiscoveredDevice {
  motor_id: number;
  feedback_can_id: number | null;
  online: boolean;
  attached: boolean;
  status_code: number;
  status: string;
  feedback_age_ms: number | null;
  rx_count: number;
}

export type DamiaoTarget =
  | {
      kind: "Mit";
      position_rad: number;
      velocity_rad_s: number;
      torque_nm: number;
      kp: number;
      kd: number;
    }
  | {
      kind: "PositionVelocity";
      position_rad: number;
      velocity_rad_s: number;
    }
  | { kind: "Velocity"; velocity_rad_s: number };

export interface DamiaoState {
  attached: boolean;
  motor_id: number;
  master_id: number;
  feedback_can_id: number | null;
  mode: DamiaoMode;
  p_max: number;
  v_max: number;
  t_max: number;
  online: boolean;
  enabled: boolean;
  streaming: boolean;
  status_code: number;
  status: string;
  position_rad: number | null;
  velocity_rad_s: number | null;
  torque_nm: number | null;
  mos_temp_c: number | null;
  rotor_temp_c: number | null;
  feedback_age_ms: number | null;
  feedback_rate_hz: number | null;
  rx_count: number;
  last_error: string | null;
}

// Unit RollerCAN stock motor-control firmware (CAN 2.0B, 29-bit, 1 Mbps).
// The independent RollerCAN SmartKnob firmware remains represented by the
// SmartKnob types further below.
export type RollerCanControlMode = "Speed" | "Position" | "Current" | "Encoder";

export type RollerCanControlTarget =
  | { kind: "Speed"; speed_rpm: number }
  | { kind: "Position"; position_deg: number }
  | { kind: "Current"; current_ma: number }
  | { kind: "Encoder"; encoder_count: number };

export interface RollerCanControlDevice {
  node_id: number;
  online: boolean;
  attached: boolean;
  enabled: boolean;
  mode: RollerCanControlMode;
  state_code: number;
  fault_bits: number;
  feedback_age_ms: number | null;
  rx_count: number;
}

export interface RollerCanControlState {
  attached: boolean;
  node_id: number;
  online: boolean;
  enabled: boolean;
  mode: RollerCanControlMode;
  state_code: number;
  fault_bits: number;
  speed_rpm: number | null;
  position_deg: number | null;
  current_ma: number | null;
  voltage_v: number | null;
  temperature_c: number | null;
  encoder_count: number | null;
  position_max_current_ma: number | null;
  speed_max_current_ma: number | null;
  feedback_age_ms: number | null;
  feedback_rate_hz: number | null;
  rx_count: number;
  last_error: string | null;
}

export interface MotorIdentity {
  node_id: number;
  vendor_id: number;
  product_code: number;
  revision_number: number;
  serial_number: number;
  product_name: string | null;
}

export type Lifecycle =
  | { kind: "Unknown" }
  | { kind: "Identified" }
  | { kind: "Initializing" }
  | { kind: "Initialized" }
  | { kind: "NeedsReinit"; reason: string };

export type Logic =
  | { state: "Disabled" }
  | { state: "Enabled"; mode: MotorMode }
  | { state: "Error"; kind: string; raw_code: number };

export type NmtState =
  | "BootUp"
  | "Stopped"
  | "Operational"
  | "PreOperational";

export interface MotorInfo {
  node_id: number;
  friendly_name: string;
  identity: MotorIdentity | null;
  lifecycle: Lifecycle;
  online: boolean;
  logic: Logic | null;
  nmt_state: NmtState | null;
  is_ready: boolean;
  can_initialize: boolean;
  peak_torque_nm: number | null;
  /** Host device kind from the 0x1018 identity: "motor" (default), "imu", … */
  device_type: string;
}

// ── IMU (mirrors imu::ImuState) ──
export interface ImuState {
  node_id: number;
  online: boolean;
  /** Orientation [w, x, y, z], unit quaternion (local→sensor). */
  quaternion: [number, number, number, number];
  /** Acceleration [x, y, z] in g. */
  accel: [number, number, number];
  /** Angular rate [x, y, z] in deg/s. */
  gyro: [number, number, number];
  temp_c: number;
  counter: number;
}

export interface Measurements {
  position_rev: number | null;
  velocity_rev_per_s: number | null;
  torque_nm: number | null;
  driver_temp_c: number | null;
  motor_temp_c: number | null;
  status_word: number | null;
  mode_display: number | null;
  error_register: number | null;
  timestamp_us: number | null;
}

export interface Connection {
  online: boolean;
  nmt_state: NmtState | null;
  has_heartbeat: boolean;
  has_tpdo: boolean;
}

export interface LiveState {
  connection: Connection;
  logic: Logic | null;
  measurements: Measurements;
}

// ── HopeA3 Robot Application (mirrors hopea3::Hopea3State / Hopea3Motor) ──
export interface Hopea3Motor {
  node_id: number;
  online: boolean;
  enabled: boolean;
  target_rev_per_s: number;
  velocity_rev_per_s: number | null;
  torque_nm: number | null;
  max_torque_permille: number;
  driver_temp_c: number | null;
  motor_temp_c: number | null;
  error: string | null;
}

export interface Hopea3InitProgress {
  active: boolean;
  current: number;
  total: number;
  attempt: number;
}

export interface Hopea3State {
  pose_x: number;
  pose_y: number;
  pose_theta: number;
  meas_vx: number;
  meas_vy: number;
  meas_wz: number;
  cmd_vx: number;
  cmd_vy: number;
  cmd_wz: number;
  max_linear: number;
  max_angular: number;
  motors: Hopea3Motor[];
  running: boolean;
}

export interface LiftCommissionView {
  available: boolean;
  abi: number;
  active_session: number;
  boot_epoch: number;
  challenge: number;
  challenge_kind: number;
  expected_pulse_id: number;
  encoder_sign: number;
  ina_fingerprint_mismatch: number;
  epoch_status: number;
  state: number;
  flags: number;
  requested_duty_permille: number;
  applied_duty_permille: number;
  hard_cap_permille: number;
  lease_ms: number;
  max_pulse_ms: number;
  pulse_elapsed_ms: number;
  command_age_ms: number;
  stop_reason: number;
  soft_current_a: number;
  active_pulse: number;
  energized_ms: number;
  foldback_cap_permille: number;
  overcurrent_ms: number;
  gap_remaining_ms: number;
  hard_current_a: number;
  tpdo3_fresh: boolean;
  tpdo4_fresh: boolean;
  pair_fresh: boolean;
  tick: number;
  raw_count: number;
  current_a: number;
  host_remaining_ms: number;
  buffered_samples: number;
  dropped_pairs: number;
}

// ── Lift raw-CAN application (mirrors lift::LiftState) ──
export interface LiftState {
  running: boolean;
  node_id: number;
  online: boolean;
  tpdo1_fresh: boolean;
  tpdo2_fresh: boolean;
  nmt_state: number;
  device_name: string;
  firmware_version: string;
  nameplate_kind: number;
  model: string;
  layout_id: number;
  nameplate_used: number;
  nameplate_crc32: number;
  nameplate_crc_ok: boolean;
  mode_command: number;
  mode_display: number;
  status_word: number;
  detailed_fault: number;
  actual_position_m: number;
  actual_velocity_mps: number;
  sample_timestamp_us: number;
  bus_voltage_v: number;
  bus_current_a: number;
  encoder_count: number;
  duty_command_permille: number;
  sensor_status: number;
  // 0x4600 effective parameters (v0.4: firmware-derived soft limits + scale).
  counts_per_meter: number;
  position_min_m: number;
  position_max_m: number;
  velocity_max_mps: number;
  velocity_min_mps: number;
  commissioning: LiftCommissionView;
  last_error: string | null;
}

// ── SmartKnob Robot Application (mirrors smartknob::KnobConfig / SmartKnobState) ──
export interface KnobConfig {
  position: number;
  min_position: number;
  max_position: number; // max < min => unbounded
  position_width_radians: number;
  detent_strength_unit: number;
  endstop_strength_unit: number;
  snap_point: number;
  snap_point_bias: number;
  detent_positions: number[];
  click_torque_nm: number;
  friction_compensation: number;
  strength_scale: number;
  p_gain: number;
  d_gain: number;
  text: string;
  led_hue: number;
  is_custom: boolean;
}

export interface SmartKnobState {
  running: boolean;
  config_index: number;
  config: KnobConfig | null;
  current_position: number;
  min_position: number;
  max_position: number;
  num_positions: number; // 0 = unbounded
  sub_position_unit: number;
  shaft_angle_rad: number;
  shaft_velocity_rev_per_s: number;
  applied_torque_nm: number;
  measured_torque_nm: number | null;
  at_endstop: boolean;
  node_id: number;
  online: boolean;
  enabled: boolean;
  driver_temp_c: number | null;
  motor_temp_c: number | null;
  error: string | null;
  strength_scale: number;
  torque_limit_nm: number;
  max_torque_permille: number;
  friction_compensation: number;
  click_torque_nm: number;
  p_gain: number;
  d_gain: number;
}

export type SmartKnobKind = "canopen" | "rollercan";
export type SmartKnobControlSide = "host" | "firmware";
export type SmartKnobEffortUnit = "Nm" | "A";

/** Protocol-qualified address. Node IDs may overlap across device kinds. */
export interface SmartKnobTarget {
  kind: SmartKnobKind;
  nodeId: number;
}

export interface SmartKnobDevice {
  target: SmartKnobTarget;
  name: string;
  online: boolean;
  controlSide: SmartKnobControlSide;
  effortUnit: SmartKnobEffortUnit;
}

export interface SmartKnobProfile {
  target: SmartKnobTarget;
  configs: KnobConfig[];
  controlSide: SmartKnobControlSide;
  effortUnit: SmartKnobEffortUnit;
  supportsTemperature: boolean;
  supportsTelemetry: boolean;
  effortLimitMax: number;
  maxOutputPermille: number;
  telemetryEnabled: boolean | null;
  telemetryRateHz: number | null;
}

export interface SmartKnobTuning {
  pGain: number;
  dGain: number;
  strengthScale: number;
  effortLimit: number;
  maxOutputPermille: number;
  frictionCompensation: number;
  clickEffort: number;
}

export interface SmartKnobTelemetry {
  enabled: boolean;
  rateHz: number;
}

export interface SmartKnobStartRequest {
  target: SmartKnobTarget;
  configIndex: number;
  customConfig?: KnobConfig;
  tuning?: SmartKnobTuning;
  telemetry?: SmartKnobTelemetry;
}

/**
 * Unified live state. The legacy snake_case position/config fields remain for
 * compatibility; effort is consumed only through the unit-aware fields below.
 */
export interface UnifiedSmartKnobState extends SmartKnobState {
  target: SmartKnobTarget | null;
  controlSide: SmartKnobControlSide;
  effortUnit: SmartKnobEffortUnit;
  appliedEffort: number;
  measuredEffort: number | null;
  effortLimit: number;
  maxOutputPermille: number;
  telemetryEnabled: boolean | null;
  telemetryRateHz: number | null;
}

// ── Diagnostics (log / events viewing — mirrors diag.rs DTOs) ──
export interface LogLine {
  proc: string;    // publishing process (arm0 / base0 / launcher / imu0…)
  ts_ns: number;   // per-process monotonic ns (ordering only, not cross-process)
  level: string;   // ERROR / WARN / INFO / DEBUG / TRACE (empty if unparsed)
  target: string;
  msg: string;
}

export interface RobotEvent {
  seq: number;      // monotonic, assigned by backend (dedupe / notify watermark)
  severity: number; // 1=INFO 2=WARNING 3=ERROR 4=FATAL
  code: string;     // stable machine code, e.g. "motor_fault_0x8130"
  text: string;     // human-readable
  kv: [string, string][];
  ts_ns: number;
}

export interface EventsSnapshot {
  events: RobotEvent[];
  baseline_seq: number; // only notify for seq >= this (suppresses seeded history)
}

// ── Base(Zenoh) (mirrors zenoh_base::ZenohBaseState / BaseInfo) ──
export interface BaseInfo {
  prefix: string;
  model: string;
}

export interface ZenohBaseState {
  controlling: boolean;
  holder: number;
  running: boolean;
  /** Controller RobotMode name (read-only observe): STANDBY/RUNNING/OVERTAKEN/FATAL_ERROR/"" */
  robot_mode: string;
  /** When OVERTAKEN, the takeover reason (human_readable or OvertakenMode name); "" otherwise. */
  overtaken_reason: string;
  model: string;
  prefix: string;
  pose_x: number;
  pose_y: number;
  pose_theta: number;
  vx: number;
  vy: number;
  wz: number;
  fatal: boolean; // RobotStatus.mode == FATAL_ERROR (latched motor fault/offline)
}

// ── Arm(Zenoh) (mirrors zenoh_arm::ZenohArmState / ArmInfo) ──
export interface ArmInfo {
  prefix: string;
  model: string;
  dof: number;
  has_ee: boolean;
  ee_model: string;
}

export interface ZenohArmState {
  controlling: boolean;
  holder: number;
  mode: string;           // our last-set OperatingMode name (only meaningful while controlling)
  /** Controller RobotMode name (read-only observe): STANDBY/RUNNING/OVERTAKEN/FATAL_ERROR/"" */
  robot_mode: string;
  /** When OVERTAKEN, the takeover reason (human_readable or OvertakenMode name); "" otherwise. */
  overtaken_reason: string;
  model: string;
  prefix: string;
  dof: number;
  joint_names: string[];
  pos_min: number[];
  pos_max: number[];
  q: number[];
  dq: number[];
  tau: number[];
  temp: number[]; // per-joint temperature ℃ (JointState.temp; empty if motors don't report)
  gravity: [number, number, number];
  has_ee: boolean;
  ee_model: string;
  fatal: boolean; // RobotStatus.mode == FATAL_ERROR (latched motor fault/offline)
}

// mirrors zenoh_arm::ArmUrdf —— 供 3D 渲染的 URDF(整机 arm+EE 或臂-only)
export interface ArmUrdf {
  xml: string;
  assembled: boolean; // 含 EE(整机)→ true;臂-only 或回退 → false
  tip_link: string;   // 工具安装 link 名(EE 拼接处)
}

// ── Controller Config(Zenoh) (mirrors zenoh_config.rs DTOs) ──
export interface ApiVersion {
  major: number;
  minor: number;
  patch: number;
}

export interface RobotRef {
  robot_index: string;
  kind: number;
  kind_name: string; // "arm" | "base" | "lift" | "hand" | "unknown"
  model: string;
}

/** A discovered controller (`<cid>/info`). `cid` = key prefix `hexmeow/<controller_id>`. */
export interface ControllerInfo {
  cid: string;
  controller_id: string;
  fw_version: string;
  api_version: ApiVersion | null;
  features: string[];
  robots: RobotRef[];
}

/** `<cid>/config` read: file text + fingerprint + path + recovery flag. */
export interface ConfigGetDto {
  yaml: string;
  sha256: string;
  path: string;
  mtime_unix: number;
  schema_version: ApiVersion | null;
  recovery_mode: boolean;
}

/** A semantic red-line change (mock flip / CAN swap / kind swap / calibration env). */
export interface CriticalChange {
  robot_id: string;
  field: string;
  old: string;
  new: string;
}

export interface ConfigValidateResult {
  ok: boolean;
  errors: string[];
  critical_changes: CriticalChange[];
}

export interface ConfigSetResult {
  ok: boolean;
  errors: string[];
  critical_changes: CriticalChange[];
  sha256: string;
  applied: boolean;
  robots: string[];
}

export interface RestartResult {
  ok: boolean;
  robots: string[];
}

// ── CAN Analyzer (mirrors analyzer.rs DTOs) ──
export interface CanTraceFrame {
  seq: number;
  /** Host receive time (µs since capture start). No hardware timestamp exists. */
  t_us: number;
  id: number;
  extended: boolean;
  kind: "data" | "fd" | "fd_brs" | "remote";
  dlc: number;
  /** Space-separated lower-case hex of the payload ("11 22 aa"). */
  data: string;
  dir: "rx" | "tx";
}

export interface CanAnalyzerStatus {
  capturing: boolean;
  total: number;
  /** Frames dropped by our subscriber queue (GUI backpressure, NOT bus health). */
  our_dropped: number;
  distinct_ids: number;
  agg_overflow: number;
  ring_len: number;
  next_seq: number;
  fd: boolean;
  max_dlen: number;
  /** Trace times come from the device's hardware clock (gs_usb hw ts). */
  hw_ts: boolean;
}

/** Controller health (analyzer::BusHealthDto). supported=false → render "—". */
export interface CanBusHealth {
  supported: boolean;
  state:
    | "error_active"
    | "error_warning"
    | "error_passive"
    | "bus_off"
    | "stopped"
    | "sleeping"
    | null;
  tx_errors: number | null;
  rx_errors: number | null;
}

export interface CanTraceReply {
  frames: CanTraceFrame[];
  next_seq: number;
  gap: boolean;
  status: CanAnalyzerStatus;
}

export interface CanAggRow {
  id: number;
  extended: boolean;
  count: number;
  rate_hz: number;
  last_dlc: number;
  last_kind: "data" | "fd" | "fd_brs" | "remote";
  last_data: string;
  first_us: number;
  last_us: number;
}

export interface CanAggReply {
  rows: CanAggRow[];
  status: CanAnalyzerStatus;
}

/** Display filter (tagged union the backend deserializes as analyzer::FilterSpec). */
export type CanFilterSpec =
  | { kind: "all" }
  | { kind: "node"; node: number; include_nodeless: boolean }
  | { kind: "mask"; id: number; mask: number; extended: boolean };

/** A frame to transmit (analyzer::SendSpec). */
export interface CanSendSpec {
  id: number;
  extended: boolean;
  fd: boolean;
  brs: boolean;
  rtr: boolean;
  /** Requested DLC for RTR frames (ignored otherwise). */
  dlc: number;
  data: number[];
}

// Tagged target union the backend deserializes (dto::MotorTargetDto).
export type MotorTarget =
  | { kind: "Disable" }
  | { kind: "Position"; rev: number }
  | { kind: "Velocity"; rev_per_s: number }
  | { kind: "Torque"; nm: number }
  | { kind: "Mit"; pos: number; vel: number; tor: number; kp: number; kd: number };

// ── EE(Zenoh)── 镜像 src-tauri/src/zenoh_ee.rs 的 DTO(11-ee-api)。
export interface EeInfo {
  prefix: string;
  model: string;
  dof: number;
  joint_names: string[];
  pos_min: number[];
  pos_max: number[];
  tau_max: number[];
  opening_poly: number[]; // width(q)=Σ poly[i]·q^i;空 = 无宽度映射
  width_max: number;
}

/** 设备树节点(机器人控制台全量发现,所有 kind)。 */
export interface RobotNode {
  prefix: string;
  cid: string;
  robot_index: string;
  kind: number;      // 1=arm 2=base 3=lift 4=ee
  kind_name: string;
  model: string;
}

// ── Controller Wi-Fi (hex-wifi JSON API over the Robot Console Zenoh session) ──
export interface WifiSsid {
  hex: string;
  display: string;
}

export interface WifiStatus {
  state: "unavailable" | "disconnected" | "associating" | "connected" | string;
  connected: WifiSsid | null;
  revision: number;
}

export interface WifiController {
  cid: string;
  status: WifiStatus;
}

export interface WifiScanEntry {
  ssid: WifiSsid;
  signal_dbm: number;
  security: "open" | "wpa2_personal" | "wpa3_personal" | "unknown" | string;
}

export interface WifiSavedNetwork {
  ssid: WifiSsid;
  enabled: boolean;
  connected: boolean;
}

export interface WifiJob {
  job_id: string;
  request_id: string;
  operation: "set" | "forget" | "forget_all" | string;
  state: "queued" | "running" | "succeeded" | "failed" | string;
  revision: number | null;
  error_code: string | null;
  error_message: string | null;
}

export interface ZenohEeState {
  controlling: boolean;
  holder: number;
  mode: string;
  robot_mode: string;
  model: string;
  prefix: string;
  q: number[];
  dq: number[];
  tau: number[];
  grasp_state: string;   // MOVING/AT_POSITION/HOLDING/LOST(设备侧 1kHz 判定)
  estop_behavior: number; // 1=保位 2=松开 3=抗拒张开
  pos_min: number[];
  pos_max: number[];
  opening_poly: number[];
  width_max: number;
  fatal: boolean;
}

/** 场景机器人(M2 常驻 3D;ee_scene 轮询)。 */
export interface SceneRobot {
  prefix: string;
  cid: string;
  robot_index: string;
  kind_name: string;
  model: string;
  joint_names: string[];
  q: number[];
}

export interface ConsoleUrdf {
  xml: string;
  assembled: boolean; // 臂已拼 EE(含 ee_mount)
}

/** 整机挂载边(M3;<cid>/machine 的 DTO,13 §4)。 */
export interface MountEdge {
  parent: string;
  parent_link: string;
  child: string;
  xyz: [number, number, number];
  rpy: [number, number, number];
}
