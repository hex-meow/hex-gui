import { useCallback, useEffect, useRef, useState } from "react";
import { App as AntdApp, Button, Input, InputNumber, Select, Slider, Switch, Tag, Typography } from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import type { BaseInfo, BaseLimitAxisDto, BaseLimitsDto, ZenohBaseState } from "../types";
import { BasePoseViewer } from "./BasePoseViewer";
import { DiagnosticsCard, FaultAlert, RobotModeTag } from "./DiagnosticsPanel";
import "./ZenohPanel.css";

const POLL_MS = 10;
const MANUAL_MS = 33;

interface LimitDraft {
  linear: number | null;
  angular: number | null;
}

const EMPTY_LIMIT_DRAFT: LimitDraft = { linear: null, angular: null };

const KEY_MAP: Record<string, "fwd" | "back" | "left" | "right" | "ccw" | "cw"> = {
  w: "fwd",
  s: "back",
  a: "left",
  d: "right",
  q: "ccw",
  e: "cw",
};

/** embedded:由机器人控制台托管(同 ArmPanel):自动连接、锁定选中、隐藏连接 UI。 */
export function ZenohPanel({ embedded }: { embedded?: { endpoint: string; prefix: string; model: string } } = {}) {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();

  const [endpoint, setEndpoint] = useState("");
  const [connected, setConnected] = useState(false);
  const [bases, setBases] = useState<BaseInfo[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [st, setSt] = useState<ZenohBaseState | null>(null);
  const [lin, setLin] = useState(0.2);
  const [ang, setAng] = useState(0.5);
  const [busy, setBusy] = useState(false);
  const [armed, setArmed] = useState(false);
  const [keyboardEnabled, setKeyboardEnabled] = useState(true);
  const [limits, setLimits] = useState<BaseLimitsDto | null>(null);
  const [limitDraft, setLimitDraft] = useState<LimitDraft>(EMPTY_LIMIT_DRAFT);
  const [limitsLoading, setLimitsLoading] = useState(false);
  const [limitsWriting, setLimitsWriting] = useState(false);
  const keysDown = useRef<Set<string>>(new Set());
  const manualWasActive = useRef(false);
  const limitsReadRequest = useRef(0);
  const limitsWriteInFlight = useRef(false);
  const selectedRef = useRef<string | null>(selected);
  selectedRef.current = selected;
  const limitsBusy = limitsLoading || limitsWriting;

  const acceptLimits = useCallback((next: BaseLimitsDto | null) => {
    setLimits(next);
    setLimitDraft({
      linear: next?.linear?.current ?? null,
      angular: next?.angular?.current ?? null,
    });
  }, []);

  const loadLimits = useCallback(async (prefix: string) => {
    const request = ++limitsReadRequest.current;
    setLimitsLoading(true);
    try {
      const next = await api.zenohGetLimits(prefix);
      if (limitsReadRequest.current === request) acceptLimits(next);
    } catch (error) {
      if (limitsReadRequest.current === request) {
        acceptLimits(null);
        message.error(errMsg(error));
      }
    } finally {
      if (limitsReadRequest.current === request) setLimitsLoading(false);
    }
  }, [acceptLimits, message]);

  useEffect(() => {
    if (!connected) {
      setSt(null);
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const s = await api.zenohGetState();
        if (alive) setSt(s);
      } catch {
        /* transient */
      }
    };
    tick();
    const h = window.setInterval(tick, POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(h);
    };
  }, [connected]);

  useEffect(() => {
    if (!embedded) return;
    let alive = true;
    (async () => {
      try { await api.zenohConnect(embedded.endpoint); } catch { /* 已连接 = 复用 */ }
      if (!alive) return;
      setConnected(true);
      try {
        let list = await api.zenohDiscover();
        if (!list.length) { await new Promise((r) => setTimeout(r, 900)); list = await api.zenohDiscover(); }
        if (alive) setBases(list);
      } catch { /* transient */ }
      if (alive) setSelected(embedded.prefix);
    })();
    return () => { alive = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [embedded?.endpoint, embedded?.prefix]);
  // 托管态卸载不释放(会话跨切换保持,同 ArmPanel);统一收口在 RobotConsole。
  useEffect(() => () => {
    if (!embedded) { api.zenohDisconnect().catch(() => {}); }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 选中(手动或自动)某底盘即诊断聚焦:订阅其 events/logs + 播种历史(与取控解耦,只读也生效)。
  useEffect(() => {
    if (connected && selected) api.zenohSetDiagFocus(selected).catch(() => {});
  }, [connected, selected]);

  // New controllers describe their own supported acceleration axes/ranges. Older controllers
  // simply return null here, so driving remains backward compatible and the card stays hidden.
  useEffect(() => {
    if (!connected || !selected) {
      limitsReadRequest.current += 1;
      acceptLimits(null);
      setLimitsLoading(false);
      return;
    }
    void loadLimits(selected);
    return () => {
      limitsReadRequest.current += 1;
    };
  }, [acceptLimits, connected, loadLimits, selected]);

  // FATAL 时控制器已把电机失能(clear_fault 也置 enabled=false),而它不在 status 里回传 enabled。
  // 故障灯亮即把 Active 开关复位到 off,避免清障后开关仍"假 on" → 拖动无效却看不出原因。
  useEffect(() => {
    if (st?.fatal) setArmed(false);
  }, [st?.fatal]);

  const connect = useCallback(async () => {
    setBusy(true);
    try {
      await api.zenohConnect(endpoint.trim());
      setConnected(true);
      message.success(t("zConnected"));
      let list = await api.zenohDiscover();
      if (list.length === 0) {
        await new Promise((r) => setTimeout(r, 900));
        list = await api.zenohDiscover();
      }
      setBases(list);
      setSelected(list[0]?.prefix ?? null);
      if (list.length === 0) message.warning(t("zNoBase"));
    } catch (e) {
      message.error(errMsg(e));
    } finally {
      setBusy(false);
    }
  }, [endpoint, message, t]);

  const disconnect = useCallback(async () => {
    try {
      await api.zenohDisconnect();
    } catch {
      /* ignore */
    }
    setConnected(false);
    setBases([]);
    setSelected(null);
    setSt(null);
    setArmed(false);
  }, []);

  const discover = useCallback(async () => {
    try {
      const list = await api.zenohDiscover();
      setBases(list);
      if (!selected) setSelected(list[0]?.prefix ?? null);
      if (list.length === 0) message.warning(t("zNoBase"));
    } catch (e) {
      message.error(errMsg(e));
    }
  }, [selected, message, t]);

  const acquire = useCallback(async () => {
    const b = bases.find((x) => x.prefix === selected)
      ?? (embedded ? { prefix: embedded.prefix, model: embedded.model } : null);
    if (!b) return;
    try {
      await api.zenohAcquire(b.prefix, b.model);
      setArmed(false);
      message.success(t("zControlling"));
    } catch (e) {
      message.error(errMsg(e));
    }
  }, [bases, selected, message, t, embedded]);

  const release = useCallback(async () => {
    try {
      await api.zenohRelease();
      setArmed(false);
    } catch (e) {
      message.error(errMsg(e));
    }
  }, [message]);

  const setActive = useCallback(async (on: boolean) => {
    try {
      await api.zenohSetActive(on);
      setArmed(on);
    } catch (e) {
      setArmed((prev) => !on && prev);
      message.error(errMsg(e));
    }
  }, [message]);

  const writeLimits = useCallback(async (next: LimitDraft) => {
    if (
      !selected ||
      limitsWriteInFlight.current ||
      (next.linear == null && next.angular == null)
    ) {
      return;
    }
    const target = selected;
    limitsWriteInFlight.current = true;
    setLimitsWriting(true);
    try {
      const applied = await api.zenohSetLimits(target, next.linear, next.angular);
      if (selectedRef.current === target) acceptLimits(applied);
      message.success(`${t("zAccelApplied")} (${target})`);
    } catch (error) {
      message.error(`${target}: ${errMsg(error)}`);
      // A rejection includes the controller's unchanged value, but Tauri
      // errors cannot carry that typed payload. Re-read it so stale drafts do
      // not linger after holder/range races.
      if (selectedRef.current === target) void loadLimits(target);
    } finally {
      limitsWriteInFlight.current = false;
      setLimitsWriting(false);
    }
  }, [acceptLimits, loadLimits, message, selected, t]);

  const applyLimits = useCallback(() => {
    if (!limits) return;
    void writeLimits({
      linear: axisChanged(limits.linear, limitDraft.linear) ? limitDraft.linear : null,
      angular: axisChanged(limits.angular, limitDraft.angular) ? limitDraft.angular : null,
    });
  }, [limitDraft, limits, writeLimits]);

  const restoreLimits = useCallback(() => {
    if (!limits) return;
    void writeLimits({
      linear: limits.linear?.default_value ?? null,
      angular: limits.angular?.default_value ?? null,
    });
  }, [limits, writeLimits]);

  const cmd = useCallback((vx: number, vy: number, wz: number) => {
    api.zenohSetCmd(vx, vy, wz).catch(() => {});
  }, []);
  const stop = useCallback(() => cmd(0, 0, 0), [cmd]);

  const hold = (vx: number, vy: number, wz: number) => ({
    onPointerDown: () => cmd(vx, vy, wz),
    onPointerUp: stop,
    onPointerCancel: stop,
    onPointerLeave: stop,
  });

  const controlling = !!st?.controlling;
  // 观察对象的身份取自发现列表 + 选中项(权威):只读观察别台时,不复用取控作用域的 st.model/st.prefix。
  const selInfo = bases.find((b) => b.prefix === selected);
  const driveReady = controlling && armed;
  const controlsSelectedBase = controlling && st?.prefix === selected;
  const canWriteLimits =
    !!limits && !!st && (controlsSelectedBase || (!controlling && st.holder === 0));
  const limitsChanged =
    axisChanged(limits?.linear ?? null, limitDraft.linear) ||
    axisChanged(limits?.angular ?? null, limitDraft.angular);
  const limitsAtDefaults =
    axisAtDefault(limits?.linear ?? null) && axisAtDefault(limits?.angular ?? null);
  const statusTag = controlling
    ? <Tag color="green">{t("zControlling")}</Tag>
    : st && st.holder !== 0
      ? <Tag color="orange">{t("zBusy")} (#{st.holder})</Tag>
      : <Tag>{t("zNotControlling")}</Tag>;

  useEffect(() => {
    if (!driveReady || !keyboardEnabled) {
      keysDown.current.clear();
      return;
    }
    const isTyping = () => {
      const tag = (document.activeElement?.tagName ?? "").toLowerCase();
      return tag === "input" || tag === "textarea";
    };
    const onDown = (e: KeyboardEvent) => {
      const key = e.key.toLowerCase();
      if (!(key in KEY_MAP) || isTyping()) return;
      e.preventDefault();
      keysDown.current.add(key);
    };
    const onUp = (e: KeyboardEvent) => {
      keysDown.current.delete(e.key.toLowerCase());
    };
    const onBlur = () => {
      keysDown.current.clear();
    };
    window.addEventListener("keydown", onDown);
    window.addEventListener("keyup", onUp);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("keydown", onDown);
      window.removeEventListener("keyup", onUp);
      window.removeEventListener("blur", onBlur);
      keysDown.current.clear();
    };
  }, [driveReady, keyboardEnabled]);

  useEffect(() => {
    if (!driveReady) {
      if (manualWasActive.current) {
        stop();
        manualWasActive.current = false;
      }
      return;
    }
    const interval = window.setInterval(() => {
      const drive = keyboardEnabled ? readKeyboard(keysDown.current, lin, ang) : null;
      if (drive) {
        cmd(drive.vx, drive.vy, drive.wz);
        manualWasActive.current = true;
      } else if (manualWasActive.current) {
        stop();
        manualWasActive.current = false;
      }
    }, MANUAL_MS);
    return () => window.clearInterval(interval);
  }, [driveReady, keyboardEnabled, lin, ang, cmd, stop]);

  return (
    <div className="zenoh-panel">
      <section className="zenoh-connect-panel">
        {!embedded && (<label className="zenoh-field zenoh-field--endpoint">
          <span>{t("zEndpoint")}</span>
          <Input
            value={endpoint}
            disabled={connected}
            placeholder={t("zEndpointHint")}
            onChange={(e) => setEndpoint(e.target.value)}
          />
        </label>)}
        <div className="zenoh-connect-panel__actions">
          {!embedded && (connected ? (
            <Button disabled={limitsBusy} onClick={disconnect}>{t("zDisconnect")}</Button>
          ) : (
            <Button type="primary" loading={busy} onClick={connect}>{t("zConnect")}</Button>
          ))}
          <Button disabled={!connected || limitsBusy} onClick={discover}>{t("zDiscover")}</Button>
        </div>
        <div className="zenoh-discovery">
          <Typography.Text type="secondary">{t("zFound")}: {bases.length}</Typography.Text>
          <Select
            className="zenoh-discovery__select"
            value={selected ?? undefined}
            onChange={setSelected}
            placeholder={t("zNoBase")}
            disabled={!connected || bases.length === 0 || controlling || limitsBusy}
            options={bases.map((b) => ({ value: b.prefix, label: `${b.model} - ${b.prefix}` }))}
          />
          {controlling ? (
            <Button danger disabled={limitsBusy} onClick={release}>{t("zRelease")}</Button>
          ) : (
            <Button type="primary" disabled={!selected || limitsBusy} onClick={acquire}>{t("zAcquire")}</Button>
          )}
          <span className="zenoh-dock-status">
            {connected ? <Tag color="blue">{t("zConnected")}</Tag> : <Tag>{t("zDisconnected")}</Tag>}
            {statusTag}
            <RobotModeTag mode={st?.robot_mode} overtaken={st?.overtaken_reason} />
          </span>
        </div>
      </section>

      {connected && <FaultAlert fatal={!!st?.fatal} controlling={controlling} onClear={api.zenohClearFault} />}

      <div className="zenoh-dashboard">
        <section className="zenoh-card zenoh-drive">
          <div className="zenoh-card__heading">
            <div>
              <h2>{t("zDriveTitle")}</h2>
              <Typography.Text type="secondary">{t("zDriveHint")}</Typography.Text>
            </div>
            <div className="zenoh-active-toggle">
              <Typography.Text type="secondary">{t("zActiveShort")}</Typography.Text>
              <Switch checked={driveReady} disabled={!controlling} onChange={setActive} />
            </div>
          </div>

          <div className="zenoh-pad" aria-label={t("zMove")}>
            <span />
            <Button disabled={!driveReady} {...hold(lin, 0, 0)}>▲</Button>
            <span />
            <Button disabled={!driveReady} {...hold(0, lin, 0)}>◀</Button>
            <Button danger disabled={!controlling} onClick={stop}>{t("zStop")}</Button>
            <Button disabled={!driveReady} {...hold(0, -lin, 0)}>▶</Button>
            <Button disabled={!driveReady} {...hold(0, 0, ang)}>↺</Button>
            <Button disabled={!driveReady} {...hold(-lin, 0, 0)}>▼</Button>
            <Button disabled={!driveReady} {...hold(0, 0, -ang)}>↻</Button>
          </div>

          <div className="zenoh-speed-grid">
            <label className="zenoh-field">
              <span>{t("zSpeedLin")}</span>
              <InputNumber min={0} max={3} step={0.1} value={lin} onChange={(v) => setLin(v ?? 0)} />
            </label>
            <label className="zenoh-field">
              <span>{t("zSpeedAng")}</span>
              <InputNumber min={0} max={3} step={0.1} value={ang} onChange={(v) => setAng(v ?? 0)} />
            </label>
          </div>

          <div className="zenoh-keyboard-control">
            <div className="zenoh-keyboard-control__top">
              <Typography.Text type="secondary">{t("zKeyboard")}</Typography.Text>
              <Switch size="small" checked={keyboardEnabled} onChange={setKeyboardEnabled} />
            </div>
            <div className="zenoh-key-hints" aria-label={t("zKeyboard")}>
              <kbd>W</kbd>
              <kbd>A</kbd>
              <kbd>S</kbd>
              <kbd>D</kbd>
              <kbd>Q</kbd>
              <kbd>E</kbd>
            </div>
            <Typography.Text type="secondary" className="zenoh-keyboard-control__hint">
              {t("zKeyHint")}
            </Typography.Text>
          </div>
        </section>

        <section className="zenoh-odom">
          <div className="zenoh-card__heading zenoh-odom__heading">
            <div>
              <h2>{t("zPose3d")}</h2>
              <Typography.Text type="secondary">{t("zPose")}</Typography.Text>
            </div>
            <Tag color={connected ? "green" : "default"}>{connected ? t("zLiveOdom") : t("zNoOdom")}</Tag>
          </div>
          <BasePoseViewer
            connected={connected}
            poseX={st?.pose_x ?? 0}
            poseY={st?.pose_y ?? 0}
            theta={st?.pose_theta ?? 0}
            vx={st?.vx ?? 0}
            vy={st?.vy ?? 0}
            wz={st?.wz ?? 0}
          />
        </section>

        <section className="zenoh-card zenoh-telemetry">
          <div className="zenoh-card__heading">
            <div>
              <h2>{t("zTelemetry")}</h2>
              <Typography.Text type="secondary">{selInfo?.model || st?.model || t("toolBaseZenoh")}</Typography.Text>
            </div>
          </div>
          <MetricGroup
            title={t("zPose")}
            items={[
              ["x", `${fmt3(st?.pose_x ?? 0)} m`],
              ["y", `${fmt3(st?.pose_y ?? 0)} m`],
              ["theta", `${fmt3(st?.pose_theta ?? 0)} rad`],
            ]}
          />
          <MetricGroup
            title={t("zTwist")}
            items={[
              ["vx", `${fmt3(st?.vx ?? 0)} m/s`],
              ["vy", `${fmt3(st?.vy ?? 0)} m/s`],
              ["wz", `${fmt3(st?.wz ?? 0)} rad/s`],
            ]}
          />
          <div className="zenoh-prefix" title={st?.prefix || selected || ""}>
            {st?.prefix || selected || t("zNoBase")}
          </div>
        </section>
      </div>

      {connected && selected && limits && (limits.linear || limits.angular) && (
        <section className="zenoh-card zenoh-limits">
          <div className="zenoh-card__heading">
            <div>
              <h2>{t("zAccelTitle")}</h2>
              <Typography.Text type="secondary">{t("zAccelHint")}</Typography.Text>
            </div>
            <div className="zenoh-limits__actions">
              <Button
                type="primary"
                loading={limitsBusy}
                disabled={!canWriteLimits || !limitsChanged}
                onClick={applyLimits}
              >
                {t("zAccelApply")}
              </Button>
              <Button
                disabled={!canWriteLimits || limitsAtDefaults || limitsBusy}
                onClick={restoreLimits}
              >
                {t("zAccelRestore")}
              </Button>
              <Button
                disabled={limitsBusy}
                onClick={() => void loadLimits(selected)}
              >
                {t("zAccelRefresh")}
              </Button>
            </div>
          </div>

          <div className="zenoh-limits__grid">
            {limits.linear && (
              <LimitAxisControl
                label={t("zAccelLinear")}
                unit="m/s²"
                axis={limits.linear}
                value={limitDraft.linear ?? limits.linear.current}
                disabled={!canWriteLimits || limitsBusy}
                currentLabel={t("zAccelCurrent")}
                defaultLabel={t("zAccelDefault")}
                onChange={(linear) => setLimitDraft((draft) => ({ ...draft, linear }))}
              />
            )}
            {limits.angular && (
              <LimitAxisControl
                label={t("zAccelAngular")}
                unit="rad/s²"
                axis={limits.angular}
                value={limitDraft.angular ?? limits.angular.current}
                disabled={!canWriteLimits || limitsBusy}
                currentLabel={t("zAccelCurrent")}
                defaultLabel={t("zAccelDefault")}
                onChange={(angular) => setLimitDraft((draft) => ({ ...draft, angular }))}
              />
            )}
          </div>

          {!canWriteLimits && st && st.holder !== 0 && !controlling && (
            <Typography.Text type="warning" className="zenoh-limits__notice">
              {t("zAccelBusyHint")}
            </Typography.Text>
          )}
          <Typography.Text type="secondary" className="zenoh-limits__notice">
            {t("zAccelSemantics")}
          </Typography.Text>
        </section>
      )}

      {connected && (
        <DiagnosticsCard
          enabled={!!selected}
          getEvents={api.zenohGetEvents}
          getLogs={api.zenohGetLogs}
          onRefresh={api.zenohRefreshDiag}
        />
      )}
    </div>
  );
}

function LimitAxisControl({
  label,
  unit,
  axis,
  value,
  disabled,
  currentLabel,
  defaultLabel,
  onChange,
}: {
  label: string;
  unit: string;
  axis: BaseLimitAxisDto;
  value: number;
  disabled: boolean;
  currentLabel: string;
  defaultLabel: string;
  onChange: (value: number) => void;
}) {
  const step = limitStep(axis);
  return (
    <div className="zenoh-limit-axis">
      <div className="zenoh-limit-axis__heading">
        <Typography.Text strong>{label}</Typography.Text>
        <Typography.Text type="secondary">
          {currentLabel} {formatLimit(axis.current)} · {defaultLabel} {formatLimit(axis.default_value)} {unit}
        </Typography.Text>
      </div>
      <div className="zenoh-limit-axis__control">
        <Slider
          min={axis.min}
          max={axis.max}
          step={step}
          value={value}
          disabled={disabled}
          tooltip={{ formatter: (next) => `${formatLimit(next ?? value)} ${unit}` }}
          onChange={onChange}
        />
        <InputNumber
          min={axis.min}
          max={axis.max}
          step={step}
          value={value}
          disabled={disabled}
          addonAfter={unit}
          onChange={(next) => {
            if (next != null) onChange(Math.min(axis.max, Math.max(axis.min, next)));
          }}
        />
      </div>
      <Typography.Text type="secondary" className="zenoh-limit-axis__range">
        {formatLimit(axis.min)} – {formatLimit(axis.max)} {unit}
      </Typography.Text>
    </div>
  );
}

function limitStep(axis: BaseLimitAxisDto): number {
  const span = axis.max - axis.min;
  if (!Number.isFinite(span) || span <= 0) return 0.01;
  return 10 ** Math.floor(Math.log10(span / 100));
}

function formatLimit(value: number): string {
  if (!Number.isFinite(value)) return "—";
  return Number(value.toFixed(3)).toString();
}

function axisChanged(axis: BaseLimitAxisDto | null, draft: number | null): boolean {
  return axis != null && draft != null && Math.abs(axis.current - draft) > 1e-6;
}

function axisAtDefault(axis: BaseLimitAxisDto | null): boolean {
  return axis == null || Math.abs(axis.current - axis.default_value) <= 1e-6;
}

function MetricGroup({ title, items }: { title: string; items: Array<[string, string]> }) {
  return (
    <div className="zenoh-metric-group">
      <Typography.Text strong>{title}</Typography.Text>
      <div className="zenoh-metric-grid">
        {items.map(([label, value]) => (
          <div className="zenoh-metric" key={label}>
            <span>{label}</span>
            <strong>{value}</strong>
          </div>
        ))}
      </div>
    </div>
  );
}

function fmt3(v: number): string {
  if (!Number.isFinite(v)) return "0.000";
  return (v < 0 ? "-" : " ") + Math.abs(v).toFixed(3);
}

interface Twist {
  vx: number;
  vy: number;
  wz: number;
}

function readKeyboard(keys: Set<string>, linear: number, angular: number): Twist | null {
  let fx = 0;
  let fy = 0;
  let fz = 0;
  for (const key of keys) {
    switch (KEY_MAP[key]) {
      case "fwd": fx += 1; break;
      case "back": fx -= 1; break;
      case "left": fy += 1; break;
      case "right": fy -= 1; break;
      case "ccw": fz += 1; break;
      case "cw": fz -= 1; break;
    }
  }
  if (fx === 0 && fy === 0 && fz === 0) return null;
  const len = Math.hypot(fx, fy);
  const scale = len > 0 ? linear / len : 0;
  return { vx: fx * scale, vy: fy * scale, wz: fz * angular };
}
