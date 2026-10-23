import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { ControlOutlined, QuestionCircleOutlined, SettingOutlined } from "@ant-design/icons";
import {
  Alert,
  App,
  Button,
  Card,
  Collapse,
  Descriptions,
  Divider,
  Empty,
  Input,
  InputNumber,
  List,
  Popconfirm,
  Segmented,
  Select,
  Space,
  Switch,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import { useDamiaoTelemetry } from "../useDamiaoTelemetry";
import type { DamiaoBitrateState, DamiaoConfig, DamiaoDiscoveredDevice, DamiaoIdChangeResult, DamiaoMode, DamiaoState, DamiaoTarget, DamiaoRegisterWriteResult } from "../types";
import { DamiaoLiveChart } from "./DamiaoLiveChart";
import { DamiaoRegisterPanel } from "./DamiaoRegisterPanel";
import "./DamiaoMotorPanel.css";

const MODE_OPTIONS: DamiaoMode[] = ["Mit", "PositionVelocity", "Velocity", "ForcePosition"];
const MAPPING_P_MAX_RAD = 12.5;
const MAPPING_V_MAX_RAD_S = 30;
const MAPPING_T_MAX_NM = 10;
const DEVICE_POLL_MS = 500;
const BITRATE_OPTIONS = [125_000, 200_000, 250_000, 500_000, 1_000_000, 2_000_000, 2_500_000, 3_200_000, 4_000_000, 5_000_000];

export function DamiaoMotorPanel({ connected, onBusyChange }: { connected: boolean; onBusyChange?: (busy: boolean) => void }) {
  const { message } = App.useApp();
  const { lang } = useI18n();
  const zh = lang === "zh";
  const [devices, setDevices] = useState<DamiaoDiscoveredDevice[]>([]);
  const [workspaceTab, setWorkspaceTab] = useState<"control" | "settings">("control");
  const [selectedMotorId, setSelectedMotorId] = useState<number | null>(null);
  const [manualKey, setManualKey] = useState(0);
  const [manual, setManual] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [listBusy, setListBusy] = useState<"rescan" | "disable" | null>(null);
  const [detailBusy, setDetailBusy] = useState(false);
  const [lastIdChange, setLastIdChange] = useState<DamiaoIdChangeResult | null>(null);
  const listBusyRef = useRef(false);
  const deviceVersion = useRef(0);

  useEffect(() => {
    onBusyChange?.(detailBusy || listBusy != null);
  }, [detailBusy, listBusy, onBusyChange]);

  useEffect(() => () => onBusyChange?.(false), [onBusyChange]);

  const refreshDevices = useCallback(async () => {
    if (!connected) {
      setDevices([]);
      return;
    }
    const version = deviceVersion.current;
    const next = await api.damiaoListDevices();
    if (version === deviceVersion.current) setDevices(next);
  }, [connected]);

  useEffect(() => {
    if (!connected) {
      setDevices([]);
      setSelectedMotorId(null);
      setLastIdChange(null);
      return;
    }
    let alive = true;
    let timer: number | undefined;
    const tick = async () => {
      try {
        const version = deviceVersion.current;
        const next = await api.damiaoListDevices();
        if (alive && version === deviceVersion.current) {
          setDevices(next);
          setDiscoveryError(null);
        }
      } catch (error) {
        if (alive) setDiscoveryError(errMsg(error));
      } finally {
        if (alive) timer = window.setTimeout(tick, DEVICE_POLL_MS);
      }
    };
    void tick();
    return () => {
      alive = false;
      if (timer != null) window.clearTimeout(timer);
    };
  }, [connected]);

  useEffect(() => {
    if (!manual && selectedMotorId == null && devices.length > 0) {
      setSelectedMotorId(devices[0].motor_id);
    }
  }, [devices, manual, selectedMotorId]);

  const selectedDevice = devices.find((device) => device.motor_id === selectedMotorId);
  const motorIdChanged = useCallback((result: DamiaoIdChangeResult, migrate = true) => {
    setLastIdChange(result);
    if (!result.verified || !migrate) return;
    deviceVersion.current += 1;
    const next = result.state;
    setDevices((previous) => [
      ...previous.filter((device) => device.motor_id !== result.previous_motor_id && device.motor_id !== next.motor_id),
      {
        motor_id: next.motor_id,
        feedback_can_id: next.feedback_can_id,
        online: next.online,
        attached: next.attached,
        status_code: next.status_code,
        status: next.status,
        feedback_age_ms: next.feedback_age_ms,
        rx_count: next.rx_count,
      },
    ].sort((a, b) => a.motor_id - b.motor_id));
    setSelectedMotorId(next.motor_id);
    setManual(false);
  }, []);
  const attachedCount = devices.filter((device) => device.attached).length;
  const copy = zh
    ? {
        motors: "达妙电机",
        automatic: "自动发现电机",
        disconnected: "CAN 未连接",
        discoveryFailed: "无法刷新电机列表",
        waiting: "正在安全扫描 ID 0x0–0xF…",
        manual: "手动配置 ID",
        rescan: "安全重扫",
        rescanOk: "已请求安全重扫（仅探测未初始化电机）",
        disableAll: "全部失能",
        disableAllOk: "所有已初始化电机均已失能",
        attached: "已初始化",
        online: "在线",
        offline: "离线",
      }
    : {
        motors: "DAMIAO motors",
        automatic: "Automatic motor discovery",
        disconnected: "CAN disconnected",
        discoveryFailed: "Unable to refresh the motor list",
        waiting: "Safely scanning IDs 0x0–0xF…",
        manual: "Enter a motor ID",
        rescan: "Safe rescan",
        rescanOk: "Safe rescan requested (only unattached IDs are disabled)",
        disableAll: "Disable all",
        disableAllOk: "All attached motors disabled",
        attached: "Initialized",
        online: "Online",
        offline: "Offline",
      };

  const rescan = async () => {
    if (!connected || listBusyRef.current || detailBusy) return;
    listBusyRef.current = true;
    setListBusy("rescan");
    try {
      await api.damiaoSafeRescan();
      message.success(copy.rescanOk);
    } catch (error) {
      message.error(errMsg(error));
    } finally {
      listBusyRef.current = false;
      setListBusy(null);
    }
  };

  const disableAll = async () => {
    if (!connected || listBusyRef.current || detailBusy) return;
    listBusyRef.current = true;
    setListBusy("disable");
    try {
      await api.damiaoDisableAll();
      message.success(copy.disableAllOk);
    } catch (error) {
      message.error(errMsg(error));
    } finally {
      listBusyRef.current = false;
      setListBusy(null);
    }
  };

  return (
    <div className="damiao-app">
      <Segmented
        block
        className="damiao-workspace-tabs"
        aria-label={zh ? "电机工作区" : "Motor workspace"}
        value={workspaceTab}
        onChange={(value) => setWorkspaceTab(value as typeof workspaceTab)}
        options={[
          { label: zh ? "控制" : "Control", value: "control", icon: <ControlOutlined /> },
          { label: zh ? "配置" : "Configuration", value: "settings", icon: <SettingOutlined /> },
        ]}
      />
      <div className="damiao-workspace">
      <aside className="damiao-device-list">
        <div className="damiao-device-list__heading">
          <div>
            <Typography.Text strong>{copy.motors} ({devices.length})</Typography.Text>
            <Typography.Text type="secondary">{connected ? copy.automatic : copy.disconnected}</Typography.Text>
          </div>
          <Space size={6} wrap>
            <Button size="small" loading={listBusy === "rescan"} disabled={!connected || listBusy != null || detailBusy} onClick={rescan}>{copy.rescan}</Button>
            <Button size="small" danger loading={listBusy === "disable"} disabled={!connected || attachedCount === 0 || listBusy != null || detailBusy} onClick={disableAll}>{copy.disableAll}</Button>
          </Space>
        </div>
        {connected && discoveryError && <Alert className="damiao-discovery-error" type="warning" showIcon message={copy.discoveryFailed} description={discoveryError} />}
        {devices.length === 0 ? (
          <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={connected ? copy.waiting : undefined} />
        ) : (
          <List
            dataSource={devices}
            rowKey={(device) => device.motor_id}
            renderItem={(device) => {
              const selected = !manual && device.motor_id === selectedMotorId;
              return (
                <List.Item
                  className={`damiao-device-list__item${selected ? " damiao-device-list__item--selected" : ""}`}
                  role="button"
                  tabIndex={0}
                  aria-pressed={selected}
                  aria-disabled={detailBusy || listBusy != null}
                  onKeyDown={(event) => {
                    if (!detailBusy && listBusy == null && (event.key === "Enter" || event.key === " ")) {
                      event.preventDefault();
                      setManual(false);
                      setSelectedMotorId(device.motor_id);
                    }
                  }}
                  onClick={() => {
                    if (detailBusy || listBusy != null) return;
                    setManual(false);
                    setSelectedMotorId(device.motor_id);
                  }}
                >
                  <div>
                    <Space style={{ justifyContent: "space-between", width: "100%" }}>
                      <Typography.Text strong>DM-J4310-2EC</Typography.Text>
                      <Typography.Text code>{formatCanId(device.motor_id)}</Typography.Text>
                    </Space>
                    <Space size={4} wrap>
                      <Tag color={device.online ? "green" : "default"}>{device.online ? copy.online : copy.offline}</Tag>
                      {device.attached && <Tag color="blue">{copy.attached}</Tag>}
                      {device.feedback_can_id != null && <Tag>RX {formatCanId(device.feedback_can_id)}</Tag>}
                    </Space>
                  </div>
                </List.Item>
              );
            }}
          />
        )}
        <Button
          block
          className="damiao-device-list__manual"
          type={manual ? "primary" : "default"}
          disabled={detailBusy || listBusy != null}
          onClick={() => {
            setManual(true);
            setSelectedMotorId(null);
            setManualKey((value) => value + 1);
          }}
        >
          {copy.manual}
        </Button>
      </aside>
      <section className="damiao-workspace__detail">
        <DamiaoMotorDetail
          key={manual ? `manual-${manualKey}` : `motor-${selectedMotorId ?? "default"}`}
          connected={connected}
          workspaceTab={workspaceTab}
          externallyBusy={listBusy != null}
          initialDevice={manual ? undefined : selectedDevice}
          initialIdChange={lastIdChange?.state.motor_id === selectedDevice?.motor_id ? lastIdChange : null}
          onMotorIdChanged={motorIdChanged}
          onDevicesChanged={refreshDevices}
          onBusyChange={setDetailBusy}
        />
      </section>
      </div>
    </div>
  );
}

function DamiaoMotorDetail({
  connected,
  workspaceTab,
  externallyBusy,
  initialDevice,
  initialIdChange,
  onMotorIdChanged,
  onDevicesChanged,
  onBusyChange,
}: {
  connected: boolean;
  workspaceTab: "control" | "settings";
  externallyBusy: boolean;
  initialDevice?: DamiaoDiscoveredDevice;
  initialIdChange: DamiaoIdChangeResult | null;
  onMotorIdChanged: (result: DamiaoIdChangeResult, migrate?: boolean) => void;
  onDevicesChanged: () => Promise<void>;
  onBusyChange: (busy: boolean) => void;
}) {
  const { message } = App.useApp();
  const { lang } = useI18n();
  const zh = lang === "zh";

  const initialMotorId = initialDevice?.motor_id ?? 1;
  const [motorId, setMotorId] = useState(formatCanId(initialMotorId));
  const [masterId, setMasterId] = useState(formatCanId(initialDevice?.feedback_can_id ?? 0));
  const [mode, setMode] = useState<DamiaoMode>(initialIdChange?.state.mode ?? "Mit");
  const [pMax, setPMax] = useState(MAPPING_P_MAX_RAD);
  const [vMax, setVMax] = useState(MAPPING_V_MAX_RAD_S);
  const [tMax, setTMax] = useState(MAPPING_T_MAX_NM);
  const [state, setState] = useState<DamiaoState | null>(initialDevice?.attached ? initialIdChange?.state ?? null : null);
  const [detachedLocally, setDetachedLocally] = useState(false);
  const [working, setBusy] = useState(false);
  const busy = working || externallyBusy;
  const busyRef = useRef(false);
  const autoAttachAttempted = useRef(false);
  const autoBitrateAttempted = useRef(false);
  const targetsSeeded = useRef(false);
  const [operationError, setOperationError] = useState<string | null>(null);
  const [initializing, setInitializing] = useState(false);
  const [newMotorId, setNewMotorId] = useState("");
  const [saveId, setSaveId] = useState(true);
  const [idError, setIdError] = useState<string | null>(null);
  const [idOutcome, setIdOutcome] = useState<DamiaoIdChangeResult | null>(initialIdChange);
  const [bitrateState, setBitrateState] = useState<DamiaoBitrateState | null>(null);
  const [newBitrate, setNewBitrate] = useState(1_000_000);
  const [bitrateError, setBitrateError] = useState<string | null>(null);
  const [bitrateOutcome, setBitrateOutcome] = useState<DamiaoBitrateState | null>(null);
  const [zeroConfirmOpen, setZeroConfirmOpen] = useState(false);
  const [zeroError, setZeroError] = useState<string | null>(null);
  const [zeroOutcome, setZeroOutcome] = useState<DamiaoState | null>(null);
  const [repeat, setRepeat] = useState(true);
  const [rateHz, setRateHz] = useState(50);
  const [view, setView] = useState<"panel" | "chart">("panel");
  const [windowSec, setWindowSec] = useState(10);
  const positionTrajectory = mode === "PositionVelocity" || mode === "ForcePosition";
  const targetRepeats = repeat && !positionTrajectory && mode !== "Unspecified";

  const [mit, setMit] = useState({
    position_rad: 0,
    velocity_rad_s: 0,
    torque_nm: 0,
    kp: 0,
    kd: 0.1,
  });
  const [positionVelocity, setPositionVelocity] = useState({
    position_rad: 0,
    velocity_rad_s: 1,
  });
  const [velocity, setVelocity] = useState(0);
  const [forcePosition, setForcePosition] = useState({ position_rad: 0, velocity_rad_s: 1, current_limit_ratio: 0.1 });

  const copy = useMemo(
    () =>
      zh
        ? {
            attach: "初始化电机",
            detach: "停止并解除初始化",
            connection: "电机与协议配置",
            telemetry: "实时反馈",
            control: "电机控制",
            display: "信息显示",
            numeric: "数值",
            chart: "图表",
            window: "窗口",
            refresh: "刷新",
            refreshLow: "低",
            refreshHigh: "高",
            refreshHint: "界面轮询只读取后端最新快照，不改变 CAN 接收和控制发送速率；图表仅记录 rx_count 增长后的真实新反馈。",
            connected: "已初始化",
            detached: "待初始化",
            online: "反馈在线",
            offline: "无新反馈",
            motorId: "电机 CAN ID",
            masterId: "反馈 Master ID",
            detectedMasterId: "实际反馈 ID",
            physicalMode: "运行时控制模式",
            mapping: "电机映射参数",
            mappingHint: "初始化时自动读取，兼容 24 V / 48 V 电机。",
            attachOk: "电机已初始化，保持失能",
            detachOk: "电机已失能并卸载",
            actionOk: "命令已发送",
            modeSwitchOk: "控制模式已切换，原定时目标已停止",
            sendOk: targetRepeats ? "目标已发送并以 50 Hz 持续更新" : "目标已发送一次",
            stopStream: "停止定时发送",
            enable: "使能",
            disable: "停止并失能",
            clearFault: "清故障",
            setZero: "将当前位置设为零点",
            setZeroConfirm: "保存当前位置为电机零点？",
            setZeroDescription: "将停止定时目标并失能电机，随后把当前位置保存为零点。位置坐标将改变，界面运动目标和图表历史会清零。请固定电机，保持轴静止。",
            send: "发送目标",
            periodic: "50 Hz 定时发送",
            positionOneShot: "位置速度目标只发送一次，由驱动器内部完成梯形轨迹",
            position: "控制位置",
            speed: "速度",
            torque: "前馈转矩",
            mosTemp: "MOS 温度",
            rotorTemp: "线圈温度",
            status: "状态",
            feedbackAge: "反馈时延",
            feedbackRate: "反馈频率",
            rxCount: "反馈帧数",
            configureFirst: "连接 CAN 后选择电机，自动读取映射参数和反馈；点击使能后即可发送目标。",
            noFeedback:
              "未收到新反馈。请确认供电、CAN_H/CAN_L、共地、双端 120 Ω、主机与电机的波特率及 CAN / CAN FD 格式一致，以及电机 ID。",
            masterMismatch: "已自动匹配电机实际反馈 ID，无需手动修改。",
            safety:
              "24 V 与 48 V 电机使用相同的标准 CAN 协议，出厂波特率为 1 Mbps。初始化保持失能；使能和切换模式会先确认停止，并预置当前坐标下的静止目标。",
            modeLocked:
              "切换模式会停止旧目标，确认失能和轴静止后切换；位置目标自动采用当前精确位置，速度和前馈转矩清零。已使能的电机会恢复使能。",
            targetLocked: "收到“使能”反馈后才允许发送运动目标。",
            tmaxHint:
              "MIT 转矩按电机读回的 TMAX 映射，不设置型号专属转矩上限。",
            vmaxHint:
              "MIT 速度按电机读回的 VMAX 映射；浮点速度指令不受反馈映射范围限制。",
            posVelSpeed: "最高速度",
          }
        : {
            attach: "Initialize motor",
            detach: "Stop & detach",
            connection: "Motor and protocol configuration",
            telemetry: "Live feedback",
            control: "Motor control",
            display: "Display",
            numeric: "Numeric",
            chart: "Chart",
            window: "Window",
            refresh: "Refresh",
            refreshLow: "Low",
            refreshHigh: "High",
            refreshHint: "UI polling reads only the latest backend snapshot and does not change CAN receive or control rates. The chart records only real new feedback when rx_count advances.",
            connected: "Initialized",
            detached: "Not initialized",
            online: "Feedback online",
            offline: "No fresh feedback",
            motorId: "Motor CAN ID",
            masterId: "Feedback Master ID",
            detectedMasterId: "Detected feedback ID",
            physicalMode: "Runtime control mode",
            mapping: "Motor mapping parameters",
            mappingHint: "Read automatically on initialization; supports 24 V / 48 V motors.",
            attachOk: "Motor initialized and disabled",
            detachOk: "Motor disabled and detached",
            actionOk: "Command sent",
            modeSwitchOk: "Control mode changed; the previous periodic target was stopped",
            sendOk: targetRepeats ? "Target sent and streaming at 50 Hz" : "Target sent once",
            stopStream: "Stop periodic send",
            enable: "Enable",
            disable: "Stop & disable",
            clearFault: "Clear fault",
            setZero: "Set current position as zero",
            setZeroConfirm: "Save the current position as the motor zero?",
            setZeroDescription: "Periodic targets will stop and the motor will be disabled before its current position is saved as zero. This changes position coordinates and clears local targets and chart history. Secure the motor and keep the shaft still.",
            send: "Send target",
            periodic: "Periodic send at 50 Hz",
            positionOneShot: "Position-velocity goals are sent once; the drive completes the internal trapezoidal trajectory",
            position: "Control position",
            speed: "Velocity",
            torque: "Feed-forward torque",
            mosTemp: "MOS temperature",
            rotorTemp: "Winding temperature",
            status: "Status",
            feedbackAge: "Feedback age",
            feedbackRate: "Feedback rate",
            rxCount: "Feedback frames",
            configureFirst: "Connect CAN and select a motor to read its mapping and feedback automatically, then enable and send a target.",
            noFeedback:
              "No fresh feedback. Check power, CAN_H/CAN_L, common ground, dual 120 Ω termination, matching host/motor bitrate and CAN / CAN FD format, and the motor ID.",
            masterMismatch: "The actual feedback ID was detected and matched automatically; no manual change is needed.",
            safety:
              "24 V and 48 V motors share the standard CAN protocol and factory bitrate of 1 Mbps. Initialization leaves the drive disabled. Enabling and mode changes confirm a stationary shaft and preload a neutral target in the current coordinate system.",
            modeLocked:
              "Mode changes stop the previous target, confirm disabled and stationary feedback, and reset position to the precise current position with zero velocity and feed-forward torque. An enabled motor is enabled again afterwards.",
            targetLocked: "Motion targets unlock after an Enabled feedback frame is received.",
            tmaxHint:
              "MIT torque uses TMAX read from the motor, without a model-specific torque cap.",
            vmaxHint:
              "MIT velocity uses VMAX read from the motor. Float velocity commands are independent of the feedback mapping range.",
            posVelSpeed: "Maximum speed",
          },
    [targetRepeats, zh],
  );

  const attached = connected && !detachedLocally && (state ? state.attached : !!initialDevice?.attached);
  const attachedMotorId = attached ? state?.motor_id ?? initialMotorId : null;
  const { latest, samples, chartVersion, resetHistory, acceptSnapshot } = useDamiaoTelemetry(
    attachedMotorId,
    connected,
    rateHz,
    view === "chart" && workspaceTab === "control",
  );
  const faulted = state != null && state.status_code !== 0 && state.status_code !== 1;
  const requiresReconnect = !!bitrateOutcome?.requires_reconnect || !!state?.requires_reconnect;
  const controlsReady = connected && attachedMotorId != null && !requiresReconnect;
  const motionReady = controlsReady && !!state?.online && !!state?.enabled && !faulted && mode !== "Unspecified";
  const mitPositionOutOfRange = mode === "Mit" && Math.abs(mit.position_rad) > pMax;
  const zeroReady = controlsReady && !!state?.online && state?.position_rad != null && !faulted && !busy;
  const masterMismatch = state?.feedback_can_id != null && state.feedback_can_id !== state.master_id;
  const singleTurnAngle = state?.output_position_rad != null
    ? (((state.output_position_rad + Math.PI) % (2 * Math.PI)) + 2 * Math.PI) % (2 * Math.PI) - Math.PI
    : null;

  useEffect(() => {
    if (requiresReconnect) setBitrateState(null);
  }, [requiresReconnect]);

  useEffect(() => {
    if (!zeroReady) setZeroConfirmOpen(false);
  }, [zeroReady]);

  useEffect(() => {
    if (!connected) {
      setState(null);
      setBitrateState(null);
      setBitrateError(null);
      setBitrateOutcome(null);
      setZeroOutcome(null);
      setZeroError(null);
      setIdOutcome(null);
      setIdError(null);
      setOperationError(null);
      targetsSeeded.current = false;
      return;
    }
    if (latest == null || !attached || busy || latest.motor_id !== attachedMotorId) return;
    setState(latest);
    setMotorId(formatCanId(latest.motor_id));
    setMasterId(formatCanId(latest.master_id));
    setMode(latest.mode);
    setPMax(latest.p_max);
    setVMax(latest.v_max);
    setTMax(latest.t_max);
  }, [attached, attachedMotorId, busy, connected, latest]);

  const acceptState = (next: DamiaoState) => {
    acceptSnapshot(next);
    setState(next);
    setMotorId(formatCanId(next.motor_id));
    setMasterId(formatCanId(next.master_id));
    setMode(next.mode);
    setPMax(next.p_max);
    setVMax(next.v_max);
    setTMax(next.t_max);
  };

  const resetTargets = (next: DamiaoState) => {
    targetsSeeded.current = next.mode === "Velocity" || next.position_rad != null;
    // Preserve the measured multi-turn coordinate. Silently clipping it to
    // PMAX would turn a later nonzero Kp into a different position command.
    setMit({ position_rad: next.position_rad ?? 0, velocity_rad_s: 0, torque_nm: 0, kp: 0, kd: 0.1 });
    setPositionVelocity({ position_rad: next.position_rad ?? 0, velocity_rad_s: 1 });
    setForcePosition({ position_rad: next.position_rad ?? 0, velocity_rad_s: 1, current_limit_ratio: 0.1 });
    setVelocity(0);
  };

  useEffect(() => {
    if (!busy && latest && latest.motor_id === attachedMotorId && !targetsSeeded.current
      && (latest.mode === "Velocity" || latest.position_rad != null)) resetTargets(latest);
  }, [attachedMotorId, busy, latest]);

  const run = useCallback(
    async (success: string, action: () => Promise<unknown>) => {
      if (!connected || externallyBusy || busyRef.current) return;
      busyRef.current = true;
      setBusy(true);
      setOperationError(null);
      onBusyChange(true);
      try {
        await action();
        if (success) message.success(success);
      } catch (error) {
        setOperationError(errMsg(error));
        message.error(errMsg(error));
        throw error;
      } finally {
        busyRef.current = false;
        setBusy(false);
        onBusyChange(false);
      }
    },
    [connected, externallyBusy, message, onBusyChange],
  );

  const initialize = async (enable: boolean, automatic = false) => {
    if (!connected || busyRef.current || externallyBusy || requiresReconnect) return;
    let config: DamiaoConfig;
    try {
      config = {
        motor_id: parseCanId(motorId, 0x7ff, copy.motorId),
        master_id: parseCanId(masterId, 0x7ff, copy.masterId),
        mode,
        p_max: pMax,
        v_max: vMax,
        t_max: tMax,
      };
    } catch (error) {
      message.error(errMsg(error));
      return;
    }
    try {
      await run(automatic ? "" : enable ? zh ? "电机已使能，反馈已确认" : "Motor enabled; feedback confirmed" : copy.attachOk, async () => {
        setInitializing(true);
        let next: DamiaoState | null = null;
        try {
          next = attachedMotorId != null
            ? await api.damiaoGetState(attachedMotorId)
            : await api.damiaoAttach(config);
          setDetachedLocally(false);
          acceptState(next);
          if (enable) next = await api.damiaoEnable(next.motor_id);
          else next = await api.damiaoReadPositions(next.motor_id).catch(() => next!);
          acceptState(next);
          resetTargets(next);
        } catch (error) {
          // Initialization may already have attached the motor. Keep that
          // session visible so Enable can be retried without another attach.
          if (next) {
            const recovered = await api.damiaoGetState(next.motor_id).catch(() => next!);
            acceptState(recovered);
            resetTargets(recovered);
          }
          throw error;
        } finally {
          setInitializing(false);
          await onDevicesChanged().catch(() => {});
        }
      });
    } catch {
      // `run` already presented the backend error.
    }
  };

  useEffect(() => {
    if (!connected) {
      autoAttachAttempted.current = false;
      return;
    }
    if (!initialDevice?.online || attached || detachedLocally || busyRef.current || externallyBusy || autoAttachAttempted.current) return;
    // One attempt per selection. An offline or incompatible motor must not
    // cause an endless loop of disable probes and error notifications.
    autoAttachAttempted.current = true;
    void initialize(false, true);
  }, [connected, initialDevice?.motor_id, initialDevice?.online, attached, detachedLocally, externallyBusy]);

  const detach = async () => {
    if (attachedMotorId == null) return;
    try {
      await run(copy.detachOk, async () => {
        await api.damiaoDetach(attachedMotorId);
        setDetachedLocally(true);
        autoBitrateAttempted.current = false;
        targetsSeeded.current = false;
        setState(null);
        setBitrateState(null);
        setBitrateOutcome(null);
        setBitrateError(null);
        setZeroOutcome(null);
        setZeroError(null);
        setIdOutcome(null);
        setIdError(null);
        await onDevicesChanged().catch(() => {});
      });
    } catch {
      // Keep the attached state visible so a failed safe-disable is retryable.
    }
  };

  const changeMode = async (nextMode: DamiaoMode) => {
    if (nextMode === mode) return;
    if (attachedMotorId == null) {
      setMode(nextMode);
      return;
    }

    const previousMode = mode;
    try {
      await run(copy.modeSwitchOk, async () => {
        const next = await api.damiaoSetMode(attachedMotorId, nextMode);
        acceptState(next);
        resetTargets(next);
      });
    } catch {
      // The backend commits the new encoder/ID state as soon as the mode
      // write succeeds, even if restoring enable later fails. Re-read it so
      // the UI never sends a target using a stale local mode.
      try {
        const latest = await api.damiaoGetState(attachedMotorId);
        acceptState(latest);
        resetTargets(latest);
      } catch {
        setMode(previousMode);
      }
    }
  };

  const action = async (fn: () => Promise<unknown>, success = copy.actionOk) => {
    try {
      await run(success, async () => {
        try {
          await fn();
        } finally {
          if (attachedMotorId != null) {
            const next = await api.damiaoGetState(attachedMotorId).catch(() => null);
            if (next) acceptState(next);
          }
        }
      });
    } catch {
      // Error was surfaced by `run`.
    }
  };

  const changeMotorId = async () => {
    if (!controlsReady || attachedMotorId == null || busyRef.current) return;
    let requestedId: number;
    try {
      requestedId = parseCanId(newMotorId, 0x7ff, zh ? "新电机 ID" : "New motor ID");
      if (requestedId === attachedMotorId) {
        message.info(zh ? "新 ID 与当前 ID 相同，无需修改" : "The motor already uses this ID");
        return;
      }
    } catch (error) {
      setIdError(errMsg(error));
      return;
    }
    try {
      await run("", async () => {
        setIdError(null);
        setIdOutcome(null);
        const result = await api.damiaoSetMotorId(attachedMotorId, requestedId, saveId);
        setState(result.state);
        setMotorId(formatCanId(result.state.motor_id));
        setIdOutcome(result);
        setBitrateState(null);
        setRepeat(false);
        resetHistory(result.state);
        onMotorIdChanged(result);
        if (result.error) {
          message.warning(zh ? "请查看电机 ID 修改结果" : "Review the motor ID change result");
        } else {
          message.success(zh
            ? `电机 ID 已改为 ${formatCanId(result.state.motor_id)}${result.saved ? "，已保存到 Flash" : "（未保存到 Flash）"}`
            : `Motor ID changed to ${formatCanId(result.state.motor_id)}${result.saved ? "; saved to Flash" : "; not saved to Flash"}`);
        }
        await onDevicesChanged().catch(() => {});
      });
    } catch (error) {
      setIdError(errMsg(error));
    }
  };

  const saveParameters = async () => {
    if (!controlsReady || attachedMotorId == null) return;
    await action(async () => {
      await api.damiaoSaveParameters(attachedMotorId);
      if (idOutcome?.verified && idOutcome.state.motor_id === attachedMotorId) {
        const result = { ...idOutcome, saved: true, error: null, state: await api.damiaoGetState(attachedMotorId) };
        setIdOutcome(result);
        onMotorIdChanged(result, false);
      }
    }, zh ? "电机已失能，当前全部参数已保存到 Flash" : "Motor disabled; all current parameters saved to Flash");
  };

  const writeRegister = async (address: number, value: number): Promise<DamiaoRegisterWriteResult> => {
    if (attachedMotorId == null) throw new Error(zh ? "请先初始化电机" : "Initialize the motor first");
    try {
      const result = await api.damiaoWriteRegister(attachedMotorId, address, value);
      acceptState(result.state);
      setRepeat(false);
      if ([0x07, 0x08, 0x0A, 0x15, 0x16, 0x17].includes(address)) {
        resetHistory(result.state);
        resetTargets(result.state);
      }
      if (result.id_change) {
        setIdOutcome(result.id_change);
        setBitrateState(null);
        onMotorIdChanged(result.id_change);
      }
      if (result.bitrate) {
        setBitrateOutcome(result.bitrate);
        setBitrateState(result.bitrate.requires_reconnect ? null : result.bitrate);
        setNewBitrate(result.bitrate.bitrate);
      }
      await onDevicesChanged().catch(() => {});
      return result;
    } catch (error) {
      const next = await api.damiaoGetState(attachedMotorId).catch(() => null);
      if (next) acceptState(next);
      if ([0x07, 0x08, 0x0A, 0x15, 0x16, 0x17].includes(address)) resetHistory(next ?? undefined);
      throw error;
    }
  };

  const idPanel = (
    <Card size="small" className="damiao-id-card" title={zh ? "电机 ID" : "Motor ID"}>
      <div className="damiao-id-settings">
        <div className="damiao-id-current">
          <Typography.Text type="secondary">{zh ? "当前电机 ID" : "Current motor ID"}</Typography.Text>
          <Typography.Text strong className="damiao-tabular">{attachedMotorId != null ? formatCanId(attachedMotorId) : "—"}</Typography.Text>
          <Typography.Text type="secondary">{zh ? "反馈 Master ID" : "Feedback Master ID"}: {state?.feedback_can_id != null ? formatCanId(state.feedback_can_id) : "—"}</Typography.Text>
        </div>
        <Space wrap align="end">
          <Field label={zh ? "新电机 ID" : "New motor ID"}>
            <Input value={newMotorId} placeholder={zh ? "如 2 或 0x02" : "e.g. 2 or 0x02"} disabled={!controlsReady || busy} onChange={(event) => setNewMotorId(event.target.value)} style={{ width: 180 }} />
          </Field>
          <Space>
            <Switch checked={saveId} disabled={!controlsReady || busy} onChange={setSaveId} />
            <Typography.Text>{zh ? "保存到 Flash" : "Save to Flash"}</Typography.Text>
          </Space>
          <Button type="primary" disabled={!controlsReady || busy || !newMotorId.trim()} onClick={() => void changeMotorId()}>
            {zh ? "失能并修改 ID" : "Disable & change ID"}
          </Button>
          {idOutcome?.verified && !idOutcome.saved && (
            <Button disabled={!controlsReady || busy} onClick={() => void saveParameters()}>
              {zh ? "失能并保存全部参数" : "Disable & save all parameters"}
            </Button>
          )}
        </Space>
      </div>
      <Typography.Paragraph className="damiao-id-hint" type="secondary">
        {zh
          ? "支持十进制或 0x 开头的十六进制，范围 0x000–0x7FF；手册建议使用 0x00–0x0F。控制帧匹配 ID 低 8 位，反馈携带低 4 位。修改会停止定时目标、确认失能，校验新 ID 后自动切换连接。反馈 Master ID 保持当前值。"
          : "Enter decimal or 0x-prefixed hexadecimal, 0x000–0x7FF; IDs below 16 are recommended. Control frames match the low 8 ID bits; feedback carries the low 4 bits. Changing stops periodic targets, confirms disabled feedback and verifies the new ID before updating the connection. Feedback Master ID is preserved."}
      </Typography.Paragraph>
      <Typography.Paragraph className="damiao-id-hint" type="secondary">
        {zh
          ? "保存到 Flash 后断电仍保留新 ID。保存会写入当前全部参数（包含运行时控制模式），请勿频繁保存。应用检查已发现电机的 ID 冲突，并查询目标 ID 是否占用；尚未响应的设备仍可能无法检测，首次配置建议单独连接目标电机。"
          : "Saving to Flash keeps the new ID after power loss and writes all current parameters, including the runtime control mode. Avoid frequent saves. The app checks discovered ID conflicts and queries the requested address; unresponsive devices may escape detection, so configure a motor on its own initially."}
      </Typography.Paragraph>
      {idError && <Alert type="error" showIcon message={zh ? "ID 修改未完成" : "ID change not completed"} description={idError} />}
      {idOutcome && (
        <Alert
          type={idOutcome.error || !idOutcome.saved ? "warning" : "success"}
          showIcon
          message={idOutcome.verified
            ? zh ? `新 ID ${formatCanId(idOutcome.state.motor_id)} 已验证${idOutcome.saved ? "，已保存到 Flash" : "，未确认保存到 Flash"}` : `New ID ${formatCanId(idOutcome.state.motor_id)} verified${idOutcome.saved ? "; saved to Flash" : "; Flash storage not confirmed"}`
            : zh ? "电机实际 ID 尚未确认，请重新连接后检查" : "Actual motor ID is uncertain; reconnect and check"}
          description={idOutcome.verified
            ? idOutcome.error ?? (idOutcome.saved ? undefined : zh ? "新 ID 已生效。可使用“失能并保存全部参数”保存；未保存时断电可能恢复原 ID。" : "The new ID is active. Use Disable & save all parameters to persist it; power loss can restore the previous ID until saved.")
            : <Space direction="vertical" size={4}>
                <Typography.Text>{zh
                  ? `请在顶部断开并重新连接 CAN，检查原 ID ${formatCanId(idOutcome.previous_motor_id)} 和目标 ID ${formatCanId(idOutcome.requested_motor_id)}。通信确认前控制保持锁定。`
                  : `Disconnect and reconnect CAN above, then check previous ID ${formatCanId(idOutcome.previous_motor_id)} and requested ID ${formatCanId(idOutcome.requested_motor_id)}. Controls stay locked until communication is verified.`}</Typography.Text>
                <Typography.Text>{idOutcome.error}</Typography.Text>
              </Space>}
        />
      )}
    </Card>
  );

  const readBitrate = async () => {
    if (attachedMotorId == null || requiresReconnect) return;
    try {
      await run("", async () => {
        const next = await api.damiaoGetBitrate(attachedMotorId);
        setBitrateState(next);
        setNewBitrate(next.bitrate);
        setBitrateError(null);
        setBitrateOutcome(null);
      });
    } catch (error) {
      setBitrateState(null);
      setBitrateError(errMsg(error));
    }
  };

  useEffect(() => {
    if (!connected || !attached) autoBitrateAttempted.current = false;
    if (workspaceTab !== "settings" || !controlsReady || busy || autoBitrateAttempted.current) return;
    autoBitrateAttempted.current = true;
    void readBitrate();
  }, [attached, busy, connected, controlsReady, workspaceTab]);

  const changeBitrate = async () => {
    if (attachedMotorId == null || !bitrateState || requiresReconnect) return;
    try {
      await run("", async () => {
        const next = await api.damiaoSetBitrate(attachedMotorId, newBitrate);
        setBitrateOutcome(next);
        setBitrateState(next.requires_reconnect ? null : next);
        setBitrateError(null);
        if (next.requires_reconnect) {
          message.warning(zh ? "请将主机改为目标波特率后重新连接，再读取确认并保存参数" : "Reconnect the host at the target bitrate, then read to verify and save parameters");
        } else {
          message.success(zh ? "已确认电机正在使用该波特率，无需修改" : "The motor already uses this bitrate; no change was needed");
        }
      });
    } catch (error) {
      setBitrateState(null);
      setBitrateError(errMsg(error));
    }
  };

  const setZero = async () => {
    if (attachedMotorId == null || !zeroReady) return;
    setZeroConfirmOpen(false);
    try {
      await run("", async () => {
        setZeroError(null);
        setZeroOutcome(null);
        // Even a missing acknowledgement can follow a real zero change.
        // Clear old coordinates before the transaction and never send them.
        setMit({ position_rad: 0, velocity_rad_s: 0, torque_nm: 0, kp: 0, kd: 0 });
        setPositionVelocity({ position_rad: 0, velocity_rad_s: 0 });
        setForcePosition({ position_rad: 0, velocity_rad_s: 0, current_limit_ratio: 0 });
        setVelocity(0);
        setRepeat(false);
        resetHistory();
        const next = await api.damiaoSetZero(attachedMotorId);
        setState(next);
        setZeroOutcome(next);
        resetHistory(next);
        message.success(zh ? "零点已保存，已收到失能且接近零位的反馈" : "Zero saved; disabled feedback confirmed near zero");
      });
    } catch (error) {
      setZeroError(errMsg(error));
      resetHistory();
    }
  };

  const zeroPanel = (
    <Card
      className="damiao-zero-card"
      size="small"
      title={zh ? "位置零点" : "Position zero"}
      extra={busy ? <Tag color="processing">{zh ? "命令执行中" : "Working"}</Tag> : zeroOutcome ? <Tag color="success">{zh ? "反馈已验证" : "Feedback verified"}</Tag> : undefined}
    >
      <div className="damiao-zero-settings">
        <div className="damiao-zero-current">
          <Typography.Text type="secondary">{zh ? "当前控制位置" : "Current control position"}</Typography.Text>
          <Typography.Text strong><TelemetryNumber value={state?.position_rad} suffix=" rad" /></Typography.Text>
          <Typography.Text type="secondary">
            {busy
              ? zh ? "等待当前命令完成" : "Wait for the current command to finish"
              : zeroReady
              ? zh ? "保持轴静止后设定零点" : "Keep the shaft still before setting zero"
              : zh ? "需初始化、反馈在线、位置已更新且无故障" : "Requires initialization with fresh position and fault-free feedback"}
          </Typography.Text>
        </div>
        <Popconfirm
          title={copy.setZeroConfirm}
          description={<>
            <Typography.Paragraph style={{ marginBottom: 8 }}>
              {zh ? "电机：" : "Motor: "}{formatCanId(attachedMotorId ?? initialMotorId)}
              {zh ? "；当前控制位置：" : "; current control position: "}
              <TelemetryNumber value={state?.position_rad} suffix=" rad" />
            </Typography.Paragraph>
            {copy.setZeroDescription}
          </>}
          overlayClassName="damiao-zero-confirm"
          open={zeroConfirmOpen}
          onOpenChange={(open) => setZeroConfirmOpen(open && zeroReady)}
          disabled={!zeroReady}
          okText={zh ? "失能并设置零点" : "Disable & set zero"}
          cancelText={zh ? "取消" : "Cancel"}
          onConfirm={() => void setZero()}
        >
          <Button disabled={!zeroReady}>{copy.setZero}</Button>
        </Popconfirm>
      </div>
      <Typography.Paragraph className="damiao-zero-hint" type="secondary">
        {zh
          ? "操作会停止定时目标、失能电机并保存当前位置为零点，之后查询新反馈确认。完成后保持失能；位置、速度、转矩和增益目标会清零，定时发送关闭，图表从新零点开始记录。"
          : "Stops periodic targets, disables the motor, saves the current position as zero, and verifies fresh feedback. The motor stays disabled. Position, velocity, torque, and gain targets are cleared, periodic send is turned off, and the chart starts a new reference."}
      </Typography.Paragraph>
      {zeroOutcome && (
        <Alert
          type="success"
          showIcon
          message={zh ? "零点已保存并验证，电机保持失能" : "Zero saved and verified; motor remains disabled"}
          description={<Space wrap size={4}>
            <Typography.Text>{zh ? "设置后反馈位置：" : "Verified position after setting: "}</Typography.Text>
            <TelemetryNumber value={zeroOutcome.position_rad} suffix=" rad" />
            <Typography.Text type="secondary">{zh ? "重新设置目标后再使能和发送。" : "Configure new targets before enabling and sending."}</Typography.Text>
          </Space>}
        />
      )}
      {zeroError && (
        <Alert
          type="warning"
          showIcon
          message={zh ? "零点尚未确认" : "Zero not verified"}
          description={`${zeroError} ${zh ? "界面旧目标已清除。请查看错误原因和最新反馈，保持轴静止后再操作。" : "Previous local targets were cleared. Check the error and fresh feedback before retrying with the shaft still."}`}
        />
      )}
    </Card>
  );

  const bitratePanel = (
    <Card
      className="damiao-bitrate-card"
      size="small"
      title={zh ? "CAN 波特率" : "CAN bitrate"}
      extra={busy ? <Tag color="processing">{zh ? "命令执行中" : "Working"}</Tag> : <Tag>{bitrateState ? bitrateState.can_fd ? "CAN FD" : "CAN 2.0" : zh ? "未读取" : "Not read"}</Tag>}
    >
      <div className="damiao-bitrate-settings">
        <div className="damiao-bitrate-current">
          <Typography.Text type="secondary">{zh ? "电机当前值" : "Current motor setting"}</Typography.Text>
          <Typography.Text strong className="damiao-tabular">
            {bitrateState ? formatBitrate(bitrateState.bitrate) : "—"}
          </Typography.Text>
          <Typography.Text type="secondary">
            {requiresReconnect
              ? zh ? "重新连接并确认电机 ID 与波特率后再读取" : "Reconnect and verify the motor ID and bitrate before reading"
              : bitrateState
              ? `${zh ? "已读取" : "Read"} · ${zh ? "代码" : "code"} ${bitrateState.register_code}`
              : zh ? "初始化后自动读取，也可手动刷新" : "Read automatically after initialization; refresh to retry"}
          </Typography.Text>
        </div>
        <Space wrap align="end" className="damiao-bitrate-actions">
          <Button disabled={!controlsReady || busy} onClick={() => void readBitrate()}>
            {zh ? "读取波特率" : "Read bitrate"}
          </Button>
          <Field label={zh ? "目标波特率" : "Target bitrate"}>
            <Select
              value={newBitrate}
              disabled={!controlsReady || busy || !bitrateState}
              onChange={setNewBitrate}
              options={BITRATE_OPTIONS.map((bitrate) => ({ value: bitrate, label: `${formatBitrate(bitrate)} · ${bitrate > 1_000_000 ? "CAN FD" : "CAN 2.0"}` }))}
            />
          </Field>
          <Button type="primary" disabled={!controlsReady || busy || !bitrateState || newBitrate === bitrateState.bitrate} onClick={() => void changeBitrate()}>
            {zh ? "失能并切换波特率" : "Disable & change bitrate"}
          </Button>
        </Space>
      </div>
      <Typography.Paragraph className="damiao-bitrate-hint" type="secondary">
        {zh
          ? "切换会停止定时目标并失能电机。≤ 1 Mbps 使用 CAN 2.0，> 1 Mbps 切换为 CAN FD 反馈。写入后需让主机使用匹配的速率重新连接。"
          : "Changing bitrate stops periodic targets and disables the motor. ≤ 1 Mbps uses CAN 2.0; > 1 Mbps switches feedback to CAN FD. Reconnect the host at the matching rate after writing."}
      </Typography.Paragraph>
      <Typography.Paragraph className="damiao-bitrate-hint" type="secondary">
        {zh
          ? "波特率写入后先在新速率下读取确认，再保存以便断电保留。保存会将当前全部参数（包含运行时控制模式）写入 Flash。手册建议在装机前单独配置波特率；本应用仅在目标电机单独连接时允许切换。"
          : "Read and verify at the new rate before saving for persistence. Save writes all current parameters, including the runtime control mode, to Flash. The manual recommends configuring bitrate before installation; this app allows changing only when the target motor is connected on its own."}
      </Typography.Paragraph>
      {bitrateError && <Alert type="warning" showIcon message={zh ? "波特率尚未确认" : "Bitrate not verified"} description={bitrateError} />}
      {bitrateOutcome?.requires_reconnect && (
        <Alert
          type="warning"
          showIcon
          message={bitrateOutcome.acknowledged
            ? zh ? `电机已应答切换到 ${formatBitrate(bitrateOutcome.bitrate)}，请重新连接主机` : `Motor acknowledged ${formatBitrate(bitrateOutcome.bitrate)}; reconnect the host`
            : zh ? "未收到可验证的切换应答，电机实际速率尚未确认" : "Bitrate change was not confirmed; actual motor bitrate is uncertain"}
          description={<Space direction="vertical" size={4}>
            <Typography.Text>
              {bitrateOutcome.can_fd
                ? zh ? `主机需使用 CAN FD，常用配置为仲裁速率 1 Mbps、数据速率 ${formatBitrate(bitrateOutcome.bitrate)}；连接后以读取结果确认。` : `Use CAN FD; the typical host setting is nominal 1 Mbps, data ${formatBitrate(bitrateOutcome.bitrate)}. Read back after connecting to verify.`
                : zh ? `主机 CAN 波特率需设为 ${formatBitrate(bitrateOutcome.bitrate)}。` : `Set the host CAN bitrate to ${formatBitrate(bitrateOutcome.bitrate)}.`}
            </Typography.Text>
            <Typography.Text>
              {zh ? "在顶部断开后，用匹配速率重新连接；选择电机自动初始化并读取确认，再保存参数。SocketCAN 的速率需在系统中配置。" : "Disconnect above, reconnect at the matching rate, select the motor to initialize and verify automatically, then save parameters. Configure SocketCAN timing in the system."}
            </Typography.Text>
          </Space>}
        />
      )}
    </Card>
  );

  const target = (): DamiaoTarget => {
    switch (mode) {
      case "Unspecified":
        throw new Error(zh ? "请先选择有效的控制模式" : "Select a defined control mode first");
      case "Mit":
        return { kind: "Mit", ...mit };
      case "PositionVelocity":
        return { kind: "PositionVelocity", ...positionVelocity };
      case "Velocity":
        return { kind: "Velocity", velocity_rad_s: velocity };
      case "ForcePosition":
        return { kind: "ForcePosition", ...forcePosition };
    }
  };

  const statusTag = state?.online ? (
    <Tag color={faulted ? "red" : state.enabled ? "green" : "blue"}>
      {statusLabel(state.status_code, zh)} · 0x{state.status_code.toString(16).toUpperCase()}
    </Tag>
  ) : (
    <Tag>{copy.offline}</Tag>
  );

  const connectionPanel = (
    <Space direction="vertical" size={12} style={{ width: "100%" }}>
      <CollapsibleWarning zh={zh} message={copy.safety} />
      {!connected && <Alert type="info" showIcon message={copy.configureFirst} />}
      <div className="damiao-config-grid">
        <Field label={copy.motorId}>
          <Input value={motorId} disabled={attached || busy} onChange={(event) => setMotorId(event.target.value)} />
        </Field>
        <Field label={copy.masterId}>
          <Input value={masterId} disabled={attached || busy} onChange={(event) => setMasterId(event.target.value)} />
        </Field>
      </div>
      <Card size="small" title={copy.mapping} extra={<Typography.Text type="secondary">{copy.mappingHint}</Typography.Text>}>
        <Space wrap>
          <Field label="PMAX (rad)">
            <InputNumber value={attached ? pMax : null} placeholder={zh ? "初始化后读取" : "Read on initialization"} disabled />
          </Field>
          <Tooltip title={copy.vmaxHint}>
            <span>
              <Field label="VMAX (rad/s)">
              <InputNumber value={attached ? vMax : null} placeholder={zh ? "初始化后读取" : "Read on initialization"} disabled />
              </Field>
            </span>
          </Tooltip>
          <Tooltip title={copy.tmaxHint}>
            <span>
              <Field label="TMAX (Nm)">
              <InputNumber value={attached ? tMax : null} placeholder={zh ? "初始化后读取" : "Read on initialization"} disabled />
              </Field>
            </span>
          </Tooltip>
        </Space>
      </Card>
      <Space wrap>
        {!attached ? (
          <Button type="primary" loading={initializing} disabled={!connected || busy} onClick={() => void initialize(false)}>
            {copy.attach}
          </Button>
        ) : (
          <Button danger loading={busy} disabled={!connected || requiresReconnect} onClick={detach}>
            {copy.detach}
          </Button>
        )}
        <Tag color={attached ? "green" : "default"}>{attached ? copy.connected : copy.detached}</Tag>
        {attached && statusTag}
      </Space>
    </Space>
  );

  const feedbackAlerts = (
    <Space direction="vertical" size={12} style={{ width: "100%" }}>
      {attached && state && !state.online && (
        <CollapsibleWarning zh={zh} message={copy.noFeedback} />
      )}
      {state && !state.online && state.rx_count > 0 && <Typography.Text type="secondary">{zh ? "当前显示最后一次反馈；收到新反馈后才允许发送运动目标。" : "Showing the last feedback. Motion targets unlock when fresh feedback resumes."}</Typography.Text>}
      {masterMismatch && state && (
        <CollapsibleWarning
          zh={zh}
          message={`${copy.masterMismatch} 0x${state.master_id.toString(16).toUpperCase()} → 0x${state.feedback_can_id!.toString(16).toUpperCase()}`}
        />
      )}
      {state?.last_error && <Alert type="error" showIcon message={state.last_error} />}
      {faulted && state && (
        <Alert type="error" showIcon message={`${copy.status}: ${statusLabel(state.status_code, zh)} (0x${state.status_code.toString(16).toUpperCase()})`} />
      )}
    </Space>
  );

  const telemetryPanel = (
    <Descriptions
      className="damiao-telemetry-table"
      bordered
      size="small"
      column={{ xs: 1, sm: 2, md: 2, lg: 2, xl: 2, xxl: 2 }}
    >
      <Descriptions.Item label={zh ? "输出轴单圈角度" : "Output-shaft single-turn angle"}>
        <Space wrap size={8}>
          <TelemetryNumber value={singleTurnAngle} suffix=" rad" />
          <Typography.Text type="secondary">
            (<TelemetryNumber value={singleTurnAngle != null ? singleTurnAngle * 180 / Math.PI : null} digits={2} suffix="°" />)
          </Typography.Text>
        </Space>
      </Descriptions.Item>
      <Descriptions.Item label={zh ? "输出轴累计位置 (rad)" : "Output-shaft cumulative position (rad)"}>
        <TelemetryNumber value={state?.output_position_rad} />
      </Descriptions.Item>
      <Descriptions.Item label={zh ? "控制位置 (rad)" : "Control position (rad)"}>
        <TelemetryNumber value={state?.position_rad} />
      </Descriptions.Item>
      <Descriptions.Item label={zh ? "编码器角度更新" : "Encoder angle age"}>
        {state?.position_read_age_ms != null ? <Tag color={state.output_position_rad != null ? "success" : "warning"}>{state.position_read_age_ms} ms</Tag> : "—"}
      </Descriptions.Item>
      <Descriptions.Item label={`${copy.speed} (rad/s)`}><TelemetryNumber value={state?.velocity_rad_s} /></Descriptions.Item>
      <Descriptions.Item label={`${copy.torque} (Nm)`}><TelemetryNumber value={state?.torque_nm} digits={3} /></Descriptions.Item>
      <Descriptions.Item label={copy.status}>{state ? statusTag : "—"}</Descriptions.Item>
      <Descriptions.Item label={`${copy.mosTemp} (°C)`}><TelemetryNumber value={state?.mos_temp_c} digits={1} /></Descriptions.Item>
      <Descriptions.Item label={`${copy.rotorTemp} (°C)`}><TelemetryNumber value={state?.rotor_temp_c} digits={1} /></Descriptions.Item>
      <Descriptions.Item label={copy.feedbackRate}>
        {state?.feedback_rate_hz != null ? <TelemetryNumber value={state.feedback_rate_hz} digits={1} suffix=" Hz" /> : "—"}
      </Descriptions.Item>
      <Descriptions.Item label={copy.feedbackAge}>
        {state?.feedback_age_ms != null ? (
          <Tag color={feedbackAgeColor(state)}>{state.feedback_age_ms} ms</Tag>
        ) : "—"}
      </Descriptions.Item>
      <Descriptions.Item label={copy.rxCount}>
        <span className="damiao-tabular">{state?.rx_count ?? 0}</span>
      </Descriptions.Item>
      <Descriptions.Item label={copy.physicalMode}>{modeLabel(state?.mode ?? mode, zh)}</Descriptions.Item>
      <Descriptions.Item label={copy.detectedMasterId}>
        {state?.feedback_can_id != null ? formatCanId(state.feedback_can_id) : "—"}
      </Descriptions.Item>
      <Descriptions.Item label="CAN">
        {state
          ? `Mode 0x7FF · Enable ${formatCanId(state.motor_id & 0xFF)} · Target ${formatCanId(controlCanId(state.motor_id, state.mode))} / DLC ${state.mode === "Velocity" ? 4 : 8}`
          : "—"}
      </Descriptions.Item>
    </Descriptions>
  );

  const controlPanel = (
    <Card
      size="small"
      title={copy.control}
      extra={busy ? <Tag color="processing">{zh ? "命令执行中" : "Working"}</Tag> : state?.streaming ? <Tag color="processing">50 Hz</Tag> : undefined}
    >
      <Space wrap align="center">
        <Typography.Text type="secondary">{copy.physicalMode}</Typography.Text>
        <Segmented
          value={mode}
          disabled={busy || requiresReconnect}
          onChange={(value) => void changeMode(value as DamiaoMode)}
          options={(mode === "Unspecified" ? [mode, ...MODE_OPTIONS] : MODE_OPTIONS).map((value) => ({ label: modeLabel(value, zh), value, disabled: value === "Unspecified" }))}
        />
        <Button type="primary" loading={initializing} disabled={!connected || busy || requiresReconnect || !!state?.enabled || faulted || mode === "Unspecified"} onClick={() => void initialize(true)}>
          {attached ? copy.enable : zh ? "初始化并使能" : "Initialize & enable"}
        </Button>
        <Button disabled={!controlsReady || busy} onClick={() => attachedMotorId != null && action(() => api.damiaoClearFault(attachedMotorId))}>
          {copy.clearFault}
        </Button>
      </Space>
      <Typography.Text className="damiao-mode-hint" type="secondary">{copy.modeLocked}</Typography.Text>

      <Divider style={{ margin: "12px 0" }} />
      <div className="damiao-target-editor">
        <Typography.Text strong>{modeLabel(mode, zh)}</Typography.Text>
        <Space wrap align="end">
          {mode === "Mit" && (
            <>
              <NumberField disabled={busy} label={`${copy.position} (rad)`} value={mit.position_rad} min={-pMax} max={pMax} step={0.01} onChange={(value) => setMit((current) => ({ ...current, position_rad: value }))} />
              <NumberField disabled={busy} label={`${copy.speed} (rad/s)`} value={mit.velocity_rad_s} min={-vMax} max={vMax} step={0.1} onChange={(value) => setMit((current) => ({ ...current, velocity_rad_s: value }))} />
              <Tooltip title={copy.tmaxHint}>
                <span>
                  <NumberField disabled={busy} label={`${copy.torque} (Nm)`} value={mit.torque_nm} min={-tMax} max={tMax} step={0.05} onChange={(value) => setMit((current) => ({ ...current, torque_nm: value }))} />
                </span>
              </Tooltip>
              <NumberField disabled={busy} label="Kp (Nm/rad)" value={mit.kp} min={0} max={500} step={0.1} onChange={(value) => setMit((current) => ({ ...current, kp: value }))} />
              <NumberField disabled={busy} label="Kd (Nm·s/rad)" value={mit.kd} min={0} max={5} step={0.01} onChange={(value) => setMit((current) => ({ ...current, kd: value }))} />
            </>
          )}
          {mode === "PositionVelocity" && (
            <>
              <NumberField disabled={busy} label={`${copy.position} (rad)`} value={positionVelocity.position_rad} step={0.01} onChange={(value) => setPositionVelocity((current) => ({ ...current, position_rad: value }))} />
              <NumberField disabled={busy} label={`${copy.posVelSpeed} (rad/s)`} value={positionVelocity.velocity_rad_s} min={0} step={0.1} onChange={(value) => setPositionVelocity((current) => ({ ...current, velocity_rad_s: value }))} />
            </>
          )}
          {mode === "Velocity" && (
            <NumberField disabled={busy} label={`${copy.speed} (rad/s)`} value={velocity} step={0.1} onChange={setVelocity} />
          )}
          {mode === "ForcePosition" && <>
            <NumberField disabled={busy} label={`${copy.position} (rad)`} value={forcePosition.position_rad} step={0.01} onChange={(value) => setForcePosition((current) => ({ ...current, position_rad: value }))} />
            <NumberField disabled={busy} label={`${copy.posVelSpeed} (rad/s)`} value={forcePosition.velocity_rad_s} min={0} max={100} step={0.01} onChange={(value) => setForcePosition((current) => ({ ...current, velocity_rad_s: value }))} />
            <NumberField disabled={busy} label={zh ? "电流限幅 (%)" : "Current limit (%)"} value={forcePosition.current_limit_ratio * 100} min={0} max={100} step={0.01} onChange={(value) => setForcePosition((current) => ({ ...current, current_limit_ratio: value / 100 }))} />
          </>}
          {mode !== "Velocity" && mode !== "Unspecified" && <Button disabled={!controlsReady || busy || !state?.online} onClick={() => action(async () => {
            if (attachedMotorId == null) return;
            const next = await api.damiaoReadPositions(attachedMotorId);
            if (next.position_rad == null) throw new Error(zh ? "精确位置尚未更新" : "Precise position is unavailable");
            if (mode === "Mit" && Math.abs(next.position_rad) > next.p_max) throw new Error(zh ? "当前位置超出 MIT 的 PMAX 编码范围，请使用位置速度模式。" : "Current position exceeds the MIT PMAX range; use position-velocity mode.");
            if (mode === "Mit") setMit((current) => ({ ...current, position_rad: next.position_rad! }));
            else if (mode === "ForcePosition") setForcePosition((current) => ({ ...current, position_rad: next.position_rad! }));
            else setPositionVelocity((current) => ({ ...current, position_rad: next.position_rad! }));
          }, zh ? "已填入当前位置，尚未发送目标" : "Current position filled in; target not sent")}>
            {zh ? "使用当前位置" : "Use current position"}
          </Button>}
        </Space>
        <Typography.Text type="secondary">{mode === "Mit"
          ? zh ? "Kp=0 时不控制位置，Kd=0 时速度目标不起作用；纯转矩控制使用 Kp=Kd=0。" : "Kp=0 ignores position; Kd=0 ignores velocity. Use Kp=Kd=0 for torque control."
          : mode === "PositionVelocity"
          ? zh ? "使用 0x50 精确控制位置（支持多圈）；最高速度为非负值，0 不产生运动。" : "Uses the precise multi-turn control position from 0x50; maximum speed is nonnegative, with 0 producing no motion."
          : mode === "ForcePosition"
          ? zh ? "使用 0x50 多圈位置；限速 0–100 rad/s，电流限幅 0–100%（实际相电流 / 最大相电流）。0% 不提供驱动电流。目标单次发送，需要调整限幅时修改后重新发送。" : "Uses multi-turn position from 0x50, speed limit 0–100 rad/s and current limit 0–100% of maximum phase current. 0% supplies no drive current. Targets are sent once; edit and send again to adjust limits."
          : mode === "Unspecified"
          ? zh ? "寄存器值 0 的控制行为未定义，请先选择上方四种有效模式之一。" : "Register value 0 has no defined control behavior. Select one of the four documented modes."
          : zh ? "正负号决定方向；速度单位 rad/s，1 rad/s ≈ 9.55 rpm。" : "The sign sets direction; velocity is in rad/s, with 1 rad/s ≈ 9.55 rpm."}</Typography.Text>
      </div>

      {mitPositionOutOfRange && <Alert type="warning" showIcon message={zh ? "位置目标超出 MIT 的 PMAX 编码范围。可切换到支持多圈浮点位置的“位置速度”模式，或重新填写可编码的位置目标。" : "The position target exceeds the MIT PMAX encoding range. Use position-velocity mode for multi-turn float positions, or enter an encodable target."} />}
      {!motionReady && <Alert type="info" showIcon message={!connected ? copy.configureFirst : requiresReconnect ? zh ? "电机 ID 或波特率已变化或尚未确认，请先重新连接 CAN 并检查通信。" : "Motor ID or bitrate changed or is uncertain; reconnect CAN and verify communication first." : attached ? copy.targetLocked : zh ? "选择电机自动初始化，或填写 ID 后点击“初始化并使能”。" : "Select a motor to initialize automatically, or enter its ID and click Initialize & enable."} />}
      <Space wrap className="damiao-target-actions">
        <Button type="primary" disabled={!motionReady || busy || mitPositionOutOfRange} onClick={() => attachedMotorId != null && action(() => api.damiaoSendTarget(attachedMotorId, target(), targetRepeats), copy.sendOk)}>
          {copy.send}
        </Button>
        <Space size={6}>
          <Switch checked={targetRepeats} disabled={busy || positionTrajectory || mode === "Unspecified"} onChange={setRepeat} />
          <Typography.Text>{positionTrajectory ? copy.positionOneShot : copy.periodic}</Typography.Text>
        </Space>
        <Tooltip title={zh ? "仅停止刷新目标，电机会继续执行最后一个目标；停止运动请使用顶部“停止并失能”。" : "Stops refreshing the target; the motor can continue its last command. Use Stop & disable above to stop motion."}>
          <Button disabled={!controlsReady || busy || !state?.streaming} onClick={() => attachedMotorId != null && action(() => api.damiaoStopStream(attachedMotorId))}>{copy.stopStream}</Button>
        </Tooltip>
        {state?.streaming && <Tag color="processing">50 Hz</Tag>}
      </Space>
    </Card>
  );

  return (
    <div className="damiao-panel">
      <Space direction="vertical" size={12} style={{ width: "100%" }}>
        <Card size="small" className="damiao-summary-card">
          <div className="damiao-summary-card__content">
            <Space direction="vertical" size={3}>
              <Space align="center" wrap>
                <Typography.Title level={4} style={{ margin: 0 }}>DM-J4310-2EC</Typography.Title>
                <Typography.Text code>{formatCanId(state?.motor_id ?? initialMotorId)}</Typography.Text>
                <Tag color={attached ? "blue" : "default"}>{attached ? copy.connected : copy.detached}</Tag>
                {attached && statusTag}
                <Tag>{modeLabel(state?.mode ?? mode, zh)}</Tag>
              </Space>
              <Typography.Text type="secondary" className="damiao-summary-card__meta">
                {state
                  ? `RX ${state.feedback_can_id != null ? formatCanId(state.feedback_can_id) : "—"} · ${state.feedback_rate_hz != null ? `${state.feedback_rate_hz.toFixed(1)} Hz` : "— Hz"}`
                  : zh ? "24 V / 48 V · 标准 CAN · 选择电机自动初始化" : "24 V / 48 V · Standard CAN · Select a motor to initialize"}
              </Typography.Text>
            </Space>
            <Space size={8} wrap>
              <Button danger disabled={!controlsReady || busy} onClick={() => attachedMotorId != null && action(() => api.damiaoDisable(attachedMotorId), zh ? "电机已失能，反馈已确认" : "Motor disabled; feedback confirmed")}>
                {copy.disable}
              </Button>
              <Typography.Text type="secondary">{copy.refresh}</Typography.Text>
              <Tooltip title={copy.refreshHint}><Typography.Text type="secondary"><QuestionCircleOutlined /></Typography.Text></Tooltip>
              <Segmented
                size="small"
                value={rateHz}
                onChange={(value) => setRateHz(value as number)}
                options={[
                  { label: copy.refreshLow, value: 50 },
                  { label: copy.refreshHigh, value: 100 },
                ]}
              />
            </Space>
          </div>
        </Card>

        {operationError && <Alert type="error" showIcon message={zh ? "操作未完成" : "Operation not completed"} description={operationError} />}
        {requiresReconnect && <Alert type="warning" showIcon message={zh ? "通信配置已变化或尚未确认，请在顶部重新连接 CAN。" : "Communication settings changed or are uncertain. Reconnect CAN above."} />}
        {!attached && !initialDevice && (
          <Card size="small" title={zh ? "手动连接电机" : "Connect a motor manually"}>
            <Space wrap align="end">
              <Field label={copy.motorId}><Input value={motorId} disabled={busy} onChange={(event) => setMotorId(event.target.value)} style={{ width: 150 }} /></Field>
              <Button disabled={!connected || busy} loading={initializing} onClick={() => void initialize(false)}>{copy.attach}</Button>
              <Typography.Text type="secondary">{zh ? "支持十进制或 0x 十六进制 ID，反馈 ID 自动识别。" : "Enter a decimal or 0x hexadecimal ID; feedback ID is detected automatically."}</Typography.Text>
            </Space>
          </Card>
        )}
        {initializing && <Alert type="info" showIcon message={zh ? "正在读取电机参数并确认状态…" : "Reading motor parameters and confirming status…"} />}

        <div hidden={workspaceTab !== "settings"}>
          <DamiaoRegisterPanel motorId={attachedMotorId} ready={controlsReady} busy={busy} zh={zh}
            runOperation={(operation) => run("", operation)} onWrite={writeRegister} />
        </div>

        {workspaceTab === "settings" && <div>
          <Space direction="vertical" size={12} style={{ width: "100%" }}>
            {!attached && <Button type="primary" loading={initializing} disabled={!connected || busy || requiresReconnect} onClick={() => void initialize(false)}>{copy.attach}</Button>}
            {idPanel}
            {bitratePanel}
            {zeroPanel}
            <Card size="small" title={zh ? "保存配置" : "Save configuration"}>
              <Space direction="vertical" size={10}>
                <Typography.Text type="secondary">{zh
                  ? "寄存器写入即时生效，通常尚未保存到 Flash。保存会先失能，再把当前全部参数写入 Flash，断电后保留；切换波特率后请先按新速率重新连接并读取确认。Flash 擦写约 1 万次，不要频繁保存。"
                  : "Register writes apply immediately and are normally volatile. Saving disables the motor and stores all parameters in Flash. After a bitrate change, reconnect at the new rate and verify first. Flash supports about 10,000 erase cycles; avoid frequent saves."}</Typography.Text>
                <Button disabled={!controlsReady || busy} onClick={() => void saveParameters()}>{zh ? "失能并保存全部参数" : "Disable & save all parameters"}</Button>
              </Space>
            </Card>
            <Collapse items={[{ key: "connection", label: copy.connection, children: connectionPanel }]} />
          </Space>
        </div>}

        {workspaceTab === "control" && <div>
          <Space direction="vertical" size={12} style={{ width: "100%" }}>
        {controlPanel}

        <Card
          size="small"
          title={copy.display}
          extra={
            <Space>
              {view === "chart" && (
                <Space size={4}>
                  <Typography.Text type="secondary">{copy.window}</Typography.Text>
                  <InputNumber
                    size="small"
                    min={1}
                    max={60}
                    value={windowSec}
                    onChange={(value) => setWindowSec(value ?? 10)}
                    style={{ width: 70 }}
                  />
                  <Typography.Text type="secondary">s</Typography.Text>
                </Space>
              )}
              <Segmented
                value={view}
                onChange={(value) => setView(value as "panel" | "chart")}
                options={[
                  { label: copy.numeric, value: "panel" },
                  { label: copy.chart, value: "chart" },
                ]}
              />
            </Space>
          }
        >
          <Space direction="vertical" size={12} style={{ width: "100%" }}>
            {((attached && state && !state.online) || masterMismatch || state?.last_error || faulted) && feedbackAlerts}
            <Typography.Text type="secondary">
              {zh
                ? "单圈角度显示为 −180～180°（−π～π rad），以当前零点为参考；累计位置保留转动圈数。运动目标使用控制位置坐标。"
                : "Single-turn angle is shown from −180 to 180° (−π to π rad) relative to the current zero. Cumulative positions retain turns; motion targets use the control coordinate."}
            </Typography.Text>
            {state?.position_read_error && (
              <Alert type="warning" showIcon message={zh ? "输出轴角度尚未更新" : "Output-shaft angle not updated"} description={state.position_read_error} />
            )}
            {view === "panel" ? telemetryPanel : (
              <DamiaoLiveChart samples={samples} chartVersion={chartVersion} windowSec={windowSec} zh={zh} />
            )}
          </Space>
        </Card>

          </Space>
        </div>}
      </Space>
    </div>
  );
}

function CollapsibleWarning({
  message,
  description,
  zh,
}: {
  message: ReactNode;
  description?: ReactNode;
  zh: boolean;
}) {
  return (
    <Collapse
      size="small"
      items={[{
        key: "warning",
        label: <Typography.Text type="warning">{zh ? "注意" : "Caution"}</Typography.Text>,
        children: <Alert type="warning" showIcon message={message} description={description} />,
      }]}
    />
  );
}

function Field({ label, children, className }: { label: ReactNode; children: ReactNode; className?: string }) {
  return (
    <label className={`damiao-field${className ? ` ${className}` : ""}`}>
      <Typography.Text type="secondary">{label}</Typography.Text>
      {children}
    </label>
  );
}

function NumberField({ label, value, onChange, ...props }: { label: ReactNode; value: number; onChange: (value: number) => void; min?: number; max?: number; step?: number; disabled?: boolean }) {
  return (
    <Field label={label}>
      <InputNumber value={value} onChange={(next) => onChange(next ?? 0)} {...props} />
    </Field>
  );
}

function parseCanId(raw: string, max: number, label: string): number {
  const text = raw.trim();
  if (!/^(?:0x[0-9a-f]+|\d+)$/i.test(text)) throw new Error(`${label}: ${raw}`);
  const value = Number(text);
  if (!Number.isInteger(value) || value < 0 || value > max) {
    throw new Error(`${label}: 0x000–0x${max.toString(16).toUpperCase()}`);
  }
  return value;
}

function modeLabel(mode: DamiaoMode, zh: boolean): string {
  switch (mode) {
    case "Unspecified":
      return zh ? "未定义 (0)" : "Undefined (0)";
    case "Mit":
      return "MIT";
    case "PositionVelocity":
      return zh ? "位置速度" : "Position + velocity";
    case "Velocity":
      return zh ? "速度" : "Velocity";
    case "ForcePosition":
      return zh ? "力位混控" : "Force + position";
  }
}

function statusLabel(code: number, zh: boolean): string {
  const labels: Record<number, [string, string]> = {
    0x0: ["Disabled", "失能"],
    0x1: ["Enabled", "使能"],
    0x3: ["Output-shaft calibration fault", "输出轴标定异常"],
    0x4: ["Output sensor fault", "输出传感器异常"],
    0x5: ["Motor encoder calibration fault", "电机编码器标定异常"],
    0x8: ["Over-voltage", "过压"],
    0x9: ["Under-voltage", "欠压"],
    0xa: ["Over-current", "过流"],
    0xb: ["MOS over-temperature", "MOS 过温"],
    0xc: ["Motor over-temperature", "线圈过温"],
    0xd: ["Communication lost", "通讯丢失"],
    0xe: ["Overload", "过载"],
  };
  const pair = labels[code] ?? ["Unknown", "未知"];
  return zh ? pair[1] : pair[0];
}

function TelemetryNumber({
  value,
  digits = 4,
  suffix = "",
}: {
  value: number | null | undefined;
  digits?: number;
  suffix?: string;
}) {
  if (value == null || !Number.isFinite(value)) return <span>—</span>;
  const rounded = Number(value.toFixed(digits));
  const sign = rounded < 0 ? "-" : " ";
  return (
    <span className="damiao-tabular">
      {sign}{Math.abs(rounded).toFixed(digits)}{suffix}
    </span>
  );
}

function feedbackAgeColor(state: DamiaoState): string {
  if (!state.online) return "default";
  const age = state.feedback_age_ms ?? Number.POSITIVE_INFINITY;
  if (age <= 100) return "green";
  if (age <= 300) return "gold";
  return "orange";
}

function formatCanId(value: number): string {
  return `0x${value.toString(16).toUpperCase().padStart(2, "0")}`;
}

function formatBitrate(value: number): string {
  return value >= 1_000_000 ? `${value / 1_000_000} Mbps` : `${value / 1_000} kbps`;
}

function controlCanId(motorId: number, mode: DamiaoMode): number {
  return (motorId & 0xFF) + (mode === "PositionVelocity" ? 0x100 : mode === "Velocity" ? 0x200 : mode === "ForcePosition" ? 0x300 : 0);
}
