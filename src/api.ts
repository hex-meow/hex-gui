// Thin typed wrappers over the Tauri commands (src-tauri/src/commands.rs).
// Arg names are camelCase on the JS side; Tauri maps them to the Rust
// snake_case parameters.

import { invoke } from "@tauri-apps/api/core";
import type {
  ArmInfo,
  ArmUrdf,
  BaseInfo,
  CanAggReply,
  CanAnalyzerStatus,
  CanBusHealth,
  CanFilterSpec,
  CanSendSpec,
  CanTraceReply,
  ConfigGetDto,
  ConfigSetResult,
  ConfigValidateResult,
  ConsoleUrdf,
  ControllerInfo,
  EeInfo,
  EventsSnapshot,
  Hopea3InitProgress,
  Hopea3State,
  ImuState,
  KnobConfig,
  LiftState,
  LiveState,
  LogLine,
  MotorInfo,
  MotorMode,
  MotorTarget,
  MountEdge,
  RestartResult,
  RobotNode,
  SceneRobot,
  SmartKnobDevice,
  SmartKnobProfile,
  SmartKnobStartRequest,
  SmartKnobTarget,
  SmartKnobTelemetry,
  SmartKnobTuning,
  UnifiedSmartKnobState,
  ZenohArmState,
  ZenohBaseState,
  ZenohEeState,
} from "./types";

export const api = {
  connect: (iface: string, ourNid: number, broadcastHeartbeat: boolean) =>
    invoke<void>("connect", { iface, ourNid, broadcastHeartbeat }),
  disconnect: () => invoke<void>("disconnect"),
  isConnected: () => invoke<boolean>("is_connected"),

  listDevices: () => invoke<MotorInfo[]>("list_devices"),
  identify: (nid: number) => invoke<void>("identify", { nid }),
  initialize: (nid: number) => invoke<void>("initialize", { nid }),
  initializeAll: () =>
    invoke<[number, string | null][]>("initialize_all"),

  setMode: (nid: number, mode: MotorMode) =>
    invoke<void>("set_mode", { nid, mode }),
  setTarget: (nid: number, target: MotorTarget) =>
    invoke<void>("set_target", { nid, target }),
  setMaxTorque: (nid: number, permille: number) =>
    invoke<void>("set_max_torque", { nid, permille }),
  disable: (nid: number) => invoke<void>("disable", { nid }),
  clearError: (nid: number) => invoke<void>("clear_error", { nid }),
  getStatus: (nid: number) => invoke<LiveState>("get_status", { nid }),

  changeNodeId: (nid: number, newId: number) =>
    invoke<void>("change_node_id", { nid, newId }),
  forgetOffline: () => invoke<void>("forget_offline"),

  setPositionPreset: (nid: number, pos: number) =>
    invoke<void>("set_position_preset", { nid, pos }),
  readPosition: (nid: number) => invoke<number>("read_position", { nid }),

  startLog: (nid: number) => invoke<string>("start_log", { nid }),
  stopLog: (nid: number) => invoke<void>("stop_log", { nid }),

  // HopeA3 Robot Application
  hopea3Start: () => invoke<void>("hopea3_start"),
  hopea3InitProgress: () => invoke<Hopea3InitProgress>("hopea3_init_progress"),
  hopea3Stop: () => invoke<void>("hopea3_stop"),
  hopea3SetCmd: (vx: number, vy: number, wz: number) =>
    invoke<void>("hopea3_set_cmd", { vx, vy, wz }),
  hopea3SetMaxTorque: (permille: number[]) =>
    invoke<void>("hopea3_set_max_torque", { permille }),
  hopea3SetKd: (kdSi: number[]) => invoke<void>("hopea3_set_kd", { kdSi }),
  hopea3SetLimits: (maxLinear: number, maxAngular: number) =>
    invoke<void>("hopea3_set_limits", { maxLinear, maxAngular }),
  hopea3SetAccelLimits: (maxLinAcc: number, maxAngAcc: number) =>
    invoke<void>("hopea3_set_accel_limits", { maxLinAcc, maxAngAcc }),
  hopea3ClearErrors: () => invoke<void>("hopea3_clear_errors"),
  hopea3ReinitMotor: (nid: number) => invoke<void>("hopea3_reinit_motor", { nid }),
  hopea3ResetOdom: () => invoke<void>("hopea3_reset_odom"),
  hopea3GetState: () => invoke<Hopea3State>("hopea3_get_state"),

  // Lift raw-CAN Robot Application
  liftStart: (nid: number) => invoke<LiftState>("lift_start", { nid }),
  liftStop: () => invoke<void>("lift_stop"),
  liftGetState: () => invoke<LiftState>("lift_get_state"),
  liftRefresh: () => invoke<LiftState>("lift_refresh"),
  liftSetNmt: (command: string) => invoke<void>("lift_set_nmt", { command }),
  liftDisable: () => invoke<void>("lift_disable"),
  liftHome: () => invoke<void>("lift_home"),
  liftClearFault: () => invoke<void>("lift_clear_fault"),
  liftSetVelocity: (velocityMps: number) =>
    invoke<void>("lift_set_velocity", { velocityMps }),
  liftRenewVelocity: () => invoke<void>("lift_renew_velocity"),
  liftSetPosition: (positionM: number) =>
    invoke<void>("lift_set_position", { positionM }),
  liftCommissionArm: () => invoke<number>("lift_commission_arm"),
  liftCommissionClearFault: () =>
    invoke<void>("lift_commission_clear_fault"),
  liftCommissionEpochService: (motorDisconnected: boolean) =>
    invoke<void>("lift_commission_epoch_service", { motorDisconnected }),
  liftCommissionHold: (dutyPermille: number) =>
    invoke<number>("lift_commission_hold", { dutyPermille }),
  liftCommissionRenew: () => invoke<void>("lift_commission_renew"),
  liftCommissionRelease: () => invoke<void>("lift_commission_release"),
  liftCommissionDisarm: () => invoke<void>("lift_commission_disarm"),
  liftCommissionEstop: () => invoke<void>("lift_commission_estop"),
  liftCommissionCsv: () => invoke<string>("lift_commission_csv"),

  // SmartKnob Robot Application
  smartknobListDevices: () =>
    invoke<SmartKnobDevice[]>("smartknob_list_devices"),
  smartknobGetProfile: (target: SmartKnobTarget) =>
    invoke<SmartKnobProfile>("smartknob_get_profile", { target }),
  smartknobProbe: (nodeId: number) =>
    invoke<SmartKnobDevice>("smartknob_probe", { nodeId }),
  smartknobStart: (request: SmartKnobStartRequest) =>
    invoke<void>("smartknob_start", { request }),
  smartknobStop: () => invoke<void>("smartknob_stop"),
  smartknobSetConfig: (index: number) =>
    invoke<void>("smartknob_set_config", { index }),
  smartknobSetTuning: (tuning: SmartKnobTuning) =>
    invoke<void>("smartknob_set_tuning", { tuning }),
  smartknobClearError: () => invoke<void>("smartknob_clear_error"),
  smartknobGetState: () =>
    invoke<UnifiedSmartKnobState>("smartknob_get_state"),
  smartknobSetCustomConfig: (config: KnobConfig) =>
    invoke<void>("smartknob_set_custom_config", { config }),
  smartknobSetTelemetry: (telemetry: SmartKnobTelemetry) =>
    invoke<void>("smartknob_set_telemetry", { telemetry }),

  // IMU
  imuStart: (nid: number) => invoke<void>("imu_start", { nid }),
  imuStop: () => invoke<void>("imu_stop"),
  imuGetState: () => invoke<ImuState>("imu_get_state"),
  imuBiasTrim: () => invoke<void>("imu_bias_trim"),
  imuYawReset: () => invoke<void>("imu_yaw_reset"),

  // CAN Analyzer
  analyzerStart: (spec: string, hwTs: boolean) =>
    invoke<void>("analyzer_start", { spec, hwTs }),
  analyzerStop: () => invoke<void>("analyzer_stop"),
  analyzerBusState: () => invoke<CanBusHealth>("analyzer_bus_state"),
  analyzerGetTrace: (afterSeq: number, max: number, filter: CanFilterSpec) =>
    invoke<CanTraceReply>("analyzer_get_trace", { afterSeq, max, filter }),
  analyzerGetAggregates: (filter: CanFilterSpec) =>
    invoke<CanAggReply>("analyzer_get_aggregates", { filter }),
  analyzerGetStatus: () => invoke<CanAnalyzerStatus>("analyzer_get_status"),
  analyzerClear: () => invoke<number>("analyzer_clear"),
  analyzerSend: (spec: CanSendSpec) => invoke<void>("analyzer_send", { spec }),
  // SDO tab (comeow engine over the analyzer's bus). dtype = CiA-309 token
  // ("u16", "x32", "vs", …) or null for raw-hex rendering on reads.
  analyzerSdoRead: (node: number, index: number, sub: number, dtype: string | null, timeoutMs: number, retries: number) =>
    invoke<string>("analyzer_sdo_read", { node, index, sub, dtype, timeoutMs, retries }),
  analyzerSdoWrite: (node: number, index: number, sub: number, dtype: string, value: string, timeoutMs: number, retries: number) =>
    invoke<string>("analyzer_sdo_write", { node, index, sub, dtype, value, timeoutMs, retries }),

  // Base(Zenoh)
  zenohConnect: (connect: string) => invoke<void>("zenoh_connect", { connect }),
  zenohDisconnect: () => invoke<void>("zenoh_disconnect"),
  zenohDiscover: () => invoke<BaseInfo[]>("zenoh_discover"),
  zenohAcquire: (prefix: string, model: string) =>
    invoke<void>("zenoh_acquire", { prefix, model }),
  zenohSetActive: (on: boolean) => invoke<void>("zenoh_set_active", { on }),
  zenohSetCmd: (vx: number, vy: number, wz: number) =>
    invoke<void>("zenoh_set_cmd", { vx, vy, wz }),
  zenohGetState: () => invoke<ZenohBaseState>("zenoh_get_state"),
  zenohRelease: () => invoke<void>("zenoh_release"),
  zenohSetDiagFocus: (prefix: string) => invoke<void>("zenoh_set_diag_focus", { prefix }),
  zenohRefreshDiag: () => invoke<void>("zenoh_refresh_diag"),
  zenohGetEvents: () => invoke<EventsSnapshot>("zenoh_get_events"),
  zenohGetLogs: () => invoke<LogLine[]>("zenoh_get_logs"),
  zenohClearFault: () => invoke<void>("zenoh_clear_fault"),

  // Arm(Zenoh)
  armConnect: (connect: string) => invoke<void>("arm_connect", { connect }),
  armDisconnect: () => invoke<void>("arm_disconnect"),
  armDiscover: () => invoke<ArmInfo[]>("arm_discover"),
  armAcquire: (prefix: string, model: string) => invoke<void>("arm_acquire", { prefix, model }),
  armSetMode: (mode: number) => invoke<void>("arm_set_mode", { mode }),
  armSetGravity: (gravity: [number, number, number]) => invoke<void>("arm_set_gravity", { gravity }),
  armGoto: (q: number[], kp: number, kd: number) => invoke<void>("arm_goto", { q, kp, kd }),
  armGetState: () => invoke<ZenohArmState>("arm_get_state"),
  armGetUrdf: (prefix: string) => invoke<ArmUrdf | null>("arm_get_urdf", { prefix }),
  armRelease: () => invoke<void>("arm_release"),
  armSetDiagFocus: (prefix: string) => invoke<void>("arm_set_diag_focus", { prefix }),
  armRefreshDiag: () => invoke<void>("arm_refresh_diag"),
  armGetEvents: () => invoke<EventsSnapshot>("arm_get_events"),
  armGetLogs: () => invoke<LogLine[]>("arm_get_logs"),
  armClearFault: () => invoke<void>("arm_clear_fault"),

  // EE(Zenoh)
  eeConnect: (connect: string) => invoke<void>("ee_connect", { connect }),
  eeDisconnect: () => invoke<void>("ee_disconnect"),
  eeDiscover: () => invoke<EeInfo[]>("ee_discover"),
  eeDiscoverAll: () => invoke<RobotNode[]>("ee_discover_all"),
  eeAcquire: (prefix: string, model: string) => invoke<void>("ee_acquire", { prefix, model }),
  eeSetFocus: (prefix: string) => invoke<void>("ee_set_focus", { prefix }),
  eeGoto: (q: number, kp?: number) => invoke<void>("ee_goto", { q, kp: kp ?? null }),
  eeSetMode: (mode: number) => invoke<void>("ee_set_mode", { mode }),
  eeSetEstopBehavior: (behavior: number) => invoke<void>("ee_set_estop_behavior", { behavior }),
  eeClearFault: () => invoke<void>("ee_clear_fault"),
  eeGetState: () => invoke<ZenohEeState>("ee_get_state"),
  eeRelease: () => invoke<void>("ee_release"),
  eeScene: () => invoke<SceneRobot[]>("ee_scene"),
  consoleGetUrdf: (prefix: string, kindName: string) => invoke<ConsoleUrdf | null>("console_get_urdf", { prefix, kindName }),
  eeMachines: () => invoke<Record<string, MountEdge[]>>("ee_machines"),

  // Controller Config(Zenoh)
  configConnect: (connect: string) => invoke<void>("config_connect", { connect }),
  configDisconnect: () => invoke<void>("config_disconnect"),
  configDiscover: () => invoke<ControllerInfo[]>("config_discover"),
  configGet: (cid: string) => invoke<ConfigGetDto>("config_get", { cid }),
  configValidate: (cid: string, yaml: string) =>
    invoke<ConfigValidateResult>("config_validate", { cid, yaml }),
  configSet: (
    cid: string,
    yaml: string,
    expectSha256: string,
    apply: boolean,
    confirm: boolean,
    force: boolean,
  ) => invoke<ConfigSetResult>("config_set", { cid, yaml, expectSha256, apply, confirm, force }),
  configRestart: (cid: string, confirm: boolean, force: boolean) =>
    invoke<RestartResult>("config_restart", { cid, confirm, force }),
};

/** Normalise a thrown Tauri error (usually a plain string) to a message. */
export function errMsg(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e) return String((e as any).message);
  return String(e);
}
