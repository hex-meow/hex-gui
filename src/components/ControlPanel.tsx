import { useEffect, useState, type ReactNode } from "react";
import {
  Alert,
  App,
  Button,
  Card,
  Divider,
  InputNumber,
  Segmented,
  Space,
  Tooltip,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import type { LiveState, MotorInfo, MotorMode, MotorTarget } from "../types";

const MODES: MotorMode[] = ["ProfilePosition", "ProfileVelocity", "Torque", "Mit"];

export function ControlPanel({
  info,
  live,
}: {
  info: MotorInfo;
  live: LiveState | null;
}) {
  const { message } = App.useApp();
  const { t } = useI18n();
  const nid = info.node_id;
  const logic = live?.logic ?? info.logic;
  const enabled = logic?.state === "Enabled";
  const faulted = logic?.state === "Error";
  const ready = info.is_ready;

  const [mode, setMode] = useState<MotorMode>("ProfileVelocity");
  const [busy, setBusy] = useState(false);

  // Per-mode target inputs.
  const [pp, setPp] = useState(0.1);
  const [pv, setPv] = useState(0.5);
  const [tq, setTq] = useState(0.0);
  const [mit, setMit] = useState({ pos: 0, vel: 0, tor: 0, kp: 0, kd: 0.1 });
  const [maxPermille, setMaxPermille] = useState(200);

  // While enabled, lock the selector to the actually-enabled mode.
  useEffect(() => {
    if (enabled && logic?.state === "Enabled") setMode(logic.mode);
  }, [enabled, logic]);

  const run = async (label: string, fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      message.success(label);
    } catch (e) {
      message.error(`${label} ${t("failed")}: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  };

  // MIT inputs are SI (rad / rad·s); the motor + hex-motor use Rev. Convert
  // here: pos/vel rad→rev is ÷2π; kp/kd stiffness/damping rad→rev is ×2π
  // (torque = kp·err; err[rad] = 2π·err[rev], so kp[Nm/Rev] = kp[Nm/rad]·2π).
  const TAU = 2 * Math.PI;
  const buildTarget = (): MotorTarget => {
    switch (mode) {
      case "ProfilePosition":
        return { kind: "Position", rev: pp };
      case "ProfileVelocity":
        return { kind: "Velocity", rev_per_s: pv };
      case "Torque":
        return { kind: "Torque", nm: tq };
      case "Mit":
        return {
          kind: "Mit",
          pos: mit.pos / TAU,
          vel: mit.vel / TAU,
          tor: mit.tor,
          kp: mit.kp * TAU,
          kd: mit.kd * TAU,
        };
    }
  };

  const peak = info.peak_torque_nm;
  const maxNm = peak != null ? (peak * maxPermille) / 1000 : null;

  return (
    <Card size="small" title={t("control")}>
      {faulted && (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 12 }}
          message={`${t("motorFault")}: ${logic.kind} @ 0x${logic.raw_code
            .toString(16)
            .toUpperCase()
            .padStart(4, "0")}`}
          description={t("faultDesc")}
        />
      )}

      {/* Mode + enable/disable */}
      <Space wrap align="center">
        <Typography.Text type="secondary">{t("mode")}</Typography.Text>
        <Segmented
          value={mode}
          disabled={enabled || !ready}
          onChange={(v) => setMode(v as MotorMode)}
          options={MODES.map((m) => ({ label: t(`mode_${m}`), value: m }))}
        />
        <Button
          type="primary"
          disabled={!ready || enabled}
          loading={busy}
          onClick={() => run(t("enable"), () => api.setMode(nid, mode))}
        >
          {t("enable")}
        </Button>
        <Button disabled={!ready} loading={busy} onClick={() => run(t("disableAction"), () => api.disable(nid))}>
          {t("disableAction")}
        </Button>
        <Button disabled={!ready} loading={busy} onClick={() => run(t("clearError"), () => api.clearError(nid))}>
          {t("clearError")}
        </Button>
        {info.can_initialize || info.lifecycle.kind === "Initialized" ? (
          <Button
            disabled={!info.online}
            loading={busy}
            onClick={() => run(info.lifecycle.kind === "Initialized" ? t("reinitialize") : t("initialize"), () => api.initialize(nid))}
          >
            {info.lifecycle.kind === "Initialized" ? t("reinitialize") : t("initialize")}
          </Button>
        ) : null}
      </Space>

      <Divider style={{ margin: "12px 0" }} />

      {/* Target inputs (depend on mode). Sending requires the motor enabled. */}
      <Space wrap align="end">
        {mode === "ProfilePosition" && (
          <Field label={t("posFieldPP")}>
            <InputNumber value={pp} step={0.01} onChange={(v) => setPp(v ?? 0)} />
          </Field>
        )}
        {mode === "ProfileVelocity" && (
          <Field label={t("velocity")}>
            <InputNumber value={pv} step={0.1} onChange={(v) => setPv(v ?? 0)} />
          </Field>
        )}
        {mode === "Torque" && (
          <Field label={t("torque")}>
            <InputNumber value={tq} step={0.01} onChange={(v) => setTq(v ?? 0)} />
          </Field>
        )}
        {mode === "Mit" && (
          <>
            <Field label={t("mitPos")}>
              <InputNumber value={mit.pos} step={0.05} onChange={(v) => setMit({ ...mit, pos: v ?? 0 })} />
            </Field>
            <Field label={t("mitVel")}>
              <InputNumber value={mit.vel} step={0.5} onChange={(v) => setMit({ ...mit, vel: v ?? 0 })} />
            </Field>
            <Field label={t("mitTor")}>
              <InputNumber value={mit.tor} step={0.01} onChange={(v) => setMit({ ...mit, tor: v ?? 0 })} />
            </Field>
            <Field label={t("mitKp")}>
              <InputNumber value={mit.kp} step={0.1} onChange={(v) => setMit({ ...mit, kp: v ?? 0 })} />
            </Field>
            <Field label={t("mitKd")}>
              <InputNumber value={mit.kd} step={0.05} onChange={(v) => setMit({ ...mit, kd: v ?? 0 })} />
            </Field>
          </>
        )}
        <Tooltip title={!enabled ? t("enableFirst") : ""}>
          <Button
            type="primary"
            disabled={!enabled}
            loading={busy}
            onClick={() => run(t("sendTarget"), () => api.setTarget(nid, buildTarget()))}
          >
            {t("sendTarget")}
          </Button>
        </Tooltip>
      </Space>

      <Divider style={{ margin: "12px 0" }} />

      {/* Max torque (0x6072), any mode */}
      <Space align="end">
        <Field label={t("maxTorqueField")}>
          <InputNumber
            value={maxPermille}
            min={0}
            max={1000}
            step={10}
            onChange={(v) => setMaxPermille(v ?? 0)}
          />
        </Field>
        <Button disabled={!ready} loading={busy} onClick={() => run(t("limitMaxTorque"), () => api.setMaxTorque(nid, Math.round(maxPermille)))}>
          {t("apply")}
        </Button>
        <Typography.Text type="secondary">
          {maxNm != null ? `≈ ${maxNm.toFixed(3)} Nm` : t("peakUnknown")}
        </Typography.Text>
      </Space>
    </Card>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <div style={{ fontSize: 12, color: "#8a93a3", marginBottom: 2 }}>{label}</div>
      {children}
    </div>
  );
}
