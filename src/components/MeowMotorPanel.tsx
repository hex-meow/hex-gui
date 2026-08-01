import { QuestionCircleOutlined } from "@ant-design/icons";
import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  Alert,
  App,
  Button,
  Card,
  Descriptions,
  InputNumber,
  Segmented,
  Select,
  Space,
  Switch,
  Tooltip,
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
} from "../types";
import type { Sample } from "../useTelemetry";
import { LiveChart } from "./LiveChart";

const NOMINAL_BITRATES = [125_000, 250_000, 500_000, 800_000, 1_000_000];
const DATA_BITRATES = [1_000_000, 2_000_000, 4_000_000, 5_000_000];
const SIGNED_Q8_24_MAX_REV = 127.999_999_940_395_36;
const CHART_BUFFER_MS = 60_000;

export function MeowMotorPanel({
  info,
  connected,
  settingsOnly = false,
  logging = false,
  logPath = null,
  onToggleLog,
  onBusyChange,
}: {
  info: MotorInfo;
  connected: boolean;
  settingsOnly?: boolean;
  logging?: boolean;
  logPath?: string | null;
  onToggleLog?: (on: boolean) => void;
  onBusyChange?: (busy: boolean) => void;
}) {
  const { message } = App.useApp();
  const { t } = useI18n();
  const [snapshot, setSnapshot] = useState<MeowMotorSnapshot | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [tpdoRateHz, setTpdoRateHz] = useState<500 | 1000>(1000);
  const [refreshHz, setRefreshHz] = useState(50);
  const [view, setView] = useState<"panel" | "chart">("panel");
  const [windowSec, setWindowSec] = useState(10);
  const positionSeeded = useRef(false);
  const lastSampleGeneration = useRef<number | null>(null);
  const samples = useRef<Sample[]>([]);
  const [chartVersion, setChartVersion] = useState(0);

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

    const measurements = next.measurements;
    if (lastSampleGeneration.current !== measurements.tpdo1_generation) {
      lastSampleGeneration.current = measurements.tpdo1_generation;
      const now = performance.now();
      const buffer = samples.current;
      buffer.push({
        t: now,
        position:
          measurements.accumulation_valid && measurements.accumulated_position_rev != null
            ? measurements.accumulated_position_rev
            : measurements.position_rev,
        velocity: measurements.velocity_rev_per_s,
        torque: measurements.torque_permille,
      });
      const cutoff = now - CHART_BUFFER_MS;
      while (buffer.length > 0 && buffer[0].t < cutoff) buffer.shift();
      setChartVersion((version) => version + 1);
    }
  };

  const refresh = async () => {
    acceptSnapshot(await api.meowGetStatus(info.node_id));
  };

  useEffect(() => {
    if (!connected) return;
    let alive = true;
    positionSeeded.current = false;
    lastSampleGeneration.current = null;
    samples.current = [];
    setChartVersion((version) => version + 1);
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
        // Identification creates an entry if the explicit manager has not yet
        // observed a heartbeat and applies the exact 0x1018 product gate.
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
    return () => {
      alive = false;
    };
  }, [connected, info.node_id]);

  useEffect(() => {
    if (!connected) return;
    let alive = true;
    const timer = window.setInterval(() => {
      void api
        .meowGetStatus(info.node_id)
        .then((next) => {
          if (alive) acceptSnapshot(next);
        })
        .catch((error) => {
          if (alive) setLoadError(errMsg(error));
        });
    }, Math.max(1, Math.round(1000 / refreshHz)));
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [connected, info.node_id, refreshHz]);

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

  const activate = (target: MeowMotorTarget) =>
    run(t("sendTarget"), () => api.meowActivateTarget(info.node_id, target));

  const activeMode = snapshot?.logic?.state === "Enabled" ? snapshot.logic.mode : null;
  const disabledActive = snapshot?.logic?.state === "Disabled";
  const canConfigure =
    snapshot?.online === true &&
    (snapshot.nmt_state === "PreOperational" || snapshot.nmt_state === "Stopped") &&
    (snapshot.lifecycle.kind === "Identified" || snapshot.lifecycle.kind === "NeedsReinit");

  return (
    <Space direction="vertical" size={12} style={{ width: "100%" }}>
      <Card size="small">
        <Space style={{ justifyContent: "space-between", width: "100%" }} align="start">
          <Space direction="vertical" size={2}>
            <Space>
              <Typography.Title level={4} style={{ margin: 0 }}>
                {snapshot?.friendly_name ?? info.friendly_name}
              </Typography.Title>
              <Typography.Text code>{nid2hex(info.node_id)}</Typography.Text>
            </Space>
            <Typography.Text type="secondary">
              {t("meowLifecycle")}: {snapshot?.lifecycle.kind ?? "Identifying"} · NMT{" "}
              {snapshot?.nmt_state ?? "—"}
            </Typography.Text>
          </Space>
          {!settingsOnly && (
            <Space size={16} wrap>
              <Space size={4}>
                <Typography.Text type="secondary">{t("refresh")}</Typography.Text>
                <Tooltip title={t("meowRefreshHint")}>
                  <Typography.Text type="secondary">
                    <QuestionCircleOutlined />
                  </Typography.Text>
                </Tooltip>
                <Segmented
                  size="small"
                  value={refreshHz}
                  onChange={(value) => setRefreshHz(value as number)}
                  options={[
                    { label: t("refreshLow"), value: 50 },
                    { label: t("refreshHigh"), value: 100 },
                  ]}
                />
              </Space>
              <Space size={4}>
                <Typography.Text type="secondary">{t("recordCsv")}</Typography.Text>
                <Switch checked={logging} onChange={onToggleLog} />
              </Space>
            </Space>
          )}
        </Space>
        {!settingsOnly && logging && logPath && (
          <Typography.Text type="secondary" style={{ fontSize: 12 }} copyable>
            {logPath}
          </Typography.Text>
        )}
      </Card>

      {loadError && (
        <Alert type="error" showIcon message={t("meowIdentifyFailed")} description={loadError} />
      )}
      {snapshot?.lifecycle.kind === "NeedsRestart" && (
        <Alert type="warning" showIcon message={t("meowPowerCycleRequired")} />
      )}

      {settingsOnly ? (
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
      ) : (
        <>
          <Card
            size="small"
            title={t("display")}
            extra={
              <Space>
                {view === "chart" && (
                  <Space size={4}>
                    <Typography.Text type="secondary">{t("window")}</Typography.Text>
                    <InputNumber
                      size="small"
                      min={1}
                      max={60}
                      value={windowSec}
                      onChange={(value) => setWindowSec(value ?? 10)}
                      addonAfter="s"
                      style={{ width: 90 }}
                    />
                  </Space>
                )}
                <Segmented
                  value={view}
                  onChange={(value) => setView(value as "panel" | "chart")}
                  options={[
                    { label: t("numeric"), value: "panel" },
                    { label: t("chart"), value: "chart" },
                  ]}
                />
              </Space>
            }
          >
            {view === "panel" ? (
              <TelemetryPanel snapshot={snapshot} />
            ) : (
              <LiveChart
                samples={samples}
                chartVersion={chartVersion}
                windowSec={windowSec}
              />
            )}
          </Card>

          <Card size="small" title={t("control")}>
            {snapshot?.logic?.state === "Error" && (
              <Alert
                type="error"
                showIcon
                style={{ marginBottom: 12 }}
                message={`${t("motorFault")}: ${snapshot.logic.kind} @ 0x${snapshot.logic.raw_code
                  .toString(16)
                  .toUpperCase()
                  .padStart(4, "0")}`}
                description={t("faultDesc")}
              />
            )}
            <Space wrap align="end" style={{ marginBottom: 12 }}>
              <Field label={t("meowTpdoRate")}>
                <Segmented
                  value={tpdoRateHz}
                  onChange={(value) => setTpdoRateHz(value as 500 | 1000)}
                  options={[500, 1000]}
                />
              </Field>
              <Button
                loading={busy}
                disabled={!snapshot?.online}
                onClick={() =>
                  run(
                    snapshot?.lifecycle.kind === "Initialized"
                      ? t("reinitialize")
                      : t("initialize"),
                    () => api.meowInitialize(info.node_id, tpdoRateHz),
                  )
                }
              >
                {snapshot?.lifecycle.kind === "Initialized" ? t("reinitialize") : t("initialize")}
              </Button>
              <Button
                loading={busy}
                disabled={snapshot == null}
                onClick={() => run(t("clearError"), () => api.meowClearError(info.node_id))}
              >
                {t("clearError")}
              </Button>
              <Typography.Text type="secondary">{t("meowModeTargetHint")}</Typography.Text>
            </Space>

            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                gap: 12,
                alignItems: "stretch",
              }}
            >
              <ModeCard title={t("disableAction")} active={disabledActive}>
                <Typography.Text type="secondary">{t("meowDisabledModeHint")}</Typography.Text>
                <Button
                  danger
                  block
                  loading={busy}
                  disabled={!snapshot?.is_ready}
                  onClick={() => run(t("disableAction"), () => api.meowDisable(info.node_id))}
                >
                  {t("disableAction")}
                </Button>
              </ModeCard>

              <ModeCard title={t("mode_ProfilePosition")} active={activeMode === "ProfilePosition"}>
                <Field label={t("meowPositionTarget")}>
                  <InputNumber
                    value={position}
                    min={-128}
                    max={SIGNED_Q8_24_MAX_REV}
                    step={0.01}
                    precision={9}
                    onChange={(value) => setPosition(value ?? 0)}
                    style={{ width: "100%" }}
                  />
                </Field>
                <Button
                  size="small"
                  disabled={snapshot?.measurements.position_rev == null}
                  onClick={() => setPosition(snapshot?.measurements.position_rev ?? position)}
                >
                  {t("meowUseCurrentPosition")}
                </Button>
                <Typography.Text type="warning" style={{ fontSize: 12 }}>
                  {t("meowPpRangeWarning")}
                </Typography.Text>
                <SendTargetButton
                  busy={busy}
                  ready={snapshot?.is_ready === true}
                  onClick={() => activate({ kind: "ProfilePosition", position_rev: position })}
                />
              </ModeCard>

              <ModeCard title={t("mode_ProfileVelocity")} active={activeMode === "ProfileVelocity"}>
                <Field label={t("meowVelocityTarget")}>
                  <InputNumber
                    value={velocity}
                    step={0.1}
                    onChange={(value) => setVelocity(value ?? 0)}
                    style={{ width: "100%" }}
                  />
                </Field>
                <SendTargetButton
                  busy={busy}
                  ready={snapshot?.is_ready === true}
                  onClick={() =>
                    activate({ kind: "ProfileVelocity", velocity_rev_per_s: velocity })
                  }
                />
              </ModeCard>

              <ModeCard title={t("mode_Torque")} active={activeMode === "Torque"}>
                <Field label={t("meowTorqueTarget")}>
                  <InputNumber
                    value={torquePermille}
                    min={-1000}
                    max={1000}
                    step={10}
                    onChange={(value) => setTorquePermille(value ?? 0)}
                    style={{ width: "100%" }}
                  />
                </Field>
                <SendTargetButton
                  busy={busy}
                  ready={snapshot?.is_ready === true}
                  onClick={() =>
                    activate({ kind: "Torque", torque_permille: Math.round(torquePermille) })
                  }
                />
              </ModeCard>

              <ModeCard title={t("mode_Mit")} active={activeMode === "Mit"}>
                <NumericField
                  label="Pdes (Rev)"
                  value={mit.position_rev}
                  set={(value) => setMit({ ...mit, position_rev: value })}
                />
                <NumericField
                  label="Vdes (Rev/s)"
                  value={mit.velocity_rev_per_s}
                  set={(value) => setMit({ ...mit, velocity_rev_per_s: value })}
                />
                <NumericField
                  label="Tff (Nm)"
                  value={mit.torque_nm}
                  set={(value) => setMit({ ...mit, torque_nm: value })}
                />
                <NumericField
                  label="Kp (u16)"
                  value={mit.kp}
                  min={0}
                  max={65535}
                  set={(value) => setMit({ ...mit, kp: Math.round(value) })}
                />
                <NumericField
                  label="Kd (u16)"
                  value={mit.kd}
                  min={0}
                  max={65535}
                  set={(value) => setMit({ ...mit, kd: Math.round(value) })}
                />
                <NumericField
                  label="Kp/Kd limit (‰)"
                  value={mit.kp_kd_limit_permille}
                  min={0}
                  max={1000}
                  set={(value) => setMit({ ...mit, kp_kd_limit_permille: Math.round(value) })}
                />
                <SendTargetButton
                  busy={busy}
                  ready={snapshot?.is_ready === true}
                  onClick={() => activate({ kind: "Mit", ...mit })}
                />
              </ModeCard>
            </div>
          </Card>

          <Card size="small" title={t("meowLimits")}>
            <Space wrap align="end">
              <Field label={t("meowMaxTorque")}>
                <InputNumber
                  value={maxTorque}
                  min={0}
                  max={1000}
                  step={10}
                  onChange={(value) => setMaxTorque(value ?? 0)}
                />
              </Field>
              <Button
                loading={busy}
                disabled={!snapshot?.is_ready}
                onClick={() =>
                  run(t("limitMaxTorque"), () =>
                    api.meowSetMaxTorque(info.node_id, Math.round(maxTorque)),
                  )
                }
              >
                {t("apply")}
              </Button>
              <NumericField
                label={t("meowProfileVelocity")}
                value={profile.velocity_rev_per_s}
                min={0.000001}
                set={(value) => setProfile({ ...profile, velocity_rev_per_s: value })}
              />
              <NumericField
                label={t("meowProfileAcceleration")}
                value={profile.acceleration_rev_per_s2}
                min={0.000001}
                set={(value) => setProfile({ ...profile, acceleration_rev_per_s2: value })}
              />
              <NumericField
                label={t("meowProfileDeceleration")}
                value={profile.deceleration_rev_per_s2}
                min={0.000001}
                set={(value) => setProfile({ ...profile, deceleration_rev_per_s2: value })}
              />
              <Button
                loading={busy}
                disabled={!snapshot?.is_ready}
                onClick={() =>
                  run(t("meowApplyProfile"), () =>
                    api.meowSetProfileLimits(info.node_id, profile),
                  )
                }
              >
                {t("meowApplyProfile")}
              </Button>
            </Space>
          </Card>
        </>
      )}
    </Space>
  );
}

function ModeCard({
  title,
  active,
  children,
}: {
  title: string;
  active: boolean;
  children: ReactNode;
}) {
  return (
    <Card
      size="small"
      title={title}
      style={{
        height: "100%",
        borderColor: active ? "#4f8cff" : undefined,
        boxShadow: active ? "0 0 18px rgba(79, 140, 255, 0.55)" : undefined,
        transition: "border-color 160ms ease, box-shadow 160ms ease",
      }}
      styles={{ body: { height: "calc(100% - 38px)" } }}
    >
      <Space direction="vertical" size={8} style={{ width: "100%", height: "100%" }}>
        {children}
      </Space>
    </Card>
  );
}

function SendTargetButton({
  busy,
  ready,
  onClick,
}: {
  busy: boolean;
  ready: boolean;
  onClick: () => void;
}) {
  const { t } = useI18n();
  return (
    <Button type="primary" block loading={busy} disabled={!ready} onClick={onClick}>
      {t("sendTarget")}
    </Button>
  );
}

function TelemetryPanel({ snapshot }: { snapshot: MeowMotorSnapshot | null }) {
  const { t } = useI18n();
  const measurements = snapshot?.measurements;
  const format = (value: number | null | undefined, digits = 6) =>
    value == null ? "—" : value.toFixed(digits);
  return (
    <Descriptions size="small" column={3} bordered>
      <Descriptions.Item label={t("meowRawPosition")}>
        {format(measurements?.position_rev, 9)} Rev
      </Descriptions.Item>
      <Descriptions.Item label={t("meowAccumulatedPosition")}>
        {format(measurements?.accumulated_position_rev, 9)} Rev ({measurements?.accumulation_valid
          ? t("meowAccumulationValid")
          : t("meowAccumulationInvalid")})
      </Descriptions.Item>
      <Descriptions.Item label={t("velocity")}>
        {format(measurements?.velocity_rev_per_s)} Rev/s
      </Descriptions.Item>
      <Descriptions.Item label={t("torque")}>
        {measurements?.torque_permille ?? "—"} ‰
      </Descriptions.Item>
      <Descriptions.Item label={t("meowTemperatures")}>
        {format(measurements?.driver_temp_c, 1)} / {format(measurements?.motor_temp_c, 1)} ℃
      </Descriptions.Item>
      <Descriptions.Item label={t("mode")}>
        {measurements?.mode_display ?? "—"}
      </Descriptions.Item>
      <Descriptions.Item label={t("meowDetailedError")}>
        {measurements?.detailed_error == null
          ? "—"
          : `0x${measurements.detailed_error.toString(16).toUpperCase().padStart(4, "0")}`}
      </Descriptions.Item>
      <Descriptions.Item label={t("meowTimestamp")}>
        {measurements?.timestamp_us ?? "—"} µs
      </Descriptions.Item>
      <Descriptions.Item label={t("meowTpdoGenerations")}>
        {measurements
          ? `${measurements.tpdo1_generation} / ${measurements.tpdo2_generation}`
          : "—"}
      </Descriptions.Item>
    </Descriptions>
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
        <Alert
          type="error"
          showIcon
          message={snapshot.can_config.reason}
          style={{ marginBottom: 12 }}
        />
      )}
      {settings ? (
        <Space wrap align="end">
          <Field label="Node-ID">
            <InputNumber
              value={settings.node_id}
              min={1}
              max={127}
              onChange={(value) => setSettings({ ...settings, node_id: value ?? 1 })}
            />
          </Field>
          <Field label={t("meowNominalBitrate")}>
            <Select
              value={settings.nominal_bitrate}
              options={NOMINAL_BITRATES.map((value) => ({
                value,
                label: `${value / 1000} kbit/s`,
              }))}
              onChange={(value) => setSettings({ ...settings, nominal_bitrate: value })}
              style={{ width: 140 }}
            />
          </Field>
          <Field label={t("meowDataBitrate")}>
            <Select
              value={settings.data_bitrate}
              options={DATA_BITRATES.map((value) => ({
                value,
                label: `${value / 1_000_000} Mbit/s`,
              }))}
              onChange={(value) => setSettings({ ...settings, data_bitrate: value })}
              style={{ width: 130 }}
            />
          </Field>
          <Field label="PDO BRS">
            <Switch
              checked={settings.transmit_pdo_brs}
              onChange={(checked) =>
                setSettings({ ...settings, transmit_pdo_brs: checked })
              }
            />
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
      <InputNumber
        value={value}
        min={min}
        max={max}
        step={0.1}
        onChange={(next) => set(next ?? 0)}
        style={{ width: "100%" }}
      />
    </Field>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div style={{ width: "100%" }}>
      <div style={{ fontSize: 12, color: "#8a93a3", marginBottom: 2 }}>{label}</div>
      {children}
    </div>
  );
}
