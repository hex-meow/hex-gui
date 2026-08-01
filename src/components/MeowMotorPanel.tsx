import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Alert,
  App,
  Button,
  Card,
  Descriptions,
  Divider,
  InputNumber,
  Segmented,
  Select,
  Space,
  Switch,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { nid2hex } from "../format";
import { useI18n } from "../i18n";
import type {
  MeowCanSettingsRequest,
  MeowMotorSnapshot,
  MeowMotorTarget,
  MotorInfo,
  MotorMode,
} from "../types";

const MODES: MotorMode[] = ["ProfilePosition", "ProfileVelocity", "Torque", "Mit"];
const NOMINAL_BITRATES = [125_000, 250_000, 500_000, 800_000, 1_000_000];
const DATA_BITRATES = [1_000_000, 2_000_000, 4_000_000, 5_000_000];
const SIGNED_Q8_24_MAX_REV = 127.999_999_940_395_36;

export function MeowMotorPanel({
  info,
  connected,
  settingsOnly = false,
  onBusyChange,
}: {
  info: MotorInfo;
  connected: boolean;
  settingsOnly?: boolean;
  onBusyChange?: (busy: boolean) => void;
}) {
  const { message } = App.useApp();
  const { t } = useI18n();
  const [snapshot, setSnapshot] = useState<MeowMotorSnapshot | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [rateHz, setRateHz] = useState<500 | 1000>(1000);
  const [mode, setMode] = useState<MotorMode>("ProfileVelocity");
  const positionSeeded = useRef(false);
  const previousEnabledMode = useRef<MotorMode | null>(null);

  const [position, setPosition] = useState(0);
  const [velocity, setVelocity] = useState(0);
  const [torquePermille, setTorquePermille] = useState(0);
  const [mit, setMit] = useState({
    position_rev: 0,
    velocity_rev_per_s: 0,
    torque_nm: 0,
    kp: 0,
    kd: 0,
    kp_kd_limit_permille: 0,
  });
  const [maxTorque, setMaxTorque] = useState(50);
  const [profile, setProfile] = useState({
    velocity_rev_per_s: 0.2,
    acceleration_rev_per_s2: 0.2,
    deceleration_rev_per_s2: 0.2,
  });
  const [settings, setSettings] = useState<MeowCanSettingsRequest | null>(null);

  const acceptSnapshot = (next: MeowMotorSnapshot) => {
    setSnapshot(next);
    setLoadError(null);
    if (!positionSeeded.current && next.measurements.position_rev != null) {
      positionSeeded.current = true;
      setPosition(next.measurements.position_rev);
    }
    if (next.can_config.status === "available") {
      const config = next.can_config.config;
      setSettings((current) =>
        current ?? {
          node_id: config.stored_node_id,
          nominal_bitrate: config.nominal_bitrate,
          data_bitrate: config.data_bitrate ?? 5_000_000,
          transmit_pdo_brs: config.transmit_pdo_brs ?? false,
        },
      );
    }
  };

  const refresh = async () => {
    const next = await api.meowGetStatus(info.node_id);
    acceptSnapshot(next);
  };

  useEffect(() => {
    if (!connected) return;
    let alive = true;
    positionSeeded.current = false;
    setSnapshot(null);
    setLoadError(null);
    setSettings(null);
    const load = async () => {
      try {
        const current = await api.meowGetStatus(info.node_id);
        if (
          current.lifecycle.kind !== "Unknown" &&
          current.lifecycle.kind !== "UnsupportedIdentity"
        ) {
          return current;
        }
      } catch {
        // The explicit manager may not have seen a heartbeat yet. Identification
        // creates the entry and applies the exact 0x1018 product gate.
      }
      return api.meowIdentify(info.node_id);
    };
    void load()
      .then((next) => {
        if (alive) acceptSnapshot(next);
      })
      .catch((error) => {
        if (alive) setLoadError(errMsg(error));
      });
    const timer = window.setInterval(() => {
      void api
        .meowGetStatus(info.node_id)
        .then((next) => {
          if (alive) acceptSnapshot(next);
        })
        .catch((error) => {
          if (alive) setLoadError(errMsg(error));
        });
    }, 100);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [connected, info.node_id]);

  const enabledMode = snapshot?.logic?.state === "Enabled" ? snapshot.logic.mode : null;

  useEffect(() => {
    if (enabledMode) setMode(enabledMode);
    if (
      enabledMode === "ProfilePosition" &&
      previousEnabledMode.current !== "ProfilePosition" &&
      snapshot?.measurements.position_rev != null
    ) {
      // set_mode_sdo holds the fresh feedback position. Mirror that safe target
      // into the editable field so a later Send Target cannot jump to a stale PP value.
      setPosition(snapshot.measurements.position_rev);
    }
    previousEnabledMode.current = enabledMode;
  }, [enabledMode, snapshot?.measurements.position_rev]);

  const run = async (label: string, action: () => Promise<unknown>) => {
    setBusy(true);
    onBusyChange?.(true);
    try {
      await action();
      await refresh();
      message.success(label);
    } catch (error) {
      message.error(`${label} ${t("failed")}: ${errMsg(error)}`);
    } finally {
      setBusy(false);
      onBusyChange?.(false);
    }
  };

  const target = useMemo<MeowMotorTarget>(() => {
    switch (mode) {
      case "ProfilePosition":
        return { kind: "ProfilePosition", position_rev: position };
      case "ProfileVelocity":
        return { kind: "ProfileVelocity", velocity_rev_per_s: velocity };
      case "Torque":
        return { kind: "Torque", torque_permille: Math.round(torquePermille) };
      case "Mit":
        return { kind: "Mit", ...mit };
    }
  }, [mit, mode, position, torquePermille, velocity]);

  const canConfigure =
    snapshot?.online === true &&
    (snapshot.nmt_state === "PreOperational" || snapshot.nmt_state === "Stopped") &&
    (snapshot.lifecycle.kind === "Identified" || snapshot.lifecycle.kind === "NeedsReinit");

  return (
    <Space direction="vertical" size={12} style={{ width: "100%" }}>
      <Card size="small">
        <Space direction="vertical" size={2}>
          <Space>
            <Typography.Title level={4} style={{ margin: 0 }}>
              {snapshot?.friendly_name ?? info.friendly_name}
            </Typography.Title>
            <Typography.Text code>{nid2hex(info.node_id)}</Typography.Text>
          </Space>
          <Typography.Text type="secondary">
            {t("meowLifecycle")}: {snapshot?.lifecycle.kind ?? "Identifying"} · NMT {snapshot?.nmt_state ?? "—"}
          </Typography.Text>
        </Space>
      </Card>

      {loadError && <Alert type="error" showIcon message={t("meowIdentifyFailed")} description={loadError} />}
      {snapshot?.lifecycle.kind === "NeedsRestart" && (
        <Alert type="warning" showIcon message={t("meowPowerCycleRequired")} />
      )}

      {!settingsOnly && (
        <>
          <TelemetryCard snapshot={snapshot} />
          <Card size="small" title={t("control")}>
            <Space wrap align="end">
              <Field label={t("meowTpdoRate")}>
                <Segmented
                  value={rateHz}
                  onChange={(value) => setRateHz(value as 500 | 1000)}
                  options={[500, 1000]}
                />
              </Field>
              <Button
                loading={busy}
                disabled={!snapshot?.online}
                onClick={() => run(t("initialize"), () => api.meowInitialize(info.node_id, rateHz))}
              >
                {snapshot?.lifecycle.kind === "Initialized" ? t("reinitialize") : t("initialize")}
              </Button>
              <Button
                danger
                loading={busy}
                disabled={snapshot == null}
                onClick={() => run(t("disableAction"), () => api.meowDisable(info.node_id))}
              >
                {t("disableAction")}
              </Button>
              <Button
                loading={busy}
                disabled={snapshot == null}
                onClick={() => run(t("clearError"), () => api.meowClearError(info.node_id))}
              >
                {t("clearError")}
              </Button>
            </Space>

            <Divider style={{ margin: "12px 0" }} />
            <Alert type="info" showIcon message={t("meowOnlineModeHint")} style={{ marginBottom: 12 }} />
            <Space wrap align="end">
              <Field label={t("mode")}>
                <Segmented
                  value={mode}
                  disabled={!snapshot?.is_ready}
                  onChange={(value) => setMode(value as MotorMode)}
                  options={MODES.map((item) => ({ label: t(`mode_${item}`), value: item }))}
                />
              </Field>
              <Button
                type="primary"
                loading={busy}
                disabled={!snapshot?.is_ready}
                onClick={() => run(t("meowApplyMode"), () => api.meowSetMode(info.node_id, mode))}
              >
                {t("meowApplyMode")}
              </Button>
            </Space>

            <Divider style={{ margin: "12px 0" }} />
            <Space wrap align="end">
              {mode === "ProfilePosition" && (
                <Field label={t("meowPositionTarget")}>
                  <InputNumber
                    value={position}
                    min={-128}
                    max={SIGNED_Q8_24_MAX_REV}
                    step={0.01}
                    precision={9}
                    onChange={(value) => setPosition(value ?? 0)}
                  />
                </Field>
              )}
              {mode === "ProfileVelocity" && (
                <Field label={t("meowVelocityTarget")}>
                  <InputNumber value={velocity} step={0.1} onChange={(value) => setVelocity(value ?? 0)} />
                </Field>
              )}
              {mode === "Torque" && (
                <Field label={t("meowTorqueTarget")}>
                  <InputNumber
                    value={torquePermille}
                    min={-1000}
                    max={1000}
                    step={10}
                    onChange={(value) => setTorquePermille(value ?? 0)}
                  />
                </Field>
              )}
              {mode === "Mit" && (
                <>
                  <NumericField label="Pdes (Rev)" value={mit.position_rev} set={(value) => setMit({ ...mit, position_rev: value })} />
                  <NumericField label="Vdes (Rev/s)" value={mit.velocity_rev_per_s} set={(value) => setMit({ ...mit, velocity_rev_per_s: value })} />
                  <NumericField label="Tff (Nm)" value={mit.torque_nm} set={(value) => setMit({ ...mit, torque_nm: value })} />
                  <NumericField label="Kp (u16)" value={mit.kp} min={0} max={65535} set={(value) => setMit({ ...mit, kp: Math.round(value) })} />
                  <NumericField label="Kd (u16)" value={mit.kd} min={0} max={65535} set={(value) => setMit({ ...mit, kd: Math.round(value) })} />
                  <NumericField label="Kp/Kd limit (‰)" value={mit.kp_kd_limit_permille} min={0} max={1000} set={(value) => setMit({ ...mit, kp_kd_limit_permille: Math.round(value) })} />
                </>
              )}
              <Button
                type="primary"
                loading={busy}
                disabled={enabledMode !== mode}
                onClick={() => run(t("sendTarget"), () => api.meowSetTarget(info.node_id, target))}
              >
                {t("sendTarget")}
              </Button>
            </Space>
            {mode === "ProfilePosition" && (
              <Alert type="warning" showIcon message={t("meowPpRangeWarning")} style={{ marginTop: 12 }} />
            )}

            <Divider style={{ margin: "12px 0" }} />
            <Space wrap align="end">
              <Field label={t("meowMaxTorque")}>
                <InputNumber value={maxTorque} min={0} max={1000} step={10} onChange={(value) => setMaxTorque(value ?? 0)} />
              </Field>
              <Button
                loading={busy}
                disabled={!snapshot?.is_ready}
                onClick={() => run(t("limitMaxTorque"), () => api.meowSetMaxTorque(info.node_id, Math.round(maxTorque)))}
              >
                {t("apply")}
              </Button>
              <NumericField label={t("meowProfileVelocity")} value={profile.velocity_rev_per_s} min={0.000001} set={(value) => setProfile({ ...profile, velocity_rev_per_s: value })} />
              <NumericField label={t("meowProfileAcceleration")} value={profile.acceleration_rev_per_s2} min={0.000001} set={(value) => setProfile({ ...profile, acceleration_rev_per_s2: value })} />
              <NumericField label={t("meowProfileDeceleration")} value={profile.deceleration_rev_per_s2} min={0.000001} set={(value) => setProfile({ ...profile, deceleration_rev_per_s2: value })} />
              <Button
                loading={busy}
                disabled={!snapshot?.is_ready}
                onClick={() => run(t("meowApplyProfile"), () => api.meowSetProfileLimits(info.node_id, profile))}
              >
                {t("meowApplyProfile")}
              </Button>
            </Space>
          </Card>
        </>
      )}

      <CanSettingsCard
        snapshot={snapshot}
        settings={settings}
        setSettings={setSettings}
        canConfigure={canConfigure}
        busy={busy}
        apply={() => {
          if (settings) {
            void run(t("apply"), async () => {
            const changed = await api.meowApplyCanSettings(info.node_id, settings);
            message.info(changed ? t("meowSettingsSaved") : t("meowSettingsResaved"));
            });
          }
        }}
      />
    </Space>
  );
}

function TelemetryCard({ snapshot }: { snapshot: MeowMotorSnapshot | null }) {
  const { t } = useI18n();
  const m = snapshot?.measurements;
  const fmt = (value: number | null | undefined, digits = 6) =>
    value == null ? "—" : value.toFixed(digits);
  return (
    <Card size="small" title={t("display")}>
      <Descriptions size="small" column={3} bordered>
        <Descriptions.Item label={t("meowRawPosition")}>{fmt(m?.position_rev, 9)} Rev</Descriptions.Item>
        <Descriptions.Item label={t("meowAccumulatedPosition")}>
          {fmt(m?.accumulated_position_rev, 9)} Rev ({m?.accumulation_valid ? "valid" : "invalid"})
        </Descriptions.Item>
        <Descriptions.Item label={t("velocity")}>{fmt(m?.velocity_rev_per_s)} Rev/s</Descriptions.Item>
        <Descriptions.Item label={t("torque")}>{m?.torque_permille ?? "—"} ‰</Descriptions.Item>
        <Descriptions.Item label={t("meowTemperatures")}>
          {fmt(m?.driver_temp_c, 1)} / {fmt(m?.motor_temp_c, 1)} ℃
        </Descriptions.Item>
        <Descriptions.Item label={t("mode")}>{m?.mode_display ?? "—"}</Descriptions.Item>
        <Descriptions.Item label={t("meowDetailedError")}>
          {m?.detailed_error == null ? "—" : `0x${m.detailed_error.toString(16).toUpperCase().padStart(4, "0")}`}
        </Descriptions.Item>
        <Descriptions.Item label={t("meowTimestamp")}>{m?.timestamp_us ?? "—"} µs</Descriptions.Item>
        <Descriptions.Item label={t("meowTpdoGenerations")}>
          {m ? `${m.tpdo1_generation} / ${m.tpdo2_generation}` : "—"}
        </Descriptions.Item>
      </Descriptions>
    </Card>
  );
}

function CanSettingsCard({
  snapshot,
  settings,
  setSettings,
  canConfigure,
  busy,
  apply,
}: {
  snapshot: MeowMotorSnapshot | null;
  settings: MeowCanSettingsRequest | null;
  setSettings: (settings: MeowCanSettingsRequest) => void;
  canConfigure: boolean;
  busy: boolean;
  apply: () => void;
}) {
  const { t } = useI18n();
  return (
    <Card size="small" title={t("meowCanSettings")}>
      {snapshot?.can_config.status === "read_failed" && (
        <Alert type="error" showIcon message={snapshot.can_config.reason} style={{ marginBottom: 12 }} />
      )}
      {settings ? (
        <Space wrap align="end">
          <Field label="Node-ID">
            <InputNumber value={settings.node_id} min={1} max={127} onChange={(value) => setSettings({ ...settings, node_id: value ?? 1 })} />
          </Field>
          <Field label={t("meowNominalBitrate")}>
            <Select value={settings.nominal_bitrate} options={NOMINAL_BITRATES.map((value) => ({ value, label: `${value / 1000} kbit/s` }))} onChange={(value) => setSettings({ ...settings, nominal_bitrate: value })} style={{ width: 140 }} />
          </Field>
          <Field label={t("meowDataBitrate")}>
            <Select value={settings.data_bitrate} options={DATA_BITRATES.map((value) => ({ value, label: `${value / 1_000_000} Mbit/s` }))} onChange={(value) => setSettings({ ...settings, data_bitrate: value })} style={{ width: 130 }} />
          </Field>
          <Field label="PDO BRS">
            <Switch checked={settings.transmit_pdo_brs} onChange={(checked) => setSettings({ ...settings, transmit_pdo_brs: checked })} />
          </Field>
          <Button type="primary" loading={busy} disabled={!canConfigure} onClick={apply}>
            {t("meowSaveAndPowerCycle")}
          </Button>
        </Space>
      ) : (
        <Typography.Text type="secondary">{t("meowWaitingSettings")}</Typography.Text>
      )}
      {!canConfigure && settings && (
        <Alert type="info" showIcon message={t("meowSettingsPreopOnly")} style={{ marginTop: 12 }} />
      )}
    </Card>
  );
}

function NumericField({
  label,
  value,
  set,
  min,
  max,
}: {
  label: string;
  value: number;
  set: (value: number) => void;
  min?: number;
  max?: number;
}) {
  return (
    <Field label={label}>
      <InputNumber value={value} min={min} max={max} step={0.1} onChange={(next) => set(next ?? 0)} />
    </Field>
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
