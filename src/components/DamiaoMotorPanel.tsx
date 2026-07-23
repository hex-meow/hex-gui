import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { QuestionCircleOutlined } from "@ant-design/icons";
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
  Space,
  Switch,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import { useDamiaoTelemetry } from "../useDamiaoTelemetry";
import type { DamiaoConfig, DamiaoDiscoveredDevice, DamiaoMode, DamiaoState, DamiaoTarget } from "../types";
import { DamiaoLiveChart } from "./DamiaoLiveChart";
import "./DamiaoMotorPanel.css";

const MODE_OPTIONS: DamiaoMode[] = ["Mit", "PositionVelocity", "Velocity"];
const PHYSICAL_MAX_SPEED_RAD_S = (200 * 2 * Math.PI) / 60;
const PHYSICAL_PEAK_TORQUE_NM = 7;
const DEVICE_POLL_MS = 500;

export function DamiaoMotorPanel({ connected }: { connected: boolean }) {
  const { message } = App.useApp();
  const { lang } = useI18n();
  const zh = lang === "zh";
  const [devices, setDevices] = useState<DamiaoDiscoveredDevice[]>([]);
  const [selectedMotorId, setSelectedMotorId] = useState<number | null>(null);
  const [manualKey, setManualKey] = useState(0);
  const [manual, setManual] = useState(false);

  const refreshDevices = useCallback(async () => {
    if (!connected) {
      setDevices([]);
      return;
    }
    const next = await api.damiaoListDevices();
    setDevices(next);
  }, [connected]);

  useEffect(() => {
    if (!connected) {
      setDevices([]);
      setSelectedMotorId(null);
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const next = await api.damiaoListDevices();
        if (alive) setDevices(next);
      } catch {
        // Discovery is best-effort and the next poll retries automatically.
      }
    };
    tick();
    const timer = window.setInterval(tick, DEVICE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [connected]);

  useEffect(() => {
    if (!manual && selectedMotorId == null && devices.length > 0) {
      setSelectedMotorId(devices[0].motor_id);
    }
  }, [devices, manual, selectedMotorId]);

  const selectedDevice = devices.find((device) => device.motor_id === selectedMotorId);
  const attachedCount = devices.filter((device) => device.attached).length;
  const copy = zh
    ? {
        motors: "达妙电机",
        automatic: "自动发现运行中",
        waiting: "正在安全扫描 ID 0x0–0xF…",
        manual: "手动配置 ID",
        rescan: "安全重扫",
        rescanOk: "已请求安全重扫（仅失能未挂载 ID）",
        disableAll: "全部失能",
        disableAllOk: "所有已挂载电机均已失能",
        attached: "已挂载",
        online: "在线",
        offline: "离线",
      }
    : {
        motors: "DAMIAO motors",
        automatic: "Automatic discovery running",
        waiting: "Safely scanning IDs 0x0–0xF…",
        manual: "Configure an ID manually",
        rescan: "Safe rescan",
        rescanOk: "Safe rescan requested (only unattached IDs are disabled)",
        disableAll: "Disable all",
        disableAllOk: "All attached motors disabled",
        attached: "Attached",
        online: "Online",
        offline: "Offline",
      };

  const rescan = async () => {
    try {
      await api.damiaoSafeRescan();
      message.success(copy.rescanOk);
    } catch (error) {
      message.error(errMsg(error));
    }
  };

  const disableAll = async () => {
    try {
      await api.damiaoDisableAll();
      message.success(copy.disableAllOk);
    } catch (error) {
      message.error(errMsg(error));
    }
  };

  return (
    <div className="damiao-workspace">
      <aside className="damiao-device-list">
        <div className="damiao-device-list__heading">
          <div>
            <Typography.Text strong>{copy.motors} ({devices.length})</Typography.Text>
            <Typography.Text type="secondary">{copy.automatic}</Typography.Text>
          </div>
          <Space size={6} wrap>
            <Button size="small" disabled={!connected} onClick={rescan}>{copy.rescan}</Button>
            <Button size="small" danger disabled={!connected || attachedCount === 0} onClick={disableAll}>{copy.disableAll}</Button>
          </Space>
        </div>
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
                  onClick={() => {
                    setManual(false);
                    setSelectedMotorId(device.motor_id);
                  }}
                >
                  <div>
                    <Space style={{ justifyContent: "space-between", width: "100%" }}>
                      <Typography.Text strong>DM-J4310-2EC V1.1</Typography.Text>
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
          key={manual ? `manual-${manualKey}` : `motor-${selectedDevice?.motor_id ?? "default"}`}
          connected={connected}
          initialDevice={manual ? undefined : selectedDevice}
          onDevicesChanged={refreshDevices}
        />
      </section>
    </div>
  );
}

function DamiaoMotorDetail({
  connected,
  initialDevice,
  onDevicesChanged,
}: {
  connected: boolean;
  initialDevice?: DamiaoDiscoveredDevice;
  onDevicesChanged: () => Promise<void>;
}) {
  const { message } = App.useApp();
  const { lang } = useI18n();
  const zh = lang === "zh";

  const initialMotorId = initialDevice?.motor_id ?? 1;
  const [motorId, setMotorId] = useState(formatCanId(initialMotorId));
  const [masterId, setMasterId] = useState(formatCanId(initialDevice?.feedback_can_id ?? 0));
  const [mode, setMode] = useState<DamiaoMode>("Mit");
  const [pMax, setPMax] = useState(12.5);
  const [vMax, setVMax] = useState(30);
  const [tMax, setTMax] = useState(10);
  const [state, setState] = useState<DamiaoState | null>(null);
  const [busy, setBusy] = useState(false);
  const [repeat, setRepeat] = useState(true);
  const [rateHz, setRateHz] = useState(50);
  const [view, setView] = useState<"panel" | "chart">("panel");
  const [windowSec, setWindowSec] = useState(10);
  const targetRepeats = repeat && mode !== "PositionVelocity";

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

  const copy = useMemo(
    () =>
      zh
        ? {
            attach: "挂载电机",
            detach: "安全失能并卸载",
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
            connected: "已挂载",
            detached: "未挂载",
            online: "反馈在线",
            offline: "无新反馈",
            motorId: "电机 CAN ID",
            masterId: "反馈 Master ID",
            detectedMasterId: "实际反馈 ID",
            physicalMode: "运行时控制模式",
            mapping: "MIT 映射范围",
            mappingHint: "必须与达妙调试助手内的 PMAX / VMAX / TMAX 完全一致。",
            attachOk: "已挂载 DM-J4310-2EC V1.1（尚未使能）",
            detachOk: "电机已失能并卸载",
            actionOk: "命令已发送",
            modeSwitchOk: "控制模式已切换，原定时目标已停止",
            sendOk: targetRepeats ? "目标已发送并以 50 Hz 持续更新" : "目标已发送一次",
            stopStream: "停止定时发送",
            enable: "使能",
            disable: "失能",
            clearFault: "清故障",
            setZero: "失能并保存当前位置为零点",
            setZeroConfirm: "该操作会先失能电机，再永久保存当前位置为零点。确定继续吗？",
            send: "发送目标",
            periodic: "50 Hz 定时发送",
            positionOneShot: "位置速度目标只发送一次，由驱动器内部完成梯形轨迹",
            position: "位置",
            speed: "速度",
            torque: "前馈转矩",
            mosTemp: "MOS 温度",
            rotorTemp: "线圈温度",
            status: "状态",
            feedbackAge: "反馈时延",
            feedbackRate: "反馈频率",
            rxCount: "反馈帧数",
            configureFirst: "请先连接 CAN，再挂载电机。挂载时只发送一次安全失能探测，不会使能或运动。",
            noFeedback:
              "安全失能探测后仍未收到反馈。请确认 24 V 供电、CAN_H/CAN_L、共地、双端 120 Ω、1 Mbps，以及电机 ID。",
            masterMismatch: "检测到的反馈 ID 与填写的 Master ID 不同；已自动接收，请按检测值更新电机或界面配置。",
            safety:
              "DM-J4310-2EC V1.1 使用 24 V、CAN 标准帧、1 Mbps。请使用限流电源并固定电机；空载转矩模式也会迅速加速。挂载后可在下方运行时切换控制模式，切换过程会短暂失能。",
            modeLocked:
              "挂载后也可直接切换。切换会停止原定时目标、先安全失能，再通过 0x7FF 临时写入模式；若切换前已使能，会自动恢复使能。模式不会保存到 Flash，速度模式严格发送 4 字节目标帧。",
            targetLocked: "收到“使能”反馈后才允许发送运动目标。",
            tmaxHint:
              "TMAX 是协议映射范围，不等于安全工作转矩。该型号额定 3 Nm、峰值 7 Nm；界面把前馈转矩限制在 ±7 Nm。",
            vmaxHint:
              "VMAX 是协议映射范围。该型号空载最大转速 200 rpm（约 20.94 rad/s），运动命令不会超过该值。",
            posVelSpeed: "最高速度",
          }
        : {
            attach: "Attach motor",
            detach: "Safe disable & detach",
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
            connected: "Attached",
            detached: "Detached",
            online: "Feedback online",
            offline: "No fresh feedback",
            motorId: "Motor CAN ID",
            masterId: "Feedback Master ID",
            detectedMasterId: "Detected feedback ID",
            physicalMode: "Runtime control mode",
            mapping: "MIT mapping ranges",
            mappingHint: "These must exactly match PMAX / VMAX / TMAX in DAMIAO Assistant.",
            attachOk: "DM-J4310-2EC V1.1 attached (not enabled)",
            detachOk: "Motor disabled and detached",
            actionOk: "Command sent",
            modeSwitchOk: "Control mode changed; the previous periodic target was stopped",
            sendOk: targetRepeats ? "Target sent and streaming at 50 Hz" : "Target sent once",
            stopStream: "Stop periodic send",
            enable: "Enable",
            disable: "Disable",
            clearFault: "Clear fault",
            setZero: "Disable and save current zero",
            setZeroConfirm: "This disables the motor, then permanently saves the current position as zero. Continue?",
            send: "Send target",
            periodic: "Periodic send at 50 Hz",
            positionOneShot: "Position-velocity goals are sent once; the drive completes the internal trapezoidal trajectory",
            position: "Position",
            speed: "Velocity",
            torque: "Feed-forward torque",
            mosTemp: "MOS temperature",
            rotorTemp: "Winding temperature",
            status: "Status",
            feedbackAge: "Feedback age",
            feedbackRate: "Feedback rate",
            rxCount: "Feedback frames",
            configureFirst: "Connect CAN, then attach the motor. Attach sends one safe disable probe; it never enables motion.",
            noFeedback:
              "No feedback after the safe disable probe. Check 24 V power, CAN_H/CAN_L, common ground, dual 120 Ω termination, 1 Mbps, and the motor ID.",
            masterMismatch: "The detected feedback ID differs from the configured Master ID. It is being received automatically; update the drive or this field to match.",
            safety:
              "DM-J4310-2EC V1.1 uses 24 V, standard CAN frames, and 1 Mbps. Use a current-limited supply and secure the motor; unloaded torque mode can accelerate rapidly. The mode can be changed after attaching; switching briefly disables the drive.",
            modeLocked:
              "You can switch after attaching. The transaction stops the previous periodic target, safely disables the motor, and writes the volatile mode through 0x7FF. If it was enabled, enable is restored automatically. Velocity targets use an exact 4-byte frame.",
            targetLocked: "Motion targets unlock after an Enabled feedback frame is received.",
            tmaxHint:
              "TMAX is a protocol mapping range, not a safe torque. This motor is rated 3 Nm and peaks at 7 Nm; feed-forward torque is limited to ±7 Nm here.",
            vmaxHint:
              "VMAX is a protocol mapping range. The motor's no-load maximum is 200 rpm (about 20.94 rad/s), which this panel enforces.",
            posVelSpeed: "Maximum speed",
          },
    [targetRepeats, zh],
  );

  const attached = !!state?.attached || !!initialDevice?.attached;
  const attachedMotorId = state?.attached
    ? state.motor_id
    : initialDevice?.attached
      ? initialMotorId
      : null;
  const { latest, samples, chartVersion } = useDamiaoTelemetry(
    attachedMotorId,
    connected,
    rateHz,
    view === "chart",
  );
  const faulted = (state?.status_code ?? 0) >= 8;
  const masterMismatch = state?.feedback_can_id != null && state.feedback_can_id !== state.master_id;
  const commandSpeedMax = Math.min(vMax, PHYSICAL_MAX_SPEED_RAD_S);
  const commandTorqueMax = Math.min(tMax, PHYSICAL_PEAK_TORQUE_NM);

  useEffect(() => {
    if (!connected) {
      setState(null);
      return;
    }
    if (latest == null) return;
    setState(latest);
    setMotorId(formatCanId(latest.motor_id));
    setMasterId(formatCanId(latest.master_id));
    setMode(latest.mode);
    setPMax(latest.p_max);
    setVMax(latest.v_max);
    setTMax(latest.t_max);
  }, [connected, latest]);

  const run = useCallback(
    async (success: string, action: () => Promise<unknown>) => {
      setBusy(true);
      try {
        await action();
        message.success(success);
      } catch (error) {
        message.error(errMsg(error));
        throw error;
      } finally {
        setBusy(false);
      }
    },
    [message],
  );

  const attach = async () => {
    let config: DamiaoConfig;
    try {
      config = {
        motor_id: parseCanId(motorId, 0xff, copy.motorId),
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
      await run(copy.attachOk, async () => {
        setState(await api.damiaoAttach(config));
        await onDevicesChanged().catch(() => {});
      });
    } catch {
      // `run` already presented the backend error.
    }
  };

  const detach = async () => {
    if (attachedMotorId == null) return;
    try {
      await run(copy.detachOk, async () => {
        await api.damiaoDetach(attachedMotorId);
        setState(null);
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
        setState(next);
        setMode(next.mode);
      });
    } catch {
      // The backend commits the new encoder/ID state as soon as the mode
      // write succeeds, even if restoring enable later fails. Re-read it so
      // the UI never sends a target using a stale local mode.
      try {
        const latest = await api.damiaoGetState(attachedMotorId);
        setState(latest);
        setMode(latest.mode);
      } catch {
        setMode(previousMode);
      }
    }
  };

  const action = async (fn: () => Promise<unknown>, success = copy.actionOk) => {
    try {
      await run(success, fn);
    } catch {
      // Error was surfaced by `run`.
    }
  };

  const target = (): DamiaoTarget => {
    switch (mode) {
      case "Mit":
        return { kind: "Mit", ...mit };
      case "PositionVelocity":
        return { kind: "PositionVelocity", ...positionVelocity };
      case "Velocity":
        return { kind: "Velocity", velocity_rad_s: velocity };
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
      <Alert type="warning" showIcon message={copy.safety} />
      {!connected && <Alert type="info" showIcon message={copy.configureFirst} />}
      <div className="damiao-config-grid">
        <Field label={copy.motorId}>
          <Input value={motorId} disabled={attached} onChange={(event) => setMotorId(event.target.value)} />
        </Field>
        <Field label={copy.masterId}>
          <Input value={masterId} disabled={attached} onChange={(event) => setMasterId(event.target.value)} />
        </Field>
      </div>
      <Card size="small" title={copy.mapping} extra={<Typography.Text type="secondary">{copy.mappingHint}</Typography.Text>}>
        <Space wrap>
          <Field label="PMAX (rad)">
            <InputNumber min={0.001} value={pMax} disabled={attached} onChange={(value) => setPMax(value ?? 12.5)} />
          </Field>
          <Tooltip title={copy.vmaxHint}>
            <span>
              <Field label="VMAX (rad/s)">
                <InputNumber min={0.001} value={vMax} disabled={attached} onChange={(value) => setVMax(value ?? 30)} />
              </Field>
            </span>
          </Tooltip>
          <Tooltip title={copy.tmaxHint}>
            <span>
              <Field label="TMAX (Nm)">
                <InputNumber min={0.001} value={tMax} disabled={attached} onChange={(value) => setTMax(value ?? 10)} />
              </Field>
            </span>
          </Tooltip>
        </Space>
      </Card>
      <Space wrap>
        {!attached ? (
          <Button type="primary" loading={busy} disabled={!connected} onClick={attach}>
            {copy.attach}
          </Button>
        ) : (
          <Button danger loading={busy} onClick={detach}>
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
      {attached && state && !state.online && <Alert type="warning" showIcon message={copy.noFeedback} />}
      {masterMismatch && state && (
        <Alert
          type="warning"
          showIcon
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
      column={2}
    >
      <Descriptions.Item label={`${copy.position} (rad)`}><TelemetryNumber value={state?.position_rad} /></Descriptions.Item>
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
          ? `Mode 0x7FF · Enable ${formatCanId(state.motor_id)} · Target ${formatCanId(controlCanId(state.motor_id, state.mode))} / DLC ${state.mode === "Velocity" ? 4 : 8}`
          : "—"}
      </Descriptions.Item>
    </Descriptions>
  );

  const controlPanel = (
    <Card
      size="small"
      title={copy.control}
      extra={state?.streaming ? <Tag color="processing">50 Hz</Tag> : undefined}
    >
      <Space wrap align="center">
        <Typography.Text type="secondary">{copy.physicalMode}</Typography.Text>
        <Segmented
          value={mode}
          disabled={busy}
          onChange={(value) => void changeMode(value as DamiaoMode)}
          options={MODE_OPTIONS.map((value) => ({ label: modeLabel(value, zh), value }))}
        />
        <Button type="primary" disabled={attachedMotorId == null || !!state?.enabled} loading={busy} onClick={() => attachedMotorId != null && action(() => api.damiaoEnable(attachedMotorId))}>
          {copy.enable}
        </Button>
        <Button danger disabled={attachedMotorId == null} loading={busy} onClick={() => attachedMotorId != null && action(() => api.damiaoDisable(attachedMotorId))}>
          {copy.disable}
        </Button>
        <Button disabled={attachedMotorId == null} loading={busy} onClick={() => attachedMotorId != null && action(() => api.damiaoClearFault(attachedMotorId))}>
          {copy.clearFault}
        </Button>
        <Popconfirm title={copy.setZeroConfirm} okText={zh ? "确定" : "Continue"} cancelText={zh ? "取消" : "Cancel"} onConfirm={() => attachedMotorId != null && action(() => api.damiaoSetZero(attachedMotorId))}>
          <Button disabled={attachedMotorId == null} loading={busy}>{copy.setZero}</Button>
        </Popconfirm>
      </Space>
      <Typography.Text className="damiao-mode-hint" type="secondary">{copy.modeLocked}</Typography.Text>

      <Divider style={{ margin: "12px 0" }} />
      <div className="damiao-target-editor">
        <Typography.Text strong>{modeLabel(mode, zh)}</Typography.Text>
        <Space wrap align="end">
          {mode === "Mit" && (
            <>
              <NumberField label={`${copy.position} (rad)`} value={mit.position_rad} min={-pMax} max={pMax} step={0.01} onChange={(value) => setMit((current) => ({ ...current, position_rad: value }))} />
              <NumberField label={`${copy.speed} (rad/s)`} value={mit.velocity_rad_s} min={-commandSpeedMax} max={commandSpeedMax} step={0.1} onChange={(value) => setMit((current) => ({ ...current, velocity_rad_s: value }))} />
              <Tooltip title={copy.tmaxHint}>
                <span>
                  <NumberField label={`${copy.torque} (Nm)`} value={mit.torque_nm} min={-commandTorqueMax} max={commandTorqueMax} step={0.05} onChange={(value) => setMit((current) => ({ ...current, torque_nm: value }))} />
                </span>
              </Tooltip>
              <NumberField label="Kp (Nm/rad)" value={mit.kp} min={0} max={500} step={0.1} onChange={(value) => setMit((current) => ({ ...current, kp: value }))} />
              <NumberField label="Kd (Nm·s/rad)" value={mit.kd} min={0} max={5} step={0.01} onChange={(value) => setMit((current) => ({ ...current, kd: value }))} />
            </>
          )}
          {mode === "PositionVelocity" && (
            <>
              <NumberField label={`${copy.position} (rad)`} value={positionVelocity.position_rad} step={0.01} onChange={(value) => setPositionVelocity((current) => ({ ...current, position_rad: value }))} />
              <NumberField label={`${copy.posVelSpeed} (rad/s)`} value={positionVelocity.velocity_rad_s} min={0} max={commandSpeedMax} step={0.1} onChange={(value) => setPositionVelocity((current) => ({ ...current, velocity_rad_s: value }))} />
            </>
          )}
          {mode === "Velocity" && (
            <NumberField label={`${copy.speed} (rad/s)`} value={velocity} min={-commandSpeedMax} max={commandSpeedMax} step={0.1} onChange={setVelocity} />
          )}
        </Space>
      </div>

      {!state?.enabled && <Alert type="info" showIcon message={copy.targetLocked} />}
      <Space wrap className="damiao-target-actions">
        <Button type="primary" disabled={attachedMotorId == null || !state?.enabled || faulted} loading={busy} onClick={() => attachedMotorId != null && action(() => api.damiaoSendTarget(attachedMotorId, target(), targetRepeats), copy.sendOk)}>
          {copy.send}
        </Button>
        <Space size={6}>
          <Switch checked={targetRepeats} disabled={mode === "PositionVelocity"} onChange={setRepeat} />
          <Typography.Text>{mode === "PositionVelocity" ? copy.positionOneShot : copy.periodic}</Typography.Text>
        </Space>
        <Button disabled={attachedMotorId == null || !state?.streaming} onClick={() => attachedMotorId != null && action(() => api.damiaoStopStream(attachedMotorId))}>
          {copy.stopStream}
        </Button>
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
                <Typography.Title level={4} style={{ margin: 0 }}>DM-J4310-2EC V1.1</Typography.Title>
                <Typography.Text code>{formatCanId(state?.motor_id ?? initialMotorId)}</Typography.Text>
                <Tag color={attached ? "blue" : "default"}>{attached ? copy.connected : copy.detached}</Tag>
                {attached && statusTag}
                <Tag>{modeLabel(state?.mode ?? mode, zh)}</Tag>
              </Space>
              <Typography.Text type="secondary" className="damiao-summary-card__meta">
                {state
                  ? `RX ${state.feedback_can_id != null ? formatCanId(state.feedback_can_id) : "—"} · Target ${formatCanId(controlCanId(state.motor_id, state.mode))} / DLC ${state.mode === "Velocity" ? 4 : 8} · ${state.feedback_rate_hz != null ? `${state.feedback_rate_hz.toFixed(1)} Hz` : "— Hz"}`
                  : `24 V · CAN 2.0A · 1 Mbps · ${modeLabel(mode, zh)}`}
              </Typography.Text>
            </Space>
            <Space size={5}>
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

      <Collapse
        defaultActiveKey={attached ? [] : ["connection"]}
        items={[
          { key: "connection", label: copy.connection, extra: attached ? <Tag color="green">{copy.connected}</Tag> : undefined, children: connectionPanel },
        ]}
      />

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
            {feedbackAlerts}
            {view === "panel" ? telemetryPanel : (
              <DamiaoLiveChart samples={samples} chartVersion={chartVersion} windowSec={windowSec} zh={zh} />
            )}
          </Space>
        </Card>

        {controlPanel}
      </Space>
    </div>
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

function NumberField({ label, value, onChange, ...props }: { label: ReactNode; value: number; onChange: (value: number) => void; min?: number; max?: number; step?: number }) {
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
    case "Mit":
      return "MIT";
    case "PositionVelocity":
      return zh ? "位置速度" : "Position + velocity";
    case "Velocity":
      return zh ? "速度" : "Velocity";
  }
}

function statusLabel(code: number, zh: boolean): string {
  const labels: Record<number, [string, string]> = {
    0x0: ["Disabled", "失能"],
    0x1: ["Enabled", "使能"],
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

function controlCanId(motorId: number, mode: DamiaoMode): number {
  return motorId + (mode === "Mit" ? 0 : mode === "PositionVelocity" ? 0x100 : 0x200);
}
