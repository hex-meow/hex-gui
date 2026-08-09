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

export interface AuthenticityIdentity {
  vendor_id: number;
  product_code: number;
  revision_number: number;
  serial_number: number;
}

export interface AuthenticityTarget {
  nodeId: number;
  sessionEpoch: number;
}

export interface AuthenticityDeviceView {
  node_id: number;
  session_epoch: number;
  identity: AuthenticityIdentity;
  device_name: string;
  scheme: "meow_motor_token" | "signed_p256";
  local_status: "envelope_valid" | "valid" | "unsupported" | "unprovisioned" | "invalid";
  detail: string;
  signing_key_id: number | null;
  digest_hex: string | null;
  registration_eligible: boolean;
}

export interface AuthenticityOnlineStatus {
  node_id: number;
  session_epoch: number;
  status: "unknown" | "issued_unregistered" | "registered" | "revoked" | "invalid";
}

export interface AuthenticityRegistrationResult {
  status: "registered" | "already_registered";
  device_count: number;
}

export interface CalibrationRawWord {
  subindex: number;
  value_u32: number;
  value_hex: string;
}

export interface CalibrationFrictionPayload {
  static_pos_raw_nm: number;
  static_neg_raw_nm: number;
  kinetic_pos_raw_nm: number;
  kinetic_neg_raw_nm: number;
  reference_speed_rad_per_s: number;
  calibration_temperature_c: number;
}

export interface CalibrationPayload {
  torque_factor: number;
  torque_fit_rmse_nm: number;
  friction: CalibrationFrictionPayload | null;
}

export interface CalibrationSource {
  vendor_id: number;
  product_code: number;
  revision_number: number;
  serial_number: number;
}

export interface CalibrationUpdatePrepared {
  node_id: number;
  session_epoch: number;
  identity: AuthenticityIdentity;
  online_status: "issued_unregistered" | "registered";
  token_decimal: string;
  token_hex: string;
  highest_subindex: number;
  backup_words: CalibrationRawWord[];
  current_calibration: CalibrationPayload;
}

export interface CalibrationUpdatePreviewRequest {
  target: AuthenticityTarget;
  torqueJson: string;
  frictionJson: string | null;
}

export interface CalibrationUpdatePreview {
  preview_id: string;
  node_id: number;
  identity: AuthenticityIdentity;
  token_decimal: string;
  token_hex: string;
  torque_source: CalibrationSource;
  friction_source: CalibrationSource | null;
  requested: CalibrationPayload;
  quantized: CalibrationPayload;
  new_words: CalibrationRawWord[];
  warnings: string[];
}

export interface CalibrationUpdateWriteRequest {
  target: AuthenticityTarget;
  previewId: string;
  backupAcknowledged: boolean;
}

export interface CalibrationUpdateWriteResult {
  node_id: number;
  identity: AuthenticityIdentity;
  preview_id: string;
  written_words: CalibrationRawWord[];
  ram_readback_confirmed: boolean;
  power_cycle_required: boolean;
}

export interface CalibrationUpdateVerifyRequest {
  target: AuthenticityTarget;
  previewId: string;
}

export interface CalibrationUpdatePersistedResult {
  node_id: number;
  session_epoch: number;
  identity: AuthenticityIdentity;
  preview_id: string;
  online_status: "issued_unregistered" | "registered";
  persisted_words: CalibrationRawWord[];
}

export interface CanBitTiming {
  bitrate: number | null;
  /** Per-mille: 800 means a 0.800 sample point. */
  sample_point_per_mille: number | null;
}

export interface ConnectionInfo {
  backend: "socketcan" | "gs_usb";
  fd_enabled: boolean | null;
  nominal: CanBitTiming | null;
  data: CanBitTiming | null;
  /** Read-only inspection failure; the underlying connection remains open. */
  inspection_error: string | null;
}

export interface DeviceCanConfig {
  /** Node-ID stored in nonvolatile config; it may differ from the active ID. */
  stored_node_id: number;
  nominal_bitrate: number;
  /** null means the device explicitly reported Classic CAN only. */
  data_bitrate: number | null;
  /** null means not applicable (for example, a Classic-only device). */
  transmit_pdo_brs: boolean | null;
}

export type DeviceCanConfigStatus =
  | { status: "pending" }
  | { status: "available"; config: DeviceCanConfig }
  | { status: "unsupported" }
  | { status: "read_failed"; reason: string };

export interface DeviceSettingsRequest {
  node_id: number;
  expected_vendor_id: number;
  expected_product_code: number;
  new_node_id: number;
  nominal_bitrate: number;
  data_bitrate: number | null;
  transmit_pdo_brs: boolean | null;
}

export interface DeviceSettingsResult {
  changed: boolean;
  restart_required: boolean;
  persistence_pending: boolean;
  brs_applied_immediately: boolean;
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
  /** Device-session generation, changed on BootUp and online→offline edges. */
  session_epoch: number;
  friendly_name: string;
  identity: MotorIdentity | null;
  can_config: DeviceCanConfigStatus;
  lifecycle: Lifecycle;
  online: boolean;
  logic: Logic | null;
  nmt_state: NmtState | null;
  is_ready: boolean;
  can_initialize: boolean;
  peak_torque_nm: number | null;
  /** Host device kind from the exact 0x1018 identity tuple. */
  device_type:
    | "unknown"
    | "cia402_motor"
    | "meow_motor"
    | "imu"
    | "lift";
}

export type MeowMotorLifecycle =
  | { kind: "Unknown" }
  | { kind: "UnsupportedIdentity" }
  | { kind: "Identified" }
  | { kind: "Initializing" }
  | { kind: "Initialized" }
  | { kind: "NeedsReinit"; reason: string }
  | { kind: "NeedsRestart" };

export interface MeowMotorMeasurements {
  position_rev: number | null;
  accumulated_position_rev: number | null;
  accumulation_valid: boolean;
  accumulation_segment: number;
  velocity_rev_per_s: number | null;
  torque_permille: number | null;
  driver_temp_c: number | null;
  motor_temp_c: number | null;
  mode_display: number | null;
  detailed_error: number | null;
  timestamp_us: number | null;
  tpdo1_generation: number;
  tpdo2_generation: number;
}

export interface MeowMotorSnapshot {
  node_id: number;
  session_epoch: number;
  friendly_name: string;
  identity: MotorIdentity | null;
  can_config: DeviceCanConfigStatus;
  lifecycle: MeowMotorLifecycle;
  online: boolean;
  nmt_state: NmtState | null;
  logic: Logic | null;
  is_ready: boolean;
  peak_torque_nm: number | null;
  mit_kp_kd_factor: number | null;
  measurements: MeowMotorMeasurements;
}

export type MeowMotorTarget =
  | { kind: "ProfilePosition"; position_rev: number }
  | { kind: "ProfileVelocity"; velocity_rev_per_s: number }
  | { kind: "Torque"; torque_permille: number }
  | {
      kind: "Mit";
      position_rev: number;
      velocity_rev_per_s: number;
      torque_nm: number;
      kp: number;
      kd: number;
      kp_kd_limit_permille: number;
    };

export interface MeowProfileLimits {
  velocity_rev_per_s: number;
  acceleration_rev_per_s2: number;
  deceleration_rev_per_s2: number;
}

export interface MeowCanSettingsRequest {
  node_id: number;
  nominal_bitrate: number;
  data_bitrate: number;
  transmit_pdo_brs: boolean;
}

export interface FrictionCalibrationRequest {
  node_id: number;
  expected_vendor_id: number;
  expected_product_code: number;
  expected_revision_number: number;
  expected_serial_number: number;
  torque_step_permille: number;
  max_torque_permille: number;
  step_dwell_ms: number;
  movement_position_threshold_rad: number;
  movement_velocity_threshold_rad_per_s: number;
  kinetic_sample_ms: number;
}

export interface FrictionCalibrationResult {
  node_id: number;
  vendor_id: number;
  product_code: number;
  revision_number: number;
  serial_number: number;
  peak_torque_nm: number;
  static_pos_raw_nm: number;
  static_neg_raw_nm: number;
  kinetic_pos_raw_nm: number;
  kinetic_neg_raw_nm: number;
  static_pos_permille: number;
  static_neg_permille: number;
  kinetic_pos_mean_permille: number;
  kinetic_neg_mean_permille: number;
  kinetic_pos_stddev_permille: number;
  kinetic_neg_stddev_permille: number;
  kinetic_reference_speed_rad_per_s: number;
  kinetic_pos_mean_speed_rad_per_s: number;
  kinetic_neg_mean_speed_rad_per_s: number;
  calibration_temperature_c: number;
}

export interface FrictionCalibrationView {
  running: boolean;
  phase: string;
  progress_percent: number;
  node_id: number | null;
  current_command_permille: number;
  position_rad: number | null;
  velocity_rad_per_s: number | null;
  actual_torque_permille: number | null;
  motor_temperature_c: number | null;
  result: FrictionCalibrationResult | null;
  error: string | null;
  cleanup_warning: string | null;
}

export interface TorqueCalibrationRequest {
  node_id: number;
  expected_vendor_id: number;
  expected_product_code: number;
  expected_revision_number: number;
  expected_serial_number: number;
  mass_kg: number;
  center_distance_m: number;
  sweep_speed_rad_per_s: number;
  sweep_acceleration_rad_per_s2: number;
  sweep_cycles: number;
  controller_kp_nm_per_rad: number;
  controller_kd_nm_s_per_rad: number;
  max_torque_permille: number;
}

export interface TorqueFitPoint {
  angle_deg: number;
  gravity_torque_nm: number;
  forward_raw_nm: number;
  reverse_raw_nm: number;
  midpoint_raw_nm: number;
  fitted_raw_nm: number;
  friction_half_difference_raw_nm: number;
  corrected_residual_nm: number;
  forward_stddev_raw_nm: number;
  reverse_stddev_raw_nm: number;
  forward_samples: number;
  reverse_samples: number;
}

export interface TorquePassSummary {
  cycle: number;
  direction: "forward" | "reverse";
  accepted_samples: number;
  rejected_samples: number;
  mean_velocity_rad_per_s: number;
  velocity_stddev_rad_per_s: number;
  peak_absolute_velocity_rad_per_s: number;
  peak_tracking_error_deg: number;
  minimum_raw_torque_nm: number;
  maximum_raw_torque_nm: number;
}

export interface TorqueCalibrationResult {
  node_id: number;
  vendor_id: number;
  product_code: number;
  revision_number: number;
  serial_number: number;
  mass_kg: number;
  center_distance_m: number;
  standard_gravity_m_per_s2: number;
  maximum_gravity_torque_nm: number;
  peak_torque_nm: number;
  torque_factor: number;
  torque_fit_rmse_nm: number;
  positive_torque_factor: number;
  negative_torque_factor: number;
  directional_asymmetry_percent: number;
  mean_hysteresis_half_width_raw_nm: number;
  forward_friction_offset_raw_nm: number;
  reverse_friction_offset_raw_nm: number;
  calibration_temperature_c: number;
  zero_position_raw: number;
  sweep_endpoint_deg: number;
  fit_angle_limit_deg: number;
  sweep_speed_rad_per_s: number;
  sweep_acceleration_rad_per_s2: number;
  sweep_cycles: number;
  control_rate_hz: number;
  controller_kp_nm_per_rad: number;
  controller_kd_nm_s_per_rad: number;
  max_torque_permille: number;
  accepted_sample_count: number;
  rejected_sample_count: number;
  pass_summaries: TorquePassSummary[];
  fit_points: TorqueFitPoint[];
}

export interface TorqueCalibrationView {
  running: boolean;
  acceptance_active: boolean;
  traffic_active: boolean;
  phase: string;
  progress_percent: number;
  node_id: number | null;
  current_command_permille: number;
  current_command_nm: number;
  angle_deg: number | null;
  target_angle_deg: number | null;
  trajectory_angle_deg: number | null;
  trajectory_velocity_rad_per_s: number | null;
  tracking_error_deg: number | null;
  velocity_rad_per_s: number | null;
  acceleration_rad_per_s2: number | null;
  actual_torque_permille: number | null;
  actual_torque_nm: number | null;
  motor_temperature_c: number | null;
  current_pass: number;
  total_passes: number;
  accepted_samples: number;
  rejected_samples: number;
  sample_valid: boolean;
  sample_rejection_reason: string | null;
  result: TorqueCalibrationResult | null;
  error: string | null;
  cleanup_warning: string | null;
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

export interface LiftFactoryCalibrationView {
  available: boolean;
  abi: number;
  state: number;
  flags: number;
  lower_count: number;
  upper_count: number;
}

export interface LiftFactoryCalibrationResult {
  lower_count: number;
  upper_count: number;
  travel_m: number;
  counts_per_meter: number;
  transmission_correction: number;
  crc32: number;
}

// ── Lift raw-CAN application (mirrors lift::LiftState) ──
export interface LiftInaDiagnosticsView {
  ina_error: number;
  ina_transport_error: number;
  diag_alert: number;
  fault_count: number;
  fingerprint_mismatch: number;
  last_attempt_age_ms: number;
  last_success_age_ms: number;
  consecutive_good: number;
  consecutive_errors: number;
  last_error: number;
  last_transport_error: number;
  last_fingerprint_mismatch: number;
  last_error_age_ms: number;
}

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
  ina_diagnostics: LiftInaDiagnosticsView;
  // 0x4600 effective parameters (v0.4: firmware-derived soft limits + scale).
  counts_per_meter: number;
  position_min_m: number;
  position_max_m: number;
  velocity_max_mps: number;
  velocity_min_mps: number;
  commissioning: LiftCommissionView;
  factory_calibration: LiftFactoryCalibrationView;
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
  fatal: boolean; // RobotStatus.mode == FATAL_ERROR (latched robot fault; see Events for cause)
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
  fatal: boolean; // RobotStatus.mode == FATAL_ERROR (latched robot fault; see Events for cause)
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

/** 发现到的一台升降(Lift(Zenoh) app;区别于直连 CAN 的 LiftState)。 */
export interface LiftRobotInfo {
  prefix: string;
  model: string;
  dof: number;
  joint_names: string[];
  pos_min: number[];
  pos_max: number[];
  vel_max: number[];
  vel_min: number[];
  needs_homing: boolean[];
  command_modes: number[]; // 1=POSITION 2=VELOCITY 3=TRAJECTORY(力控型号,当前无)
  payload_max_kg: number | null;
}

/** Lift(Zenoh)面板状态快照。 */
export interface ZenohLiftState {
  connected: boolean;
  controlling: boolean;
  holder: number;
  mode: string;       // DISABLED/ACTIVE/FAULT/CALIBRATING
  robot_mode: string; // STANDBY/RUNNING/OVERTAKEN/FATAL_ERROR
  model: string;
  prefix: string;

  height: number;     // 当前高度 m(未 homing 时设备严格报 0)
  pos_min: number;
  pos_max: number;
  vel_max: number;
  vel_min: number;    // 速度释放死区:jog 下限取它,更小的值设备不会动
  payload_max_kg: number | null;

  // LiftStatus 直译
  homed: boolean;
  config_valid: boolean;   // false ⇒ 设备 fail-closed 拒绝一切运动
  target_reached: boolean; // 自主 goal 无回执,这是判完成的唯一途径
  moving: boolean;
  output_limited: boolean;
  at_lower_limit: boolean;
  at_upper_limit: boolean;
  estop: boolean;
  fault_code: number;
  fault_text: string;

  // 能力声明(决定禁用哪些控件)
  can_position: boolean;
  can_velocity: boolean;
  guarded_contact_supported: boolean;

  homing: boolean;
  fatal: boolean;
  last_error: string | null;
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

// ── Controller HAL (RobotConsole read-only hardware view) ──
export interface HardwareField {
  name: string;
  value: string;
}

export interface HardwareResource {
  id: string;
  kind: string;
  model: string;
  key: string;
  alive: boolean;
  sample_age_ms: number | null;
  sample_bytes: number | null;
  header_present: boolean | null;
  seq: string | null;
  stamp_ns: string | null;
  sync_ns: string | null;
  fields: HardwareField[];
  decode_error: string | null;
}

export interface HardwareController {
  controller_id: string;
  reported_controller_ids: string[];
  supervisor_versions: string[];
  info_reply_count: number;
  resources: HardwareResource[];
  warnings: string[];
}

export interface HardwareSnapshot {
  controllers: HardwareController[];
  errors: string[];
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
