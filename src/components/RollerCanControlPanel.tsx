import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
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
  Segmented,
  Space,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import type {
  RollerCanControlDevice,
  RollerCanControlMode,
  RollerCanControlState,
  RollerCanControlTarget,
} from "../types";
import { useRollerCanControlTelemetry } from "../useRollerCanControlTelemetry";
import { RollerCanControlChart } from "./RollerCanControlChart";
import "./RollerCanControlPanel.css";

const MODE_OPTIONS: RollerCanControlMode[] = ["Speed", "Position", "Current", "Encoder"];
const DEVICE_POLL_MS = 500;

export function RollerCanControlPanel({ connected }: { connected: boolean }) {
  const { message } = App.useApp();
  const { lang } = useI18n();
  const zh = lang === "zh";
  const [devices, setDevices] = useState<RollerCanControlDevice[]>([]);
  const [selectedNodeId, setSelectedNodeId] = useState<number | null>(null);
  const [manual, setManual] = useState(false);
  const [manualKey, setManualKey] = useState(0);

  const copy = zh
    ? {
        motors: "RollerCAN 电机",
        discovery: "扩展帧设备发现",
        waiting: "正在扫描 ID 0x00–0xFF…",
        rescan: "重新扫描",
        rescanOk: "已请求重新扫描",
        manual: "手动填写 ID",
        online: "在线",
        offline: "离线",
        attached: "已挂载",
        enabled: "已使能",
      }
    : {
        motors: "RollerCAN motors",
        discovery: "Extended-frame discovery",
        waiting: "Scanning IDs 0x00–0xFF…",
        rescan: "Rescan",
        rescanOk: "A new scan was requested",
        manual: "Enter an ID manually",
        online: "Online",
        offline: "Offline",
        attached: "Attached",
        enabled: "Enabled",
      };

  const refreshDevices = useCallback(async () => {
    if (!connected) {
      setDevices([]);
      return;
    }
    setDevices(await api.rollerCanControlListDevices());
  }, [connected]);

  useEffect(() => {
    if (!connected) {
      setDevices([]);
      setSelectedNodeId(null);
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const next = await api.rollerCanControlListDevices();
        if (alive) setDevices(next);
      } catch {
        // Discovery is best-effort and the next poll retries.
      }
    };
    void tick();
    const timer = window.setInterval(tick, DEVICE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [connected]);

  useEffect(() => {
    if (!manual && selectedNodeId == null && devices.length > 0) {
      setSelectedNodeId(devices.find((device) => device.attached)?.node_id ?? devices[0].node_id);
    }
  }, [devices, manual, selectedNodeId]);

  const selectedDevice = devices.find((device) => device.node_id === selectedNodeId);
  const rescan = async () => {
    try {
      await api.rollerCanControlRescan();
      message.success(copy.rescanOk);
    } catch (error) {
      message.error(errMsg(error));
    }
  };

  return (
    <div className="rollercan-workspace">
      <aside className="rollercan-device-list">
        <div className="rollercan-device-list__heading">
          <div>
            <Typography.Text strong>{copy.motors} ({devices.length})</Typography.Text>
            <Typography.Text type="secondary">{copy.discovery}</Typography.Text>
          </div>
          <Button size="small" disabled={!connected} onClick={rescan}>{copy.rescan}</Button>
        </div>
        {devices.length === 0 ? (
          <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={connected ? copy.waiting : undefined} />
        ) : (
          <List
            dataSource={devices}
            rowKey={(device) => device.node_id}
            renderItem={(device) => {
              const selected = !manual && device.node_id === selectedNodeId;
              return (
                <List.Item
                  className={`rollercan-device-list__item${selected ? " rollercan-device-list__item--selected" : ""}`}
                  onClick={() => {
                    setManual(false);
                    setSelectedNodeId(device.node_id);
                  }}
                >
                  <div>
                    <Space style={{ justifyContent: "space-between", width: "100%" }}>
                      <Typography.Text strong>Unit RollerCAN</Typography.Text>
                      <Typography.Text code>{formatCanId(device.node_id)}</Typography.Text>
                    </Space>
                    <Space size={4} wrap>
                      <Tag color={device.online ? "green" : "default"}>{device.online ? copy.online : copy.offline}</Tag>
                      {device.attached && <Tag color="blue">{copy.attached}</Tag>}
                      {device.enabled && <Tag color="processing">{copy.enabled}</Tag>}
                      <Tag>{modeLabel(device.mode, zh)}</Tag>
                    </Space>
                  </div>
                </List.Item>
              );
            }}
          />
        )}
        <Button
          block
          className="rollercan-device-list__manual"
          type={manual ? "primary" : "default"}
          onClick={() => {
            setManual(true);
            setSelectedNodeId(null);
            setManualKey((value) => value + 1);
          }}
        >
          {copy.manual}
        </Button>
      </aside>
      <section className="rollercan-workspace__detail">
        <RollerCanControlDetail
          key={manual ? `manual-${manualKey}` : `node-${selectedDevice?.node_id ?? "default"}`}
          connected={connected}
          initialDevice={manual ? undefined : selectedDevice}
          onDevicesChanged={refreshDevices}
        />
      </section>
    </div>
  );
}

function RollerCanControlDetail({
  connected,
  initialDevice,
  onDevicesChanged,
}: {
  connected: boolean;
  initialDevice?: RollerCanControlDevice;
  onDevicesChanged: () => Promise<void>;
}) {
  const { message } = App.useApp();
  const { lang } = useI18n();
  const zh = lang === "zh";
  const [nodeIdText, setNodeIdText] = useState(formatCanId(initialDevice?.node_id ?? 0xA8));
  const [attachedState, setAttachedState] = useState<RollerCanControlState | null>(null);
  const [mode, setMode] = useState<RollerCanControlMode>(initialDevice?.mode ?? "Speed");
  const [busy, setBusy] = useState(false);
  const [view, setView] = useState<"numeric" | "chart">("numeric");
  const [windowSec, setWindowSec] = useState(10);
  const [rateHz, setRateHz] = useState(20);
  const [speedRpm, setSpeedRpm] = useState(0);
  const [positionDeg, setPositionDeg] = useState(0);
  const [currentMa, setCurrentMa] = useState(0);
  const [encoderCount, setEncoderCount] = useState(0);
  const [currentLimitMa, setCurrentLimitMa] = useState(450);

  const attachedNodeId = attachedState?.node_id ?? (initialDevice?.attached ? initialDevice.node_id : null);
  const telemetry = useRollerCanControlTelemetry(attachedNodeId, connected, rateHz);
  const state = telemetry.latest ?? attachedState;
  const faulted = !!state && (state.fault_bits !== 0 || state.state_code === 2);

  const copy = useMemo(
    () => zh
      ? {
          connection: "电机连接",
          attach: "挂载电机",
          detach: "安全失能并卸载",
          attached: "已挂载",
          detached: "未挂载",
          attachOk: "RollerCAN 已挂载（保持设备当前使能状态）",
          detachOk: "电机已确认失能并卸载",
          nodeId: "电机 CAN ID",
          display: "信息显示",
          numeric: "数值",
          chart: "图表",
          window: "窗口",
          refresh: "刷新",
          refreshNow: "立即刷新",
          control: "电机控制",
          mode: "模式",
          enable: "使能",
          disable: "失能",
          release: "解除堵转保护",
          send: "发送目标",
          applyLimit: "应用最大电流",
          maxCurrent: "最大电流 (mA)",
          speed: "速度",
          position: "位置",
          current: "电流",
          voltage: "输入电压",
          temperature: "芯片温度",
          encoder: "编码器计数",
          status: "状态",
          faults: "故障",
          feedbackAge: "反馈时延",
          feedbackRate: "反馈频率",
          rxCount: "反馈帧数",
          online: "在线",
          offline: "离线",
          actionOk: "命令已确认",
          targetOk: "目标值已写入并回读确认",
          safety: "Unit RollerCAN 使用 CAN 2.0 扩展帧和 1 Mbps。请固定电机并使用限流电源；Current 模式上限为 ±1200 mA。挂载只读取状态，不会自动使能。",
          bitrate: "gs_usb 会以 1 Mbps 标称速率打开；SocketCAN 接口需在系统侧预先配置为 1 Mbps。",
          connectFirst: "请先在顶部连接 CAN 总线，然后挂载或扫描 RollerCAN 电机。",
          noFeedback: "设备未返回参数。请检查供电、CAN_H/CAN_L、共地、终端电阻、扩展帧支持和 1 Mbps 波特率。",
          enableFirst: "先使能电机才能发送目标；切换模式前必须先失能。",
          smartknobBoundary: "此窗口用于 RollerCAN 原厂控制固件。SmartKnob 使用独立固件和独立业务窗口，两者不会共享控制状态。",
        }
      : {
          connection: "Motor connection",
          attach: "Attach motor",
          detach: "Safe disable & detach",
          attached: "Attached",
          detached: "Detached",
          attachOk: "RollerCAN attached (the device's existing enable state was preserved)",
          detachOk: "Motor confirmed disabled and detached",
          nodeId: "Motor CAN ID",
          display: "Display",
          numeric: "Numeric",
          chart: "Chart",
          window: "Window",
          refresh: "Refresh",
          refreshNow: "Refresh now",
          control: "Motor control",
          mode: "Mode",
          enable: "Enable",
          disable: "Disable",
          release: "Release stall protection",
          send: "Send target",
          applyLimit: "Apply max current",
          maxCurrent: "Maximum current (mA)",
          speed: "Speed",
          position: "Position",
          current: "Current",
          voltage: "Input voltage",
          temperature: "SoC temperature",
          encoder: "Encoder count",
          status: "State",
          faults: "Faults",
          feedbackAge: "Feedback age",
          feedbackRate: "Feedback rate",
          rxCount: "RX count",
          online: "Online",
          offline: "Offline",
          actionOk: "Command confirmed",
          targetOk: "Target written and verified by readback",
          safety: "Unit RollerCAN uses CAN 2.0 extended frames at 1 Mbps. Secure the motor and use a current-limited supply; Current mode is limited to ±1200 mA. Attach only reads state and never enables automatically.",
          bitrate: "gs_usb opens at a 1 Mbps nominal rate. Configure SocketCAN interfaces to 1 Mbps in the operating system first.",
          connectFirst: "Connect the CAN bus in the top bar before attaching or scanning for a RollerCAN motor.",
          noFeedback: "No parameter response. Check power, CAN_H/CAN_L, common ground, termination, extended-frame support, and the 1 Mbps bitrate.",
          enableFirst: "Enable the motor before sending a target. Disable it before changing mode.",
          smartknobBoundary: "This window targets the stock RollerCAN control firmware. SmartKnob uses separate firmware and an independent application path.",
        },
    [zh],
  );

  useEffect(() => {
    if (!connected || !initialDevice?.attached) return;
    let alive = true;
    api.rollerCanControlGetState(initialDevice.node_id)
      .then((next) => {
        if (!alive) return;
        setAttachedState(next);
        setMode(next.mode);
      })
      .catch(() => {});
    return () => { alive = false; };
  }, [connected, initialDevice?.attached, initialDevice?.node_id]);

  useEffect(() => {
    if (!state) return;
    setMode(state.mode);
    const limit = state.mode === "Speed"
      ? state.speed_max_current_ma
      : state.mode === "Position"
        ? state.position_max_current_ma
        : null;
    if (limit != null) setCurrentLimitMa(limit);
  }, [state?.mode, state?.position_max_current_ma, state?.speed_max_current_ma]);

  const run = async (success: string, action: () => Promise<unknown>) => {
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
  };

  const attach = async () => {
    let nodeId: number;
    try {
      nodeId = parseCanId(nodeIdText);
    } catch (error) {
      message.error(errMsg(error));
      return;
    }
    try {
      await run(copy.attachOk, async () => {
        const next = await api.rollerCanControlAttach(nodeId);
        setAttachedState(next);
        setMode(next.mode);
        await onDevicesChanged();
      });
    } catch {
      // `run` already surfaced the protocol error.
    }
  };

  const detach = async () => {
    if (attachedNodeId == null) return;
    try {
      await run(copy.detachOk, async () => {
        await api.rollerCanControlDetach(attachedNodeId);
        setAttachedState(null);
        await onDevicesChanged();
      });
    } catch {
      // Keep the attached state visible so Disable/Detach can be retried.
    }
  };

  const action = async (success: string, operation: () => Promise<unknown>) => {
    try {
      await run(success, operation);
    } catch {
      // Error was displayed by `run`.
    }
  };

  const changeMode = async (nextMode: RollerCanControlMode) => {
    if (attachedNodeId == null) {
      setMode(nextMode);
      return;
    }
    const previous = mode;
    setMode(nextMode);
    try {
      await run(copy.actionOk, async () => {
        const next = await api.rollerCanControlSetMode(attachedNodeId, nextMode);
        setAttachedState(next);
      });
    } catch {
      setMode(previous);
    }
  };

  const target = (): RollerCanControlTarget => {
    switch (mode) {
      case "Speed": return { kind: "Speed", speed_rpm: speedRpm };
      case "Position": return { kind: "Position", position_deg: positionDeg };
      case "Current": return { kind: "Current", current_ma: currentMa };
      case "Encoder": return { kind: "Encoder", encoder_count: Math.round(encoderCount) };
    }
  };

  const statusTag = state ? (
    <Tag color={faulted ? "red" : state.enabled ? "green" : state.online ? "blue" : "default"}>
      {stateLabel(state.state_code, zh)}
    </Tag>
  ) : <Tag>{copy.detached}</Tag>;

  const connectionPanel = (
    <Space direction="vertical" size={12} style={{ width: "100%" }}>
      <Alert type="warning" showIcon message={copy.safety} description={copy.bitrate} />
      <Alert type="info" showIcon message={copy.smartknobBoundary} />
      {!connected && <Alert type="info" showIcon message={copy.connectFirst} />}
      <Field label={copy.nodeId}>
        <Input value={nodeIdText} disabled={attachedNodeId != null} onChange={(event) => setNodeIdText(event.target.value)} />
      </Field>
      <Space wrap>
        {attachedNodeId == null ? (
          <Button type="primary" loading={busy} disabled={!connected} onClick={attach}>{copy.attach}</Button>
        ) : (
          <Button danger loading={busy} onClick={detach}>{copy.detach}</Button>
        )}
        <Tag color={attachedNodeId != null ? "green" : "default"}>{attachedNodeId != null ? copy.attached : copy.detached}</Tag>
        {state && statusTag}
      </Space>
    </Space>
  );

  const numericPanel = (
    <Descriptions bordered size="small" column={2} className="rollercan-telemetry-table">
      <Descriptions.Item label={`${copy.speed} (rpm)`}><NumberValue value={state?.speed_rpm} /></Descriptions.Item>
      <Descriptions.Item label={`${copy.position} (°)`}><NumberValue value={state?.position_deg} /></Descriptions.Item>
      <Descriptions.Item label={`${copy.current} (mA)`}><NumberValue value={state?.current_ma} digits={1} /></Descriptions.Item>
      <Descriptions.Item label={`${copy.voltage} (V)`}><NumberValue value={state?.voltage_v} digits={2} /></Descriptions.Item>
      <Descriptions.Item label={`${copy.temperature} (°C)`}><NumberValue value={state?.temperature_c} digits={1} /></Descriptions.Item>
      <Descriptions.Item label={copy.encoder}>{state?.encoder_count ?? "—"}</Descriptions.Item>
      <Descriptions.Item label={copy.mode}>{modeLabel(state?.mode ?? mode, zh)}</Descriptions.Item>
      <Descriptions.Item label={copy.status}>{statusTag}</Descriptions.Item>
      <Descriptions.Item label={copy.faults}>{faultLabel(state?.fault_bits ?? 0, zh)}</Descriptions.Item>
      <Descriptions.Item label={copy.feedbackAge}>{state?.feedback_age_ms != null ? `${state.feedback_age_ms} ms` : "—"}</Descriptions.Item>
      <Descriptions.Item label={copy.feedbackRate}><NumberValue value={state?.feedback_rate_hz} digits={1} suffix=" Hz" /></Descriptions.Item>
      <Descriptions.Item label={copy.rxCount}>{state?.rx_count ?? 0}</Descriptions.Item>
    </Descriptions>
  );

  return (
    <div className="rollercan-panel">
      <Space direction="vertical" size={12} style={{ width: "100%" }}>
        <Card size="small" className="rollercan-summary-card">
          <div className="rollercan-summary-card__content">
            <Space direction="vertical" size={3}>
              <Space align="center" wrap>
                <Typography.Title level={4} style={{ margin: 0 }}>Unit RollerCAN</Typography.Title>
                <Typography.Text code>{formatCanId(state?.node_id ?? parseCanIdSafe(nodeIdText))}</Typography.Text>
                <Tag>CAN 2.0B · 29-bit · 1 Mbps</Tag>
                {statusTag}
                <Tag>{modeLabel(state?.mode ?? mode, zh)}</Tag>
              </Space>
              <Typography.Text type="secondary" className="rollercan-summary-card__meta">
                {state ? `${state.online ? copy.online : copy.offline} · RX ${state.rx_count} · ${faultLabel(state.fault_bits, zh)}` : copy.detached}
              </Typography.Text>
            </Space>
            <Space size={5}>
              <Typography.Text type="secondary">{copy.refresh}</Typography.Text>
              <Segmented
                size="small"
                value={rateHz}
                onChange={(value) => setRateHz(value as number)}
                options={[{ label: "20 Hz", value: 20 }, { label: "50 Hz", value: 50 }]}
              />
            </Space>
          </div>
        </Card>

        <Collapse
          defaultActiveKey={attachedNodeId == null ? ["connection"] : []}
          items={[{ key: "connection", label: copy.connection, children: connectionPanel }]}
        />

        <Card
          size="small"
          title={copy.display}
          extra={
            <Space>
              {view === "chart" && (
                <Space size={4}>
                  <Typography.Text type="secondary">{copy.window}</Typography.Text>
                  <InputNumber size="small" min={1} max={60} value={windowSec} onChange={(value) => setWindowSec(value ?? 10)} style={{ width: 68 }} />
                  <Typography.Text type="secondary">s</Typography.Text>
                </Space>
              )}
              <Segmented
                value={view}
                onChange={(value) => setView(value as "numeric" | "chart")}
                options={[{ label: copy.numeric, value: "numeric" }, { label: copy.chart, value: "chart" }]}
              />
            </Space>
          }
        >
          <Space direction="vertical" size={12} style={{ width: "100%" }}>
            {state && !state.online && <Alert type="warning" showIcon message={copy.noFeedback} />}
            {state?.last_error && <Alert type="error" showIcon message={state.last_error} />}
            {view === "numeric" ? numericPanel : (
              <RollerCanControlChart
                samples={telemetry.samples}
                chartVersion={telemetry.chartVersion}
                windowSec={windowSec}
                zh={zh}
              />
            )}
          </Space>
        </Card>

        <Card size="small" title={copy.control}>
          <Space wrap align="center">
            <Typography.Text type="secondary">{copy.mode}</Typography.Text>
            <Segmented
              value={mode}
              disabled={busy || !!state?.enabled}
              onChange={(value) => void changeMode(value as RollerCanControlMode)}
              options={MODE_OPTIONS.map((value) => ({ label: modeLabel(value, zh), value }))}
            />
            <Button
              type="primary"
              disabled={attachedNodeId == null || !!state?.enabled || faulted}
              loading={busy}
              onClick={() => attachedNodeId != null && action(copy.actionOk, () => api.rollerCanControlEnable(attachedNodeId))}
            >{copy.enable}</Button>
            <Button
              danger
              disabled={attachedNodeId == null}
              loading={busy}
              onClick={() => attachedNodeId != null && action(copy.actionOk, () => api.rollerCanControlDisable(attachedNodeId))}
            >{copy.disable}</Button>
            <Button
              disabled={attachedNodeId == null}
              loading={busy}
              onClick={() => attachedNodeId != null && action(copy.actionOk, () => api.rollerCanControlReleaseStall(attachedNodeId))}
            >{copy.release}</Button>
            <Button
              disabled={attachedNodeId == null}
              loading={busy}
              onClick={() => attachedNodeId != null && action(copy.actionOk, () => api.rollerCanControlRefresh(attachedNodeId))}
            >{copy.refreshNow}</Button>
          </Space>

          <Divider style={{ margin: "12px 0" }} />
          <div className="rollercan-target-editor">
            <Typography.Text strong>{modeLabel(mode, zh)}</Typography.Text>
            <Space wrap align="end">
              {mode === "Speed" && <NumberField label={`${copy.speed} (rpm)`} value={speedRpm} step={10} onChange={setSpeedRpm} />}
              {mode === "Position" && <NumberField label={`${copy.position} (°)`} value={positionDeg} step={1} onChange={setPositionDeg} />}
              {mode === "Current" && <NumberField label={`${copy.current} (mA)`} value={currentMa} min={-1200} max={1200} step={10} onChange={setCurrentMa} />}
              {mode === "Encoder" && <NumberField label={copy.encoder} value={encoderCount} step={1} onChange={setEncoderCount} />}
              <Button
                type="primary"
                disabled={attachedNodeId == null || !state?.enabled || faulted}
                loading={busy}
                onClick={() => attachedNodeId != null && action(copy.targetOk, () => api.rollerCanControlSendTarget(attachedNodeId, target()))}
              >{copy.send}</Button>
            </Space>
          </div>
          {!state?.enabled && <Alert type="info" showIcon message={copy.enableFirst} className="rollercan-control-hint" />}

          {(mode === "Speed" || mode === "Position") && (
            <>
              <Divider style={{ margin: "12px 0" }} />
              <Space wrap align="end">
                <NumberField label={copy.maxCurrent} value={currentLimitMa} min={0} max={1200} step={10} onChange={setCurrentLimitMa} />
                <Button
                  disabled={attachedNodeId == null}
                  loading={busy}
                  onClick={() => attachedNodeId != null && action(copy.actionOk, () => api.rollerCanControlSetCurrentLimit(attachedNodeId, currentLimitMa))}
                >{copy.applyLimit}</Button>
              </Space>
            </>
          )}
        </Card>
      </Space>
    </div>
  );
}

function Field({ label, children }: { label: ReactNode; children: ReactNode }) {
  return (
    <label className="rollercan-field">
      <Typography.Text type="secondary">{label}</Typography.Text>
      {children}
    </label>
  );
}

function NumberField({
  label,
  value,
  onChange,
  ...props
}: {
  label: ReactNode;
  value: number;
  onChange: (value: number) => void;
  min?: number;
  max?: number;
  step?: number;
}) {
  return (
    <Field label={label}>
      <InputNumber value={value} onChange={(next) => onChange(next ?? 0)} {...props} />
    </Field>
  );
}

function NumberValue({ value, digits = 2, suffix = "" }: { value: number | null | undefined; digits?: number; suffix?: string }) {
  if (value == null || !Number.isFinite(value)) return <span>—</span>;
  return <span className="rollercan-tabular">{value.toFixed(digits)}{suffix}</span>;
}

function parseCanId(raw: string): number {
  const text = raw.trim();
  if (!/^(?:0x[0-9a-f]+|\d+)$/i.test(text)) throw new Error(`CAN ID: ${raw}`);
  const value = Number(text);
  if (!Number.isInteger(value) || value < 0 || value > 0xFF) throw new Error("CAN ID: 0x00–0xFF");
  return value;
}

function parseCanIdSafe(raw: string): number {
  try { return parseCanId(raw); } catch { return 0xA8; }
}

function formatCanId(value: number): string {
  return `0x${value.toString(16).toUpperCase().padStart(2, "0")}`;
}

function modeLabel(mode: RollerCanControlMode, zh: boolean): string {
  const labels: Record<RollerCanControlMode, [string, string]> = {
    Speed: ["Speed", "速度"],
    Position: ["Position", "位置"],
    Current: ["Current", "电流"],
    Encoder: ["Encoder", "编码器"],
  };
  return labels[mode][zh ? 1 : 0];
}

function stateLabel(code: number, zh: boolean): string {
  const labels: Record<number, [string, string]> = {
    0: ["Standby", "待机"],
    1: ["Running", "运行"],
    2: ["Error", "故障"],
  };
  return (labels[code] ?? ["Unknown", "未知"])[zh ? 1 : 0];
}

function faultLabel(bits: number, zh: boolean): string {
  const faults: string[] = [];
  if (bits & 0b001) faults.push(zh ? "过压" : "Over-voltage");
  if (bits & 0b010) faults.push(zh ? "堵转" : "Stall");
  if (bits & 0b100) faults.push(zh ? "超范围" : "Out-of-range");
  return faults.length > 0 ? faults.join(" / ") : zh ? "无" : "None";
}
