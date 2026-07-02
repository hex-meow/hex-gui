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
import { nid2hex } from "../format";
import type { KnobConfig, MotorInfo, SmartKnobState } from "../types";

const POLL_MS = 40; // 25 Hz UI poll (haptic loop runs at 1 kHz in Rust)

interface PerModeTuning {
  pGain: number;
  dGain: number;
  strength: number;
  torqueLimit: number;
  maxTorque: number;
  frictionComp: number;
  clickTorque: number;
}

export function SmartKnobPanel({ connected, devices }: { connected: boolean; devices: MotorInfo[] }) {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();

  const [configs, setConfigs] = useState<KnobConfig[]>([]);
  const [selectedNid, setSelectedNid] = useState<number | null>(null);
  const [modeIndex, setModeIndex] = useState(0);

  const [running, setRunning] = useState(false);
  const [starting, setStarting] = useState(false);
  const [state, setState] = useState<SmartKnobState | null>(null);

  // Tuning (local; applied live to the backend).
  const [strength, setStrength] = useState(0.15);
  const [torqueLimit, setTorqueLimit] = useState(2.0);
  const [maxTorque, setMaxTorque] = useState(700);
  const [frictionComp, setFrictionComp] = useState(0.0);
  const [clickTorque, setClickTorque] = useState(0.0);
  const [pGain, setPGain] = useState(0.0);
  const [dGain, setDGain] = useState(0.0);

  // Custom mode config editing (only meaningful for mode index 0).
  const [customConfig, setCustomConfig] = useState<KnobConfig | null>(null);

  // Per-mode tuning RAM — survives mode switches so the user doesn't lose
  // their tweaks.  Lazy: only populated when the user touches a slider.
  const perModeTuning = useRef<Map<number, PerModeTuning>>(new Map());

  // Fetch the preset list once (it's static, connection-independent).
  // Also seed the custom config placeholder from index 0.
  useEffect(() => {
    api.smartknobConfigs().then((cfgs) => {
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

  // Auto-select the first discovered motor.
  useEffect(() => {
    if (selectedNid == null && devices.length > 0) setSelectedNid(devices[0].node_id);
  }, [devices, selectedNid]);

  // Poll backend state while running.
  useEffect(() => {
    if (!running) return;
    let alive = true;
    const tick = async () => {
      try {
        const s = await api.smartknobGetState();
        if (alive) {
          setState(s);
          // Sync tuning values so config-driven changes (e.g. detent_strength →
          // p_gain) propagate to the sliders.  strength_scale is excluded —
          // the user controls it independently via the Tuning — Feel slider.
          setPGain(s.p_gain);
          setDGain(s.d_gain);
          setTorqueLimit(s.torque_limit_nm);
          setMaxTorque(s.max_torque_permille);
          setFrictionComp(s.friction_compensation);
          setClickTorque(s.click_torque_nm);
        }
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
  }, [running]);

  // If the bus drops under us, return to the stopped view.
  useEffect(() => {
    if (!connected && running) {
      setRunning(false);
      setState(null);
    }
  }, [connected, running]);

  const start = useCallback(async () => {
    if (selectedNid == null) return;
    setStarting(true);
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
      await api.smartknobStart(selectedNid, modeIndex);
      // If starting in custom mode, push the current custom config.
      if (modeIndex === 0 && customConfig) {
        await api.smartknobSetCustomConfig({
          ...customConfig,
          strength_scale: startStrength,
          friction_compensation: startFriction,
          click_torque_nm: startClick,
          p_gain: startPGain,
          d_gain: startDGain,
        });
      }
      await api.smartknobSetTuning(
        startPGain,
        startDGain,
        startStrength,
        startTorqueLimit,
        startMaxTorque,
        startFriction,
        startClick,
      );
      setRunning(true);
      message.success(t("skRunning"));
    } catch (e) {
      message.error(`${t("skStartFailed")}: ${errMsg(e)}`);
    } finally {
      setStarting(false);
    }
  }, [selectedNid, modeIndex, configs, customConfig, pGain, dGain, strength, torqueLimit, maxTorque, frictionComp, clickTorque, message, t]);

  const stop = useCallback(async () => {
    try {
      await api.smartknobStop();
    } catch (e) {
      message.error(errMsg(e));
    }
    setRunning(false);
    setState(null);
  }, [message]);

  const pickMode = useCallback(
    (idx: number) => {
      setModeIndex(idx);
      // Restore per-mode tuning if the user has touched this mode before;
      // otherwise fall back to the preset defaults.
      const saved = perModeTuning.current.get(idx);
      let s: number, tl: number, mt: number, fc: number, ct: number, pg: number, dg: number;
      if (saved) {
        s = saved.strength;
        tl = saved.torqueLimit;
        mt = saved.maxTorque;
        fc = saved.frictionComp;
        ct = saved.clickTorque;
        pg = saved.pGain;
        dg = saved.dGain;
      } else {
        s = configs[idx]?.strength_scale ?? 0.15;
        tl = torqueLimit;
        mt = maxTorque;
        fc = configs[idx]?.friction_compensation ?? 0;
        ct = configs[idx]?.click_torque_nm ?? 0;
        pg = configs[idx]?.p_gain ?? 0;
        dg = configs[idx]?.d_gain ?? 0;
      }
      setStrength(s);
      setTorqueLimit(tl);
      setMaxTorque(mt);
      setFrictionComp(fc);
      setClickTorque(ct);
      setPGain(pg);
      setDGain(dg);
      if (idx === 0 && customConfig) {
        setCustomConfig({
          ...customConfig,
          strength_scale: s,
          friction_compensation: fc,
          click_torque_nm: ct,
          p_gain: pg,
          d_gain: dg,
        });
      }
      if (running) {
        api.smartknobSetConfig(idx).catch(() => {});
        // When switching to custom mode, push the local custom config so the
        // backend picks up any edits made while stopped.
        if (idx === 0 && customConfig) {
          api.smartknobSetCustomConfig(customConfig).catch(() => {});
        }
        api.smartknobSetTuning(pg, dg, s, tl, mt, fc, ct).catch(() => {});
      }
    },
    [running, configs, torqueLimit, maxTorque, customConfig]
  );

  const applyTuning = useCallback(
    (s: number, tl: number, mt: number, fc: number, ct: number, pg: number, dg: number) => {
      setStrength(s);
      setTorqueLimit(tl);
      setMaxTorque(mt);
      setFrictionComp(fc);
      setClickTorque(ct);
      setPGain(pg);
      setDGain(dg);
      if (modeIndex === 0) {
        setCustomConfig((prev) => prev ? {
          ...prev,
          strength_scale: s,
          friction_compensation: fc,
          click_torque_nm: ct,
          p_gain: pg,
          d_gain: dg,
        } : prev);
      }
      // Persist into the per-mode RAM slot for the currently-active mode.
      perModeTuning.current.set(modeIndex, {
        strength: s, torqueLimit: tl, maxTorque: mt, frictionComp: fc,
        clickTorque: ct, pGain: pg, dGain: dg,
      });
      if (running) api.smartknobSetTuning(pg, dg, s, tl, mt, fc, ct).catch(() => {});
    },
    [running, modeIndex]
  );

  // Custom mode config editor: merge updates into local state (immediate
  // dial feedback) and push to the backend if running.
  const applyCustomConfig = useCallback(
    (updates: Partial<KnobConfig>) => {
      setCustomConfig((prev) => {
        if (!prev) return prev;
        const next: KnobConfig = {
          ...prev,
          strength_scale: strength,
          friction_compensation: frictionComp,
          click_torque_nm: clickTorque,
          p_gain: pGain,
          d_gain: dGain,
          ...updates,
        };
        if (modeIndex === 0 && updates.detent_strength_unit !== undefined) {
          next.p_gain = recommendedPGain(next);
          next.d_gain = recommendedDGain(next);
          setPGain(next.p_gain);
          setDGain(next.d_gain);
          perModeTuning.current.set(modeIndex, {
            strength,
            torqueLimit,
            maxTorque,
            frictionComp,
            clickTorque,
            pGain: next.p_gain,
            dGain: next.d_gain,
          });
          if (running) {
            api.smartknobSetTuning(
              next.p_gain,
              next.d_gain,
              strength,
              torqueLimit,
              maxTorque,
              frictionComp,
              clickTorque,
            ).catch(() => {});
          }
        }
        if (running) {
          api.smartknobSetCustomConfig(next).catch(() => {});
        }
        return next;
      });
    },
    [running, modeIndex, strength, torqueLimit, maxTorque, frictionComp, clickTorque, pGain, dGain],
  );

  const applyRecommendedGains = useCallback(() => {
    const cfg = modeIndex === 0 ? customConfig : configs[modeIndex];
    if (!cfg) return;
    const nextPGain = recommendedPGain(cfg);
    const nextDGain = recommendedDGain(cfg);
    applyTuning(strength, torqueLimit, maxTorque, frictionComp, clickTorque, nextPGain, nextDGain);
  }, [modeIndex, customConfig, configs, applyTuning, strength, torqueLimit, maxTorque, frictionComp, clickTorque]);

  const clearError = useCallback(async () => {
    try {
      await api.smartknobClearError();
      message.success(t("skCleared"));
    } catch (e) {
      message.error(errMsg(e));
    }
  }, [message, t]);

  if (!connected) {
    return (
      <div style={{ paddingTop: 80 }}>
        <Empty description={t("skConnectFirst")} />
      </div>
    );
  }

  const activeIndex = running ? state?.config_index ?? modeIndex : modeIndex;
  // Use local customConfig for immediate dial feedback when in custom mode;
  // otherwise prefer the backend's live config, falling back to presets.
  const activeConfig =
    (activeIndex === 0 && customConfig)
      ? customConfig
      : (state?.config ?? configs[activeIndex] ?? null);

  return (
    <Space direction="vertical" size={16} style={{ width: "100%" }}>
      <Card>
        <Space wrap>
          {!running ? (
            <>
              <Typography.Text type="secondary">{t("skMotor")}:</Typography.Text>
              <Select
                style={{ width: 220 }}
                placeholder={t("skNoMotors")}
                value={selectedNid ?? undefined}
                onChange={setSelectedNid}
                options={devices.map((d) => ({
                  value: d.node_id,
                  label: `${nid2hex(d.node_id)} — ${d.friendly_name}`,
                }))}
              />
              <Button
                type="primary"
                loading={starting}
                disabled={selectedNid == null}
                onClick={start}
              >
                {starting ? t("skStarting") : t("skStart")}
              </Button>
            </>
          ) : (
            <>
              <Button danger onClick={stop}>
                {t("skStop")}
              </Button>
              <Button onClick={clearError}>{t("skClearError")}</Button>
            </>
          )}
          <Tag color={running ? "green" : "default"}>{running ? t("skRunning") : t("skStopped")}</Tag>
          {state?.error && <Tag color="red">{state.error}</Tag>}
        </Space>
      </Card>

      <Row gutter={16}>
        <Col xs={24} lg={11}>
          <Card>
            <Dial config={activeConfig} state={state} />
          </Card>

          {/* Mode config params — editable for custom mode, locked for presets. */}
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
                    <Input
                      disabled={activeIndex !== 0}
                      value={activeConfig?.text ?? ""}
                      onChange={(e) => applyCustomConfig({ text: e.target.value })}
                      placeholder={t("skCustomName")}
                    />
                  </Labeled>
                </Col>
              </Row>
              <Row gutter={8}>
                <Col span={12}>
                  <Labeled label={t("skLedHue")}>
                    <InputNumber
                      disabled={activeIndex !== 0}
                      min={0} max={255} step={1}
                      value={activeConfig?.led_hue ?? 120}
                      onChange={(v) => applyCustomConfig({ led_hue: v ?? 120 })}
                      style={{ width: "100%" }}
                    />
                  </Labeled>
                </Col>
                <Col span={12}>
                  <Labeled label={t("skSnapPoint")}>
                    <InputNumber
                      disabled={activeIndex !== 0}
                      min={0.5} max={1.1} step={0.01}
                      value={activeConfig?.snap_point ?? 0.55}
                      onChange={(v) => applyCustomConfig({ snap_point: v ?? 0.55 })}
                      style={{ width: "100%" }}
                    />
                  </Labeled>
                </Col>
              </Row>
              <Row gutter={8}>
                <Col span={8}>
                  <Labeled label={t("skMinPos")}>
                    <InputNumber
                      disabled={activeIndex !== 0}
                      value={activeConfig?.min_position ?? 0}
                      onChange={(v) => applyCustomConfig({ min_position: v ?? 0 })}
                      style={{ width: "100%" }}
                    />
                  </Labeled>
                </Col>
                <Col span={8}>
                  <Labeled label={t("skMaxPos")}>
                    <InputNumber
                      disabled={activeIndex !== 0}
                      value={activeConfig?.max_position ?? -1}
                      onChange={(v) => applyCustomConfig({ max_position: v ?? -1 })}
                      style={{ width: "100%" }}
                    />
                  </Labeled>
                </Col>
                <Col span={8}>
                  <Labeled label={t("skPosWidth")}>
                    <InputNumber
                      disabled={activeIndex !== 0}
                      min={0.5} step={1}
                      value={Math.round(radToDeg(activeConfig?.position_width_radians ?? 0.1745) * 10) / 10}
                      onChange={(v) => applyCustomConfig({ position_width_radians: degToRad(v ?? 10) })}
                      style={{ width: "100%" }}
                    />
                  </Labeled>
                </Col>
              </Row>
              <Row gutter={8}>
                <Col span={8}>
                  <Labeled label={t("skDetentStrength")}>
                    <InputNumber
                      disabled={activeIndex !== 0}
                      min={0} step={0.1}
                      value={activeConfig?.detent_strength_unit ?? 0}
                      onChange={(v) => applyCustomConfig({ detent_strength_unit: v ?? 0 })}
                      style={{ width: "100%" }}
                    />
                  </Labeled>
                </Col>
                <Col span={8}>
                  <Labeled label={t("skEndstopStrength")}>
                    <InputNumber
                      disabled={activeIndex !== 0}
                      min={0} step={0.1}
                      value={activeConfig?.endstop_strength_unit ?? 1}
                      onChange={(v) => applyCustomConfig({ endstop_strength_unit: v ?? 1 })}
                      style={{ width: "100%" }}
                    />
                  </Labeled>
                </Col>
              </Row>
            </Space>
          </Card>
        </Col>
        <Col xs={24} lg={13}>
          <Card title={t("skModes")} size="small">
            <Row gutter={[8, 8]}>
              {configs.map((cfg, idx) => (
                <Col xs={12} sm={8} key={idx}>
                  <ModeButton
                    cfg={cfg}
                    active={idx === activeIndex}
                    onClick={() => pickMode(idx)}
                  />
                </Col>
              ))}
            </Row>
          </Card>

          <Card title={t("skTuningFeel")} size="small" style={{ marginTop: 16 }}>
            <Typography.Text type="secondary" style={{ fontSize: 12, display: "block", marginBottom: 8 }}>
              (p_gain &times; input &minus; d_gain &times; velocity) &times; strength_scale
            </Typography.Text>
            <Space wrap align="end">
              <Labeled label={t("skPGain")}>
                <InputNumber
                  min={0}
                  step={0.1}
                  value={pGain}
                  onChange={(v) => applyTuning(strength, torqueLimit, maxTorque, frictionComp, clickTorque, v ?? 0, dGain)}
                />
              </Labeled>
              <Labeled label={t("skDGain")}>
                <InputNumber
                  min={0}
                  step={0.001}
                  value={dGain}
                  onChange={(v) => applyTuning(strength, torqueLimit, maxTorque, frictionComp, clickTorque, pGain, v ?? 0)}
                />
              </Labeled>
              <Button onClick={applyRecommendedGains}>
                {t("skRecommendedGains")}
              </Button>
              <Labeled label={t("skStrength")}>
                <InputNumber
                  min={0}
                  step={0.01}
                  value={strength}
                  onChange={(v) => applyTuning(v ?? 0, torqueLimit, maxTorque, frictionComp, clickTorque, pGain, dGain)}
                />
              </Labeled>
              <Labeled label={t("skFrictionComp")}>
                <InputNumber
                  min={0}
                  max={0.5}
                  step={0.005}
                  value={frictionComp}
                  onChange={(v) => applyTuning(strength, torqueLimit, maxTorque, v ?? 0, clickTorque, pGain, dGain)}
                />
              </Labeled>
              <Labeled label={t("skClickTorque")}>
                <InputNumber
                  min={0}
                  max={2.0}
                  step={0.01}
                  value={clickTorque}
                  onChange={(v) => applyTuning(strength, torqueLimit, maxTorque, frictionComp, v ?? 0, pGain, dGain)}
                />
              </Labeled>
            </Space>
          </Card>

          <Card title={t("skTuningSafety")} size="small" style={{ marginTop: 16 }}>
            <Space wrap align="end">
              <Labeled label={t("skTorqueLimit")}>
                <InputNumber
                  min={0}
                  step={0.1}
                  value={torqueLimit}
                  onChange={(v) => applyTuning(strength, v ?? 0, maxTorque, frictionComp, clickTorque, pGain, dGain)}
                />
              </Labeled>
              <Labeled label={t("skMaxTorque")}>
                <InputNumber
                  min={0}
                  max={1000}
                  step={50}
                  value={maxTorque}
                  onChange={(v) => applyTuning(strength, torqueLimit, v ?? 0, frictionComp, clickTorque, pGain, dGain)}
                />
              </Labeled>
            </Space>
          </Card>

          {running && (
            <Card title={t("skTorque")} size="small" style={{ marginTop: 16 }}>
              <Row gutter={8}>
                <Col span={8}>
                  <Statistic title={t("skAngle") + " (°)"} value={fmt(degOf(state?.shaft_angle_rad), 1)} />
                </Col>
                <Col span={8}>
                  <Statistic title="τ cmd (Nm)" value={fmt(state?.applied_torque_nm)} />
                </Col>
                <Col span={8}>
                  <Statistic title="τ meas (Nm)" value={fmt(state?.measured_torque_nm)} />
                </Col>
              </Row>
              <Row gutter={8} style={{ marginTop: 8 }}>
                <Col span={8}>
                  <Statistic
                    title={t("skMotor")}
                    value={state?.online ? (state?.enabled ? "on" : "idle") : "off"}
                  />
                </Col>
                <Col span={8}>
                  <Statistic title="Drv (℃)" value={fmt(state?.driver_temp_c, 1)} />
                </Col>
                <Col span={8}>
                  <Statistic title="Mot (℃)" value={fmt(state?.motor_temp_c, 1)} />
                </Col>
              </Row>
            </Card>
          )}
        </Col>
      </Row>
    </Space>
  );
}

// ─────────────────────────────── the dial ───────────────────────────────────

const SIZE = 340;
const C = SIZE / 2;
const R = 150;
const GAUGE_SPAN = 300; // degrees for the bounded gauge (gap at the bottom)

function Dial({ config, state }: { config: KnobConfig | null; state: SmartKnobState | null }) {
  const { t } = useI18n();
  const hue = config ? (config.led_hue / 255) * 360 : 210;
  const accent = `hsl(${hue}, 70%, 58%)`;
  const dim = `hsl(${hue}, 30%, 32%)`;

  const num = state?.num_positions ?? (config ? positionCount(config) : 0);
  const pos = state?.current_position ?? config?.position ?? 0;
  const sub = state?.sub_position_unit ?? 0; // pointer offset toward next detent, in (−1..1)
  const minP = state?.min_position ?? config?.min_position ?? 0;
  const maxP = state?.max_position ?? config?.max_position ?? 0;
  const endstop = state?.at_endstop ?? false;
  const running = state?.running ?? false;

  // Continuous value (for display) = position + fractional sub-position.
  const value = pos + clamp(sub, -0.5, 0.5);
  // Gauge mode for a small, bounded count; otherwise a free-rotation dial.
  const gauge = num >= 2 && num <= 49;

  const ticks: JSX.Element[] = [];
  let needleDeg = 0;

  if (gauge) {
    // Bounded value gauge: spread positions across a 300° arc, gap at bottom.
    const start = 90 + (360 - GAUGE_SPAN) / 2; // 120° (SVG: 0°=+x, CW positive here)
    const frac = num > 1 ? (maxP - value) / (num - 1) : 0;
    needleDeg = start + clamp(frac, 0, 1) * GAUGE_SPAN;
    for (let i = 0; i < num; i++) {
      const deg = start + ((num - 1 - i) / (num - 1)) * GAUGE_SPAN;
      const active = i === pos - minP;
      ticks.push(
        <Tick key={i} deg={deg} color={active ? accent : dim} long={active} />
      );
    }
  } else {
    // Free-rotation dial: needle = physical shaft angle; detent pips around it.
    needleDeg = degOf(state?.shaft_angle_rad ?? 0) - 90; // 0 rad → 12 o'clock
    const width = config?.position_width_radians ?? Math.PI / 18;
    const tickCount = Math.min(72, Math.max(12, Math.round((2 * Math.PI) / width)));
    // Nearest detent center sits at sub*width radians ahead of the needle.
    const baseDeg = needleDeg + (sub * width * 180) / Math.PI;
    const stepDeg = Math.max(360 / tickCount, (width * 180) / Math.PI);
    for (let i = -Math.ceil(180 / stepDeg); i <= Math.ceil(180 / stepDeg); i++) {
      const deg = baseDeg + i * stepDeg;
      ticks.push(<Tick key={i} deg={deg} color={i === 0 ? accent : dim} long={i === 0} />);
    }
  }

  // Torque indicator (a small arc whose length ∝ |applied torque| up to limit).
  const tq = state?.applied_torque_nm ?? 0;
  const tqLimit = state?.torque_limit_nm || 2;
  const tqFrac = clamp(Math.abs(tq) / tqLimit, 0, 1);

  return (
    <div style={{ display: "flex", flexDirection: "column", alignItems: "center" }}>
      <svg viewBox={`0 0 ${SIZE} ${SIZE}`} style={{ width: "100%", maxWidth: SIZE, aspectRatio: "1 / 1" }}>
        {/* track */}
        <circle cx={C} cy={C} r={R} fill="none" stroke="#222831" strokeWidth={2} />
        {ticks}
        {/* needle */}
        <line
          x1={C}
          y1={C}
          {...lineEnd(needleDeg, R - 18)}
          stroke={endstop ? "#ff4d4f" : accent}
          strokeWidth={4}
          strokeLinecap="round"
        />
        <circle cx={C} cy={C} r={8} fill={endstop ? "#ff4d4f" : accent} />
        {/* torque ring */}
        <circle
          cx={C}
          cy={C}
          r={R + 10}
          fill="none"
          stroke={tq >= 0 ? accent : "#ff7875"}
          strokeWidth={4}
          strokeOpacity={0.7}
          strokeDasharray={`${tqFrac * 2 * Math.PI * (R + 10)} ${2 * Math.PI * (R + 10)}`}
          transform={`rotate(-90 ${C} ${C})`}
          strokeLinecap="round"
        />
      </svg>

      <div style={{ textAlign: "center", marginTop: 4 }}>
        <Typography.Title level={1} style={{ margin: 0, lineHeight: 1, color: accent }}>
          {running ? pos : "—"}
        </Typography.Title>
        <Typography.Text type="secondary">
          {config ? `${t("skValue")} ${value.toFixed(2)}` : ""}
          {endstop ? ` · ${t("skEndstop")}` : num === 0 ? ` · ${t("skUnbounded")}` : ""}
        </Typography.Text>
        <div style={{ marginTop: 6, whiteSpace: "pre-line", fontWeight: 500 }}>
          {config?.text ?? ""}
        </div>
      </div>
    </div>
  );
}

function Tick({ deg, color, long }: { deg: number; color: string; long: boolean }) {
  const inner = long ? R - 22 : R - 12;
  const a = lineEnd(deg, R - 2);
  const b = lineEnd(deg, inner);
  return (
    <line
      x1={b.x2}
      y1={b.y2}
      x2={a.x2}
      y2={a.y2}
      stroke={color}
      strokeWidth={long ? 4 : 2}
      strokeLinecap="round"
    />
  );
}

function ModeButton({ cfg, active, onClick }: { cfg: KnobConfig; active: boolean; onClick: () => void }) {
  const hue = (cfg.led_hue / 255) * 360;
  return (
    <Button
      block
      onClick={onClick}
      type={active ? "primary" : "default"}
      style={{
        height: 56,
        whiteSpace: "normal",
        lineHeight: 1.2,
        fontSize: 12,
        borderColor: active ? undefined : `hsl(${hue}, 40%, 40%)`,
      }}
    >
      {cfg.text}
    </Button>
  );
}

function Labeled({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div>
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          {label}
        </Typography.Text>
      </div>
      {children}
    </div>
  );
}

// ─────────────────────────────── helpers ────────────────────────────────────

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

/** End coordinates of a line from center at `deg` (0°=+x, CW) and radius. */
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
  if (v == null || Number.isNaN(v)) return "—";
  return v.toFixed(digits);
}
