// TS mirrors of the serde DTOs in src-tauri/src/dto.rs.

export type MotorMode =
  | "ProfilePosition"
  | "ProfileVelocity"
  | "Torque"
  | "Mit";

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
  bus_power_w: number;
  ina_temperature_c: number;
  encoder_count: number;
  duty_command_permille: number;
  control_loop_hz: number;
  counts_per_meter: number;
  position_min_m: number;
  position_max_m: number;
  velocity_max_mps: number;
  acceleration_limit_mps2: number;
  homing_speed_mps: number;
  homing_stall_bus_current_a: number;
  homing_stall_speed_mps: number;
  homing_arm_time_ms: number;
  homing_confirm_time_ms: number;
  homing_timeout_ms: number;
  velocity_watchdog_time_ms: number;
  position_tolerance_m: number;
  max_bus_current_a: number;
  bus_voltage_min_v: number;
  bus_voltage_max_v: number;
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
