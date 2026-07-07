import { useCallback, useEffect, useRef, useState } from "react";
import {
  App as AntdApp,
  Button,
  Card,
  Col,
  Empty,
  Input,
  InputNumber,
  Row,
  Select,
  Space,
  Statistic,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import type { KnobConfig, SmartKnobState } from "../types";
import "./RollerCanPanel.css";

const POLL_MS = 40;
const CMD_SET_CONFIG = 0x8001;
const TUNING = {
  pGain: 0x8101,
  dGain: 0x8102,
  strength: 0x8103,
  torqueLimit: 0x8104,
  maxTorque: 0x8105,
  friction: 0x8106,
  click: 0x8107,
};
const CUSTOM = {
  position: 0x8201,
  minPosition: 0x8202,
  maxPosition: 0x8203,
  widthDeg: 0x8204,
  detentStrength: 0x8205,
  endstopStrength: 0x8206,
  snapPoint: 0x8207,
  snapBias: 0x8208,
  click: 0x8209,
  friction: 0x820a,
  strength: 0x820b,
  pGain: 0x820c,
  dGain: 0x820d,
  ledHue: 0x820e,
};

interface RollerCanKnobState extends SmartKnobState {
  connected?: boolean;
}

interface PerModeTuning {
  pGain: number;
  dGain: number;
  strength: number;
  torqueLimit: number;
  maxTorque: number;
  frictionComp: number;
  clickTorque: number;
}

function defaultBus() {
  return /linux/i.test(navigator.userAgent) ? "can0" : "gs_usb0";
}

export function RollerCanPanel() {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();

  const [bus, setBus] = useState(defaultBus());
  const [motorId, setMotorId] = useState(0xA8);
  const [connected, setConnected] = useState(false);
  const [busy, setBusy] = useState(false);
  const [configs, setConfigs] = useState<KnobConfig[]>([]);
  const [modeIndex, setModeIndex] = useState(0);
  const [running, setRunning] = useState(false);
  const [starting, setStarting] = useState(false);
  const [state, setState] = useState<RollerCanKnobState | null>(null);

  const [strength, setStrength] = useState(0.04);
  const [torqueLimit, setTorqueLimit] = useState(0.45);
  const [maxTorque, setMaxTorque] = useState(700);
  const [frictionComp, setFrictionComp] = useState(0.0);
  const [clickTorque, setClickTorque] = useState(0.0);
  const [pGain, setPGain] = useState(0.0);
  const [dGain, setDGain] = useState(0.0);
  const [customConfig, setCustomConfig] = useState<KnobConfig | null>(null);

  const perModeTuning = useRef<Map<number, PerModeTuning>>(new Map());
  const lastEditRef = useRef(0);
  const pickSeqRef = useRef<Promise<void>>(Promise.resolve());
  const EDIT_GRACE_MS = 1200;

  useEffect(() => {
    api.rollercanConfigs().then((cfgs) => {
      setConfigs(cfgs);
      if (cfgs.length > 0) {
        setCustomConfig(cfgs[0]);
        setStrength(cfgs[0].strength_scale);
        setFrictionComp(cfgs[0].friction_compensation);
        setClickTorque(cfgs[0].click_torque_nm);
        setPGain(cfgs[0].p_gain);
        setDGain(cfgs[0].d_gain);
      }
    }).catch(() => {});
  }, []);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const s = await rollercanGetState();
        if (!alive) return;
        setConnected(Boolean(s.connected));
        setRunning(Boolean(s.running));
        setState(s.running ? s : null);
        if (s.running && performance.now() - lastEditRef.current > EDIT_GRACE_MS) {
          setPGain(s.p_gain);
          setDGain(s.d_gain);
          setTorqueLimit(s.torque_limit_nm);
          setMaxTorque(s.max_torque_permille);
          setFrictionComp(s.friction_compensation);
          setClickTorque(s.click_torque_nm);
        }
      } catch {
        if (alive) {
          setConnected(false);
          setRunning(false);
          setState(null);
        }
      }
    };
    tick();
    const h = window.setInterval(tick, POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(h);
    };
  }, []);

  const connect = useCallback(async () => {
    setBusy(true);
    try {
      await api.rollercanConnect(bus);
      setConnected(true);
      message.success("RollerCAN SmartKnob connected");
    } catch (e) {
      message.error(`Connect failed: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  }, [bus, message]);

  const disconnect = useCallback(async () => {
    setBusy(true);
    try {
      await api.rollercanDisconnect();
      setConnected(false);
      setRunning(false);
      setState(null);
      message.success("RollerCAN SmartKnob disconnected");
    } catch (e) {
      message.error(`Disconnect failed: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  }, [message]);

  const pushTuning = useCallback(
    async (s: number, tl: number, mt: number, fc: number, ct: number, pg: number, dg: number) => {
      await Promise.all([
        writeScaled(TUNING.pGain, pg),
        writeScaled(TUNING.dGain, dg),
        writeScaled(TUNING.strength, s),
        writeScaled(TUNING.torqueLimit, tl),
        api.rollercanWriteParam(0, motorId, TUNING.maxTorque, Math.trunc(mt)),
        writeScaled(TUNING.friction, fc),
        writeScaled(TUNING.click, ct),
      ]);
    },
    [motorId],
  );

  const start = useCallback(async () => {
    setStarting(true);
    let backendStarted = false;
    try {
      const saved = perModeTuning.current.get(modeIndex);
      const cfg = configs[modeIndex];
      const startPGain = saved?.pGain ?? cfg?.p_gain ?? pGain;
      const startDGain = saved?.dGain ?? cfg?.d_gain ?? dGain;
      const startStrength = saved?.strength ?? cfg?.strength_scale ?? strength;
      const startFriction = saved?.frictionComp ?? cfg?.friction_compensation ?? frictionComp;
      const startClick = saved?.clickTorque ?? cfg?.click_torque_nm ?? clickTorque;
      const startTorqueLimit = saved?.torqueLimit ?? torqueLimit;
      const startMaxTorque = saved?.maxTorque ?? maxTorque;
      if (modeIndex === 0 && customConfig) await pushCustomConfig(customConfig, motorId);
      await pushTuning(startStrength, startTorqueLimit, startMaxTorque, startFriction, startClick, startPGain, startDGain);
      await api.rollercanEnable(modeIndex, motorId);
      backendStarted = true;
      setRunning(true);
      message.success("RollerCAN SmartKnob running");
    } catch (e) {
      if (backendStarted) await api.rollercanStopMotor(0, motorId).catch(() => {});
      setRunning(false);
      setState(null);
      message.error(`Start failed: ${errMsg(e)}`);
    } finally {
      setStarting(false);
    }
  }, [modeIndex, configs, pGain, dGain, strength, torqueLimit, maxTorque, frictionComp, clickTorque, motorId, customConfig, pushTuning, message]);

  const stop = useCallback(async () => {
    try {
      await api.rollercanStopMotor(0, motorId);
    } catch (e) {
      message.error(errMsg(e));
    }
    setRunning(false);
    setState(null);
  }, [motorId, message]);

  const clearError = useCallback(async () => {
    try {
      await api.rollercanReleaseStall(0, motorId);
      message.success(t("skCleared"));
    } catch (e) {
      message.error(errMsg(e));
    }
  }, [motorId, message, t]);

  const pickMode = useCallback((idx: number) => {
    setModeIndex(idx);
    lastEditRef.current = 0;
    const saved = perModeTuning.current.get(idx);
    let merged: KnobConfig | null = null;
    if (saved) {
      setStrength(saved.strength);
      setTorqueLimit(saved.torqueLimit);
      setMaxTorque(saved.maxTorque);
      setFrictionComp(saved.frictionComp);
      setClickTorque(saved.clickTorque);
      setPGain(saved.pGain);
      setDGain(saved.dGain);
      if (idx === 0 && customConfig) {
        merged = { ...customConfig, strength_scale: saved.strength, friction_compensation: saved.frictionComp, click_torque_nm: saved.clickTorque, p_gain: saved.pGain, d_gain: saved.dGain };
        setCustomConfig(merged);
      }
    } else {
      setStrength(configs[idx]?.strength_scale ?? 0.04);
      setFrictionComp(configs[idx]?.friction_compensation ?? 0);
      setClickTorque(configs[idx]?.click_torque_nm ?? 0);
      setPGain(configs[idx]?.p_gain ?? 0);
      setDGain(configs[idx]?.d_gain ?? 0);
      if (idx === 0 && customConfig) merged = customConfig;
    }
    if (running) {
      const savedTuning = saved;
      const cfgToPush = merged;
      pickSeqRef.current = pickSeqRef.current.then(async () => {
        await api.rollercanWriteParam(0, motorId, CMD_SET_CONFIG, idx);
        if (idx === 0 && cfgToPush) await pushCustomConfig(cfgToPush, motorId);
        if (savedTuning) {
          await pushTuning(savedTuning.strength, savedTuning.torqueLimit, savedTuning.maxTorque, savedTuning.frictionComp, savedTuning.clickTorque, savedTuning.pGain, savedTuning.dGain);
        }
      }).catch(() => {});
    }
  }, [running, configs, customConfig, motorId, pushTuning]);

  const applyTuning = useCallback((s: number, tl: number, mt: number, fc: number, ct: number, pg: number, dg: number) => {
    lastEditRef.current = performance.now();
    setStrength(s);
    setTorqueLimit(tl);
    setMaxTorque(mt);
    setFrictionComp(fc);
    setClickTorque(ct);
    setPGain(pg);
    setDGain(dg);
    if (modeIndex === 0) {
      setCustomConfig((prev) => prev ? { ...prev, strength_scale: s, friction_compensation: fc, click_torque_nm: ct, p_gain: pg, d_gain: dg } : prev);
    }
    perModeTuning.current.set(modeIndex, { strength: s, torqueLimit: tl, maxTorque: mt, frictionComp: fc, clickTorque: ct, pGain: pg, dGain: dg });
    if (running) pushTuning(s, tl, mt, fc, ct, pg, dg).catch(() => {});
  }, [modeIndex, running, pushTuning]);

  const applyCustomConfig = useCallback((updates: Partial<KnobConfig>) => {
    lastEditRef.current = performance.now();
    setCustomConfig((prev) => {
      if (!prev) return prev;
      const next: KnobConfig = { ...prev, strength_scale: strength, friction_compensation: frictionComp, click_torque_nm: clickTorque, p_gain: pGain, d_gain: dGain, ...updates };
      if (modeIndex === 0 && updates.detent_strength_unit !== undefined) {
        next.p_gain = recommendedPGain(next);
        next.d_gain = recommendedDGain(next);
        setPGain(next.p_gain);
        setDGain(next.d_gain);
        perModeTuning.current.set(modeIndex, { strength, torqueLimit, maxTorque, frictionComp, clickTorque, pGain: next.p_gain, dGain: next.d_gain });
        if (running) pushTuning(strength, torqueLimit, maxTorque, frictionComp, clickTorque, next.p_gain, next.d_gain).catch(() => {});
      }
      if (running && modeIndex === 0) pushCustomConfig(next, motorId).catch(() => {});
      return next;
    });
  }, [modeIndex, running, motorId, strength, torqueLimit, maxTorque, frictionComp, clickTorque, pGain, dGain, pushTuning]);

  const applyRecommendedGains = useCallback(() => {
    const cfg = modeIndex === 0 ? customConfig : configs[modeIndex];
    if (!cfg) return;
    applyTuning(strength, torqueLimit, maxTorque, frictionComp, clickTorque, recommendedPGain(cfg), recommendedDGain(cfg));
  }, [modeIndex, customConfig, configs, applyTuning, strength, torqueLimit, maxTorque, frictionComp, clickTorque]);

  const activeIndex = running ? state?.config_index ?? modeIndex : modeIndex;
  const activeConfig = activeIndex === 0 && customConfig ? customConfig : state?.config ?? configs[activeIndex] ?? null;

  return (
    <div className="rollercan-workspace">
      <section className="rollercan-toolbar">
        <Input addonBefore="Bus" value={bus} disabled={connected} onChange={(e) => setBus(e.target.value)} />
        <InputNumber addonBefore="Motor" min={0} max={255} value={motorId} disabled={running} onChange={(v) => setMotorId(v ?? 0xA8)} />
        <Button type={connected ? "default" : "primary"} loading={busy} onClick={connected ? disconnect : connect}>
          {connected ? "Disconnect" : "Connect"}
        </Button>
      </section>

      {!connected ? (
        <div className="rollercan-empty">
          <Empty description="Connect a RollerCAN SmartKnob bus first" />
        </div>
      ) : (
        <Space direction="vertical" size={16} style={{ width: "100%" }}>
          <Card>
            <Space wrap>
              {!running ? (
                <>
                  <Typography.Text type="secondary">{t("skMotor")}:</Typography.Text>
                  <Tag>0x{motorId.toString(16).toUpperCase().padStart(2, "0")}</Tag>
                  <Button type="primary" loading={starting} onClick={start}>
                    {starting ? t("skStarting") : t("skStart")}
                  </Button>
                </>
              ) : (
                <>
                  <Button danger onClick={stop}>{t("skStop")}</Button>
                  <Button onClick={clearError}>{t("skClearError")}</Button>
                </>
              )}
              <Tag color={running ? "green" : "default"}>{running ? t("skRunning") : t("skStopped")}</Tag>
              {state?.error && <Tag color="red">{state.error}</Tag>}
            </Space>
          </Card>

          <Row gutter={16}>
            <Col xs={24} lg={11}>
              <Card><Dial config={activeConfig} state={state} /></Card>

              <Card title={t("skModeConfig")} size="small" style={{ marginTop: 16 }}>
                {activeIndex !== 0 && (
                  <Typography.Text type="secondary" style={{ fontSize: 12, display: "block", marginBottom: 8 }}>
                    {t("skCustomLocked")}
                  </Typography.Text>
                )}
                <Space direction="vertical" style={{ width: "100%" }} size={8}>
                  <Row gutter={8}>
                    <Col span={24}>
                      <Labeled label={t("skCustomName")}>
                        <Input disabled={activeIndex !== 0} value={activeConfig?.text ?? ""} onChange={(e) => applyCustomConfig({ text: e.target.value })} />
                      </Labeled>
                    </Col>
                  </Row>
                  <Row gutter={8}>
                    <Col span={12}>
                      <Labeled label={t("skLedHue")}>
                        <InputNumber disabled={activeIndex !== 0} min={0} max={255} step={1} value={activeConfig?.led_hue ?? 120} onChange={(v) => applyCustomConfig({ led_hue: v ?? 120 })} style={{ width: "100%" }} />
                      </Labeled>
                    </Col>
                    <Col span={12}>
                      <Labeled label={t("skSnapPoint")}>
                        <InputNumber disabled={activeIndex !== 0} min={0.5} max={1.1} step={0.01} value={activeConfig?.snap_point ?? 0.55} onChange={(v) => applyCustomConfig({ snap_point: v ?? 0.55 })} style={{ width: "100%" }} />
                      </Labeled>
                    </Col>
                  </Row>
                  <Row gutter={8}>
                    <Col span={8}><Labeled label={t("skMinPos")}><InputNumber disabled={activeIndex !== 0} value={activeConfig?.min_position ?? 0} onChange={(v) => applyCustomConfig({ min_position: v ?? 0 })} style={{ width: "100%" }} /></Labeled></Col>
                    <Col span={8}><Labeled label={t("skMaxPos")}><InputNumber disabled={activeIndex !== 0} value={activeConfig?.max_position ?? -1} onChange={(v) => applyCustomConfig({ max_position: v ?? -1 })} style={{ width: "100%" }} /></Labeled></Col>
                    <Col span={8}><Labeled label={t("skPosWidth")}><InputNumber disabled={activeIndex !== 0} min={0.5} step={1} value={Math.round(radToDeg(activeConfig?.position_width_radians ?? 0.1745) * 10) / 10} onChange={(v) => applyCustomConfig({ position_width_radians: degToRad(v ?? 10) })} style={{ width: "100%" }} /></Labeled></Col>
                  </Row>
                  <Row gutter={8}>
                    <Col span={8}><Labeled label={t("skDetentStrength")}><InputNumber disabled={activeIndex !== 0} min={0} step={0.1} value={activeConfig?.detent_strength_unit ?? 0} onChange={(v) => applyCustomConfig({ detent_strength_unit: v ?? 0 })} style={{ width: "100%" }} /></Labeled></Col>
                    <Col span={8}><Labeled label={t("skEndstopStrength")}><InputNumber disabled={activeIndex !== 0} min={0} step={0.1} value={activeConfig?.endstop_strength_unit ?? 1} onChange={(v) => applyCustomConfig({ endstop_strength_unit: v ?? 1 })} style={{ width: "100%" }} /></Labeled></Col>
                  </Row>
                </Space>
              </Card>
            </Col>

            <Col xs={24} lg={13}>
              <Card title={t("skModes")} size="small">
                <Row gutter={[8, 8]}>
                  {configs.map((cfg, idx) => (
                    <Col xs={12} sm={8} key={idx}>
                      <ModeButton cfg={cfg} active={idx === activeIndex} onClick={() => pickMode(idx)} />
                    </Col>
                  ))}
                </Row>
              </Card>

              <Card title={t("skTuningFeel")} size="small" style={{ marginTop: 16 }}>
                <Typography.Text type="secondary" style={{ fontSize: 12, display: "block", marginBottom: 8 }}>
                  (p_gain x input - d_gain x velocity) x current_scale
                </Typography.Text>
                <Space wrap align="end">
                  <Labeled label={t("skPGain")}><InputNumber min={0} step={0.1} value={pGain} onChange={(v) => applyTuning(strength, torqueLimit, maxTorque, frictionComp, clickTorque, v ?? 0, dGain)} /></Labeled>
                  <Labeled label={t("skDGain")}><InputNumber min={0} step={0.001} value={dGain} onChange={(v) => applyTuning(strength, torqueLimit, maxTorque, frictionComp, clickTorque, pGain, v ?? 0)} /></Labeled>
                  <Button onClick={applyRecommendedGains}>{t("skRecommendedGains")}</Button>
                  <Labeled label="Current scale (A)"><InputNumber min={0} step={0.005} value={strength} onChange={(v) => applyTuning(v ?? 0, torqueLimit, maxTorque, frictionComp, clickTorque, pGain, dGain)} /></Labeled>
                  <Labeled label="Friction current (A)"><InputNumber min={0} max={0.5} step={0.005} value={frictionComp} onChange={(v) => applyTuning(strength, torqueLimit, maxTorque, v ?? 0, clickTorque, pGain, dGain)} /></Labeled>
                  <Labeled label="Click current (A)"><InputNumber min={0} max={0.8} step={0.005} value={clickTorque} onChange={(v) => applyTuning(strength, torqueLimit, maxTorque, frictionComp, v ?? 0, pGain, dGain)} /></Labeled>
                </Space>
              </Card>

              <Card title={t("skTuningSafety")} size="small" style={{ marginTop: 16 }}>
                <Space wrap align="end">
                  <Labeled label="Current limit (A)"><InputNumber min={0} max={1.2} step={0.05} value={torqueLimit} onChange={(v) => applyTuning(strength, v ?? 0, maxTorque, frictionComp, clickTorque, pGain, dGain)} /></Labeled>
                  <Labeled label="Motor current clamp (‰)"><InputNumber min={0} max={1000} step={50} value={maxTorque} onChange={(v) => applyTuning(strength, torqueLimit, v ?? 0, frictionComp, clickTorque, pGain, dGain)} /></Labeled>
                </Space>
              </Card>

              {running && (
                <Card title="Current" size="small" style={{ marginTop: 16 }}>
                  <Row gutter={8}>
                    <Col span={8}><Statistic title={`${t("skAngle")} (deg)`} value={fmt(degOf(state?.shaft_angle_rad), 1)} /></Col>
                    <Col span={8}><Statistic title="I cmd (A)" value={fmt(state?.applied_torque_nm)} /></Col>
                    <Col span={8}><Statistic title="I meas (A)" value={fmt(state?.measured_torque_nm)} /></Col>
                  </Row>
                  <Row gutter={8} style={{ marginTop: 8 }}>
                    <Col span={8}><Statistic title={t("skMotor")} value={state?.online ? (state?.enabled ? "on" : "idle") : "off"} /></Col>
                    <Col span={8}><Statistic title="Drv (C)" value={fmt(state?.driver_temp_c, 1)} /></Col>
                    <Col span={8}><Statistic title="Mot (C)" value={fmt(state?.motor_temp_c, 1)} /></Col>
                  </Row>
                </Card>
              )}
            </Col>
          </Row>
        </Space>
      )}
    </div>
  );
}

const SIZE = 340;
const C = SIZE / 2;
const R = 150;
const GAUGE_SPAN = 300;

function Dial({ config, state }: { config: KnobConfig | null; state: SmartKnobState | null }) {
  const { t } = useI18n();
  const hue = config ? (config.led_hue / 255) * 360 : 210;
  const accent = `hsl(${hue}, 70%, 58%)`;
  const dim = `hsl(${hue}, 30%, 32%)`;
  const num = state?.num_positions ?? (config ? positionCount(config) : 0);
  const pos = state?.current_position ?? config?.position ?? 0;
  const sub = state?.sub_position_unit ?? 0;
  const minP = state?.min_position ?? config?.min_position ?? 0;
  const maxP = state?.max_position ?? config?.max_position ?? 0;
  const endstop = state?.at_endstop ?? false;
  const running = state?.running ?? false;
  const value = pos + clamp(sub, -0.5, 0.5);
  const gauge = num >= 2 && num <= 49;
  const ticks: JSX.Element[] = [];
  let needleDeg = 0;
  if (gauge) {
    const start = 90 + (360 - GAUGE_SPAN) / 2;
    const frac = num > 1 ? (maxP - value) / (num - 1) : 0;
    needleDeg = start + clamp(frac, 0, 1) * GAUGE_SPAN;
    for (let i = 0; i < num; i++) {
      const deg = start + ((num - 1 - i) / (num - 1)) * GAUGE_SPAN;
      const active = i === pos - minP;
      ticks.push(<Tick key={i} deg={deg} color={active ? accent : dim} long={active} />);
    }
  } else {
    needleDeg = degOf(state?.shaft_angle_rad ?? 0) - 90;
    const width = config?.position_width_radians ?? Math.PI / 18;
    const tickCount = Math.min(72, Math.max(12, Math.round((2 * Math.PI) / width)));
    const baseDeg = needleDeg + (sub * width * 180) / Math.PI;
    const stepDeg = Math.max(360 / tickCount, (width * 180) / Math.PI);
    for (let i = -Math.ceil(180 / stepDeg); i <= Math.ceil(180 / stepDeg); i++) {
      const deg = baseDeg + i * stepDeg;
      ticks.push(<Tick key={i} deg={deg} color={i === 0 ? accent : dim} long={i === 0} />);
    }
  }
  const tq = state?.applied_torque_nm ?? 0;
  const tqLimit = state?.torque_limit_nm || 2;
  const tqFrac = clamp(Math.abs(tq) / tqLimit, 0, 1);
  return (
    <div className="rollercan-dial">
      <svg viewBox={`0 0 ${SIZE} ${SIZE}`} className="rollercan-dial-svg">
        <circle cx={C} cy={C} r={R} fill="none" stroke="#222831" strokeWidth={2} />
        {ticks}
        <line x1={C} y1={C} {...lineEnd(needleDeg, R - 18)} stroke={endstop ? "#ff4d4f" : accent} strokeWidth={4} strokeLinecap="round" />
        <circle cx={C} cy={C} r={8} fill={endstop ? "#ff4d4f" : accent} />
        <circle cx={C} cy={C} r={R + 10} fill="none" stroke={tq >= 0 ? accent : "#ff7875"} strokeWidth={4} strokeOpacity={0.7} strokeDasharray={`${tqFrac * 2 * Math.PI * (R + 10)} ${2 * Math.PI * (R + 10)}`} transform={`rotate(-90 ${C} ${C})`} strokeLinecap="round" />
      </svg>
      <div className="rollercan-dial-readout">
        <Typography.Title level={1} style={{ margin: 0, lineHeight: 1, color: accent }}>
          {running ? pos : "-"}
        </Typography.Title>
        <Typography.Text type="secondary">
          {config ? `${t("skValue")} ${value.toFixed(2)}` : ""}
          {endstop ? ` / ${t("skEndstop")}` : num === 0 ? ` / ${t("skUnbounded")}` : ""}
        </Typography.Text>
        <div className="rollercan-dial-label">{config?.text ?? ""}</div>
      </div>
    </div>
  );
}

function Tick({ deg, color, long }: { deg: number; color: string; long: boolean }) {
  const inner = long ? R - 22 : R - 12;
  const a = lineEnd(deg, R - 2);
  const b = lineEnd(deg, inner);
  return <line x1={b.x2} y1={b.y2} x2={a.x2} y2={a.y2} stroke={color} strokeWidth={long ? 4 : 2} strokeLinecap="round" />;
}

function ModeButton({ cfg, active, onClick }: { cfg: KnobConfig; active: boolean; onClick: () => void }) {
  const hue = (cfg.led_hue / 255) * 360;
  return (
    <Button block onClick={onClick} type={active ? "primary" : "default"} style={{ height: 56, whiteSpace: "normal", lineHeight: 1.2, fontSize: 12, borderColor: active ? undefined : `hsl(${hue}, 40%, 40%)` }}>
      {cfg.text}
    </Button>
  );
}

function Labeled({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div><Typography.Text type="secondary" style={{ fontSize: 12 }}>{label}</Typography.Text></div>
      {children}
    </div>
  );
}

async function rollercanGetState(): Promise<RollerCanKnobState> {
  return await api.rollercanGetState() as unknown as RollerCanKnobState;
}

async function writeScaled(index: number, value: number) {
  await api.rollercanWriteParam(0, 0, index, Math.round(value * 1000));
}

async function pushCustomConfig(cfg: KnobConfig, motorId: number) {
  await Promise.all([
    api.rollercanWriteParam(0, motorId, CUSTOM.position, Math.trunc(cfg.position)),
    api.rollercanWriteParam(0, motorId, CUSTOM.minPosition, Math.trunc(cfg.min_position)),
    api.rollercanWriteParam(0, motorId, CUSTOM.maxPosition, Math.trunc(cfg.max_position)),
    api.rollercanWriteParam(0, motorId, CUSTOM.widthDeg, Math.round(radToDeg(cfg.position_width_radians) * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.detentStrength, Math.round(cfg.detent_strength_unit * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.endstopStrength, Math.round(cfg.endstop_strength_unit * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.snapPoint, Math.round(cfg.snap_point * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.snapBias, Math.round(cfg.snap_point_bias * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.click, Math.round(cfg.click_torque_nm * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.friction, Math.round(cfg.friction_compensation * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.strength, Math.round(cfg.strength_scale * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.pGain, Math.round(cfg.p_gain * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.dGain, Math.round(cfg.d_gain * 1000)),
    api.rollercanWriteParam(0, motorId, CUSTOM.ledHue, Math.trunc(cfg.led_hue)),
  ]);
}

const DEG = Math.PI / 180;
const CLICK_WIDTH_THRESHOLD_RAD = 3 * DEG;

function recommendedPGain(cfg: KnobConfig): number {
  return cfg.detent_strength_unit * 4.0;
}

function recommendedDGain(cfg: KnobConfig): number {
  if (cfg.detent_positions.length > 0) return 0;
  if (cfg.click_torque_nm > 0 || cfg.position_width_radians < CLICK_WIDTH_THRESHOLD_RAD) return 0;
  const lower = cfg.detent_strength_unit * 0.08;
  const upper = cfg.detent_strength_unit * 0.02;
  const wLower = 3 * DEG;
  const wUpper = 8 * DEG;
  const raw = lower + ((upper - lower) / (wUpper - wLower)) * (cfg.position_width_radians - wLower);
  return clamp(raw, Math.min(lower, upper), Math.max(lower, upper));
}

function lineEnd(deg: number, radius: number): { x2: number; y2: number } {
  const rad = (deg * Math.PI) / 180;
  return { x2: C + radius * Math.cos(rad), y2: C + radius * Math.sin(rad) };
}

function positionCount(c: KnobConfig): number {
  return c.max_position >= c.min_position ? c.max_position - c.min_position + 1 : 0;
}

function degOf(rad: number | null | undefined): number {
  if (rad == null) return 0;
  return (rad * 180) / Math.PI;
}

function radToDeg(rad: number): number {
  return (rad * 180) / Math.PI;
}

function degToRad(deg: number): number {
  return (deg * Math.PI) / 180;
}

function clamp(x: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, x));
}

function fmt(v: number | null | undefined, digits = 3): string {
  if (v == null || Number.isNaN(v)) return "-";
  return v.toFixed(digits);
}
