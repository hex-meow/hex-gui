import { useCallback, useEffect, useRef, useState, type PointerEvent } from "react";
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  InputNumber,
  Space,
  Tag,
  Typography,
} from "antd";
import {
  LIFT_COMMISSION_DEVICE_NAME,
  isLiftCommissionAbi2,
} from "../liftCommissionProtocol";
import { api, errMsg } from "../api";
import { useI18n, type I18nKey } from "../i18n";
import type { LiftState } from "../types";
import { LiftCommissioningCard } from "./LiftCommissioningCard";
import "./LiftPanel.css";

const POLL_MS = 100;
const SDO_REFRESH_MS = 1000;
const VELOCITY_LEASE_RENEW_MS = 50;

// Fallback for the firmware velocity release deadband used until the node is
// read. The live value now comes from the wire (`0x4600:05` velocity_min_mps):
// a commanded |speed| below it coasts (the self-locking screw holds), so the
// jog control must never offer a lower non-zero setpoint or it would look dead.
// The upper bound likewise comes from the wire (`velocity_max_mps`).
const LIFT_MIN_JOG_MPS = 0.001;
const LIFT_DEFAULT_JOG_MPS = 0.02;

const STATUS_CONFIG_VALID = 1 << 0;
const STATUS_HOMED = 1 << 1;
const STATUS_TARGET_REACHED = 1 << 2;
const STATUS_MOVING = 1 << 3;
const STATUS_LOWER_LIMIT = 1 << 4;
const STATUS_UPPER_LIMIT = 1 << 5;
const STATUS_OUTPUT_LIMITED = 1 << 6;
const STATUS_FAULT = 1 << 7;

// v0.4 sensor_status is five bits (docs/lift-object-dictionary.md §8).
const SENSOR_ENCODER_READY = 1 << 0;
const SENSOR_INA_PRESENT = 1 << 1;
const SENSOR_INA_FRESH = 1 << 2;
const SENSOR_SAMPLE_VALID = 1 << 3;
const SENSOR_INA_ALERT = 1 << 4;

type HoldDirection = -1 | 0 | 1;

function finite(value: number, digits = 3, suffix = ""): string {
  return Number.isFinite(value) ? value.toFixed(digits) + suffix : "—";
}

function integer(value: number): string {
  return Number.isFinite(value) ? Math.trunc(value).toLocaleString() : "—";
}

function hex(value: number, width: number): string {
  if (!Number.isFinite(value)) return "—";
  return "0x" + (Math.trunc(value) >>> 0).toString(16).toUpperCase().padStart(width, "0");
}

function isOperational(nmt: number): boolean {
  return nmt === 0x05;
}

function nmtName(nmt: number): string {
  switch (nmt) {
    case 0x00:
      return "Boot-up (0x00)";
    case 0x04:
      return "Stopped (0x04)";
    case 0x05:
      return "Operational (0x05)";
    case 0x7f:
      return "Pre-operational (0x7F)";
    default:
      return hex(nmt, 2);
  }
}

function nmtColor(nmt: number): string {
  if (isOperational(nmt)) return "green";
  if (nmt === 0x04) return "red";
  if (nmt === 0x7f) return "gold";
  return "default";
}

export function LiftPanel({ connected }: { connected: boolean }) {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();

  const [nodeId, setNodeId] = useState(20);
  const [attached, setAttached] = useState(false);
  const [state, setState] = useState<LiftState | null>(null);
  const [sdoError, setSdoError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [safetyBusy, setSafetyBusy] = useState(false);
  const [positionGoal, setPositionGoal] = useState(0.1);
  const [jogSpeed, setJogSpeed] = useState(LIFT_DEFAULT_JOG_MPS);

  const attachedRef = useRef(false);
  const holdDirection = useRef<HoldDirection>(0);
  const velocityArmed = useRef(false);
  const commandInFlight = useRef(false);
  const jogSpeedRef = useRef(jogSpeed);

  useEffect(() => {
    attachedRef.current = attached;
  }, [attached]);

  useEffect(() => {
    jogSpeedRef.current = jogSpeed;
  }, [jogSpeed]);

  const updateState = useCallback((next: LiftState) => {
    setState(next);
    if (!next.running) {
      setAttached(false);
    }
  }, []);

  useEffect(() => {
    if (!attached || !connected) return;
    let alive = true;
    let inFlight = false;

    const poll = async () => {
      if (inFlight) return;
      inFlight = true;
      try {
        const next = await api.liftGetState();
        if (alive) updateState(next);
      } catch (e) {
        if (alive) {
          setState((previous) =>
            previous ? { ...previous, last_error: errMsg(e) } : previous
          );
        }
      } finally {
        inFlight = false;
      }
    };

    void poll();
    const timer = window.setInterval(poll, POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [attached, connected, updateState]);

  useEffect(() => {
    if (!attached || !connected) return;
    let alive = true;
    let inFlight = false;

    const refreshSdo = async () => {
      if (inFlight) return;
      inFlight = true;
      try {
        const next = await api.liftRefresh();
        if (alive) {
          setSdoError(null);
          updateState(next);
        }
      } catch (e) {
        if (alive) {
          setSdoError(t("liftRefreshFailed") + ": " + errMsg(e));
        }
      } finally {
        inFlight = false;
      }
    };

    const timer = window.setInterval(refreshSdo, SDO_REFRESH_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [attached, connected, t, updateState]);

  const releaseMotion = useCallback(async () => {
    if (holdDirection.current === 0 && !velocityArmed.current) return;
    holdDirection.current = 0;
    velocityArmed.current = false;
    try {
      await api.liftSetVelocity(0);
    } catch (e) {
      message.error(t("liftCommandFailed") + ": " + errMsg(e));
    }
  }, [message, t]);

  useEffect(() => {
    const release = () => {
      void releaseMotion();
    };
    window.addEventListener("pointerup", release);
    window.addEventListener("pointercancel", release);
    window.addEventListener("blur", release);
    return () => {
      window.removeEventListener("pointerup", release);
      window.removeEventListener("pointercancel", release);
      window.removeEventListener("blur", release);
    };
  }, [releaseMotion]);

  useEffect(() => {
    if (!attached || !connected) return;
    let alive = true;
    let inFlight = false;
    const renew = async () => {
      if (!velocityArmed.current || holdDirection.current === 0 || inFlight) return;
      inFlight = true;
      try {
        await api.liftRenewVelocity();
      } catch (e) {
        if (alive) {
          holdDirection.current = 0;
          velocityArmed.current = false;
          setSdoError(t("liftCommandFailed") + ": " + errMsg(e));
          try {
            await api.liftSetVelocity(0);
          } catch {
            /* backend lease expiry sends directed NMT Stop */
          }
        }
      } finally {
        inFlight = false;
      }
    };
    const timer = window.setInterval(renew, VELOCITY_LEASE_RENEW_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [attached, connected, t]);

  useEffect(() => {
    return () => {
      if (attachedRef.current) {
        holdDirection.current = 0;
        velocityArmed.current = false;
        void api.liftDisable();
        void api.liftStop();
      }
    };
  }, []);

  useEffect(() => {
    if (connected || !attached) return;
    holdDirection.current = 0;
    velocityArmed.current = false;
    setAttached(false);
    setState(null);
    setSdoError(null);
    void api.liftStop();
  }, [connected, attached]);

  const status = state?.status_word ?? 0;
  const configValid = (status & STATUS_CONFIG_VALID) !== 0;
  const homed = (status & STATUS_HOMED) !== 0;
  const faulted =
    state != null &&
    (((status & STATUS_FAULT) !== 0) || state.detailed_fault !== 0);
  const operational = state != null && isOperational(state.nmt_state);
  const sensorStatus = state?.sensor_status ?? 0;
  const encoderReady = (sensorStatus & SENSOR_ENCODER_READY) !== 0;
  const inaPresent = (sensorStatus & SENSOR_INA_PRESENT) !== 0;
  // v0.4 folds INA sample age (≤100 ms) into the INA_FRESH bit.
  const inaFresh = (sensorStatus & SENSOR_INA_FRESH) !== 0;
  const sensorSampleValid = (sensorStatus & SENSOR_SAMPLE_VALID) !== 0;
  const inaAlert = (sensorStatus & SENSOR_INA_ALERT) !== 0;
  const sensorHealthy =
    encoderReady && inaPresent && inaFresh && sensorSampleValid && !inaAlert;

  const commonBlockers: I18nKey[] = [];
  if (!connected) commonBlockers.push("liftBlockBus");
  if (!attached) commonBlockers.push("liftBlockAttach");
  if (state != null && !state.online) commonBlockers.push("liftBlockOffline");
  if (state != null && (!state.tpdo1_fresh || !state.tpdo2_fresh)) {
    commonBlockers.push("liftBlockTelemetry");
  }
  if (state != null && !sensorHealthy) commonBlockers.push("liftBlockSensors");
  if (state != null && !operational) commonBlockers.push("liftBlockNmt");
  if (state != null && !configValid) commonBlockers.push("liftBlockConfig");
  if (state != null && faulted) commonBlockers.push("liftBlockFault");

  const motionBlockers = [...commonBlockers];
  if (state != null && !homed) motionBlockers.push("liftBlockHomed");

  const commissionImage =
    state?.device_name === LIFT_COMMISSION_DEVICE_NAME ||
    state?.commissioning.available === true;
  const commissionAvailable =
    state != null &&
    isLiftCommissionAbi2(
      state.device_name,
      state.commissioning.abi,
      state.commissioning.available
    );

  const canHome =
    connected &&
    attached &&
    state != null &&
    state.online &&
    state.tpdo1_fresh &&
    state.tpdo2_fresh &&
    sensorHealthy &&
    operational &&
    configValid &&
    !commissionImage &&
    !faulted;
  const canMove = canHome && homed;

  useEffect(() => {
    if (!canMove) void releaseMotion();
  }, [canMove, releaseMotion]);

  const attach = useCallback(async () => {
    setBusy("attach");
    try {
      const next = await api.liftStart(nodeId);
      setState(next);
      setSdoError(null);
      setAttached(true);
      const min = next.position_min_m;
      const max = next.position_max_m;
      const actual = next.actual_position_m;
      if (Number.isFinite(actual) && max > min) {
        setPositionGoal(Math.min(max, Math.max(min, actual)));
      }
      message.success(t("liftAttached"));
    } catch (e) {
      message.error(t("liftAttachFailed") + ": " + errMsg(e));
    } finally {
      setBusy(null);
    }
  }, [message, nodeId, t]);

  const detach = useCallback(async () => {
    setSafetyBusy(true);
    setBusy("detach");
    holdDirection.current = 0;
    velocityArmed.current = false;
    try {
      await api.liftStop();
      setAttached(false);
      setState(null);
      setSdoError(null);
      message.info(t("liftDetached"));
    } catch (e) {
      const detail = t("liftDetachFailed") + ": " + errMsg(e);
      setSdoError(detail);
      message.error(detail);
    } finally {
      setBusy(null);
      setSafetyBusy(false);
    }
  }, [message, t]);

  const refresh = useCallback(async () => {
    setBusy("refresh");
    try {
      updateState(await api.liftRefresh());
      setSdoError(null);
    } catch (e) {
      setSdoError(t("liftRefreshFailed") + ": " + errMsg(e));
      message.error(t("liftRefreshFailed") + ": " + errMsg(e));
    } finally {
      setBusy(null);
    }
  }, [message, t, updateState]);

  const command = useCallback(
    async (name: string, action: () => Promise<void>, successKey?: I18nKey) => {
      if (commandInFlight.current) return;
      commandInFlight.current = true;
      setBusy(name);
      try {
        await action();
        if (successKey) message.success(t(successKey));
        try {
          updateState(await api.liftRefresh());
        } catch {
          /* polling will catch up */
        }
      } catch (e) {
        message.error(t("liftCommandFailed") + ": " + errMsg(e));
      } finally {
        commandInFlight.current = false;
        setBusy(null);
      }
    },
    [message, t, updateState]
  );

  const disable = useCallback(async () => {
    holdDirection.current = 0;
    velocityArmed.current = false;
    setSafetyBusy(true);
    setBusy("disable");
    try {
      await api.liftDisable();
      message.success(t("liftDisabled"));
    } catch (e) {
      const detail = t("liftDisableFailed") + ": " + errMsg(e);
      setSdoError(detail);
      message.error(detail);
    } finally {
      setBusy(null);
      setSafetyBusy(false);
    }
  }, [message, t]);

  const startJog = useCallback(
    async (direction: Exclude<HoldDirection, 0>, event: PointerEvent<HTMLElement>) => {
      if (!canMove || holdDirection.current !== 0) return;
      event.preventDefault();
      event.currentTarget.setPointerCapture(event.pointerId);
      holdDirection.current = direction;
      velocityArmed.current = false;
      try {
        await api.liftSetVelocity(direction * Math.abs(jogSpeedRef.current));
        if (holdDirection.current === direction) {
          velocityArmed.current = true;
        } else {
          await api.liftSetVelocity(0);
        }
      } catch (e) {
        holdDirection.current = 0;
        velocityArmed.current = false;
        message.error(t("liftCommandFailed") + ": " + errMsg(e));
        try {
          await api.liftSetVelocity(0);
        } catch {
          /* best effort */
        }
      }
    },
    [canMove, message, t]
  );

  const positionBoundsValid =
    state != null &&
    Number.isFinite(state.position_min_m) &&
    Number.isFinite(state.position_max_m) &&
    state.position_max_m > state.position_min_m;
  const maxJog =
    state != null && state.velocity_max_mps > 0
      ? state.velocity_max_mps
      : undefined;
  const minJog =
    state != null && state.velocity_min_mps > 0
      ? state.velocity_min_mps
      : LIFT_MIN_JOG_MPS;

  // Firmware detailed-fault codes (0x453F); see lift-driver motion::fault_code.
  const faultName = (code: number): string => {
    switch (code) {
      case 0x0000:
        return t("liftFaultNone");
      case 0x2100:
        return t("liftFaultOvercurrent");
      case 0x3210:
        return t("liftFaultOvervoltage");
      case 0x3220:
        return t("liftFaultUndervoltage");
      case 0x5000:
        return t("liftFaultPowerMonitor");
      case 0x7340:
        return t("liftFaultEncoder");
      case 0x8130:
        return t("liftFaultVelocityWatchdog");
      case 0x8500:
        return t("liftFaultPositionControl");
      case 0xff01:
        return t("liftFaultHomingTimeout");
      case 0xff03:
        return t("liftFaultConfigInvalid");
      default:
        return t("liftFaultUnknown");
    }
  };

  const modeName = (value: number): string => {
    switch (value) {
      case 0:
        return t("liftModeDisabled");
      case 1:
        return t("liftModePosition");
      case 2:
        return t("liftModeVelocity");
      case 5:
        return t("liftModeHoming");
      // Fault-latched mode-display codes (0xA1..0xAF); reuse the fault labels.
      case 0xa1:
        return t("liftFaultOvercurrent");
      case 0xa2:
        return t("liftFaultOvervoltage");
      case 0xa3:
        return t("liftFaultUndervoltage");
      case 0xa4:
        return t("liftFaultVelocityWatchdog");
      case 0xa7:
        return t("liftFaultHomingTimeout");
      case 0xa8:
        return t("liftFaultEncoder");
      case 0xaf:
        return t("liftModeConfigInvalid");
      default:
        return hex(value, 2);
    }
  };

  const statusBits: Array<[number, I18nKey]> = [
    [STATUS_CONFIG_VALID, "liftStatusConfig"],
    [STATUS_HOMED, "liftStatusHomed"],
    [STATUS_TARGET_REACHED, "liftStatusReached"],
    [STATUS_MOVING, "liftStatusMoving"],
    [STATUS_LOWER_LIMIT, "liftStatusLower"],
    [STATUS_UPPER_LIMIT, "liftStatusUpper"],
    [STATUS_OUTPUT_LIMITED, "liftStatusLimited"],
    [STATUS_FAULT, "liftStatusFault"],
  ];
  const sensorBits: Array<[number, I18nKey, boolean]> = [
    [SENSOR_ENCODER_READY, "liftEncoderReady", false],
    [SENSOR_INA_PRESENT, "liftInaPresent", false],
    [SENSOR_INA_FRESH, "liftInaFresh", false],
    [SENSOR_SAMPLE_VALID, "liftSensorSampleValid", false],
    [SENSOR_INA_ALERT, "liftInaAlert", true],
  ];

  const electricalValue = (value: string): string =>
    inaFresh ? value : value + " · " + t("liftStale");

  const visibleError = sdoError ?? state?.last_error ?? null;
  const commandBusy = busy !== null || safetyBusy;

  return (
    <div className="lift-panel">
      <Card className="lift-session app-command-card">
        <div className="lift-session__copy">
          <Typography.Text strong>{t("liftSession")}</Typography.Text>
          <Typography.Text type="secondary">{t("liftAttachHint")}</Typography.Text>
        </div>
        <div className="lift-session__controls">
          <label className="lift-field">
            <span>{t("liftNodeId")}</span>
            <InputNumber
              min={1}
              max={127}
              precision={0}
              value={nodeId}
              disabled={attached || !connected || commandBusy}
              onChange={(value) => setNodeId(value ?? 20)}
            />
          </label>
          {!attached ? (
            <Button
              type="primary"
              disabled={!connected || commandBusy}
              loading={busy === "attach"}
              onClick={attach}
            >
              {t("liftAttach")}
            </Button>
          ) : (
            <Button danger loading={busy === "detach"} onClick={detach}>
              {t("liftDetach")}
            </Button>
          )}
          <Button
            disabled={!attached || commandBusy}
            loading={busy === "refresh"}
            onClick={refresh}
          >
            {t("liftRefresh")}
          </Button>
          <Tag color={attached ? "green" : "default"}>
            {attached ? t("liftAttached") : t("liftNotAttached")}
          </Tag>
        </div>
      </Card>

      {!state ? (
        <Alert
          type={connected ? "info" : "warning"}
          showIcon
          message={connected ? t("liftAttachPrompt") : t("liftConnectPrompt")}
          description={t("liftSafeIdle")}
        />
      ) : (
        <>
          {visibleError && (
            <Alert
              className="lift-alert"
              type="error"
              showIcon
              message={t("liftBackendError")}
              description={visibleError}
            />
          )}

          {motionBlockers.length > 0 && (
            <Alert
              className="lift-alert"
              type={!configValid ? "warning" : "info"}
              showIcon
              message={
                !configValid
                  ? t("liftConfigBlockedTitle")
                  : t("liftSafetyBlockedTitle")
              }
              description={motionBlockers.map((key) => t(key)).join(" · ")}
            />
          )}

          {commissionAvailable && (
            <LiftCommissioningCard
              state={state}
              connected={connected}
              attached={attached}
              globalBusy={commandBusy}
            />
          )}

          <div className="lift-summary-grid">
            <Card title={t("liftIdentity")} size="small">
              <MetricGrid
                items={[
                  [t("liftDeviceName"), state.device_name || "—"],
                  [t("liftFirmware"), state.firmware_version || "—"],
                  [t("liftNodeId"), String(state.node_id)],
                  [
                    t("liftOnline"),
                    state.online ? t("liftYes") : t("liftNo"),
                  ],
                  [
                    t("liftTpdo1Fresh"),
                    state.tpdo1_fresh ? t("liftYes") : t("liftNo"),
                  ],
                  [
                    t("liftTpdo2Fresh"),
                    state.tpdo2_fresh ? t("liftYes") : t("liftNo"),
                  ],
                  [t("liftNmt"), nmtName(state.nmt_state)],
                  [
                    t("liftWorker"),
                    state.running ? t("liftRunning") : t("liftStopped"),
                  ],
                ]}
              />
            </Card>

            <Card title={t("liftNameplate")} size="small">
              <MetricGrid
                items={[
                  [t("liftKind"), String(state.nameplate_kind)],
                  [t("liftModel"), state.model || "—"],
                  [t("liftLayout"), hex(state.layout_id, 8)],
                  [t("liftUsed"), String(state.nameplate_used)],
                  [t("liftCrc"), hex(state.nameplate_crc32, 8)],
                  [
                    t("liftCrcValid"),
                    state.nameplate_crc_ok ? t("liftYes") : t("liftNo"),
                  ],
                ]}
              />
            </Card>

            <Card
              title={t("liftState")}
              size="small"
              extra={
                <Space size={6} wrap>
                  <Tag color={nmtColor(state.nmt_state)}>
                    {nmtName(state.nmt_state)}
                  </Tag>
                  <Tag color={faulted ? "red" : configValid ? "green" : "gold"}>
                    {faulted
                      ? state.detailed_fault !== 0
                        ? faultName(state.detailed_fault)
                        : t("liftFaulted")
                      : t("liftNoFault")}
                  </Tag>
                </Space>
              }
            >
              <MetricGrid
                items={[
                  [t("liftModeCommand"), modeName(state.mode_command)],
                  [t("liftModeDisplay"), modeName(state.mode_display)],
                  [t("liftStatusWord"), hex(state.status_word, 4)],
                  [
                    t("liftDetailedFault"),
                    state.detailed_fault === 0
                      ? t("liftFaultNone")
                      : faultName(state.detailed_fault) +
                        " (" +
                        hex(state.detailed_fault, 4) +
                        ")",
                  ],
                ]}
              />
              <div className="lift-status-bits">
                {statusBits.map(([mask, key]) => {
                  const active = (state.status_word & mask) !== 0;
                  return (
                    <Tag
                      key={mask}
                      color={active ? (mask === STATUS_FAULT ? "red" : "green") : "default"}
                    >
                      {active ? "● " : "○ "}
                      {t(key)}
                    </Tag>
                  );
                })}
              </div>
            </Card>
          </div>

          <div className="lift-main-grid">
            <Card title={t("liftControl")} className="lift-control-card">
              <section className="lift-control-section">
                <Typography.Text strong>{t("liftNmtControl")}</Typography.Text>
                <Space wrap>
                  <Button
                    disabled={!connected || !attached || !state.online || commandBusy}
                    loading={busy === "nmt-op"}
                    onClick={() =>
                      void command(
                        "nmt-op",
                        () => api.liftSetNmt("operational"),
                        "liftNmtSent"
                      )
                    }
                  >
                    {t("liftNmtOperational")}
                  </Button>
                  <Button
                    disabled={!connected || !attached || !state.online || commandBusy}
                    loading={busy === "nmt-preop"}
                    onClick={() =>
                      void command(
                        "nmt-preop",
                        () => api.liftSetNmt("pre_operational"),
                        "liftNmtSent"
                      )
                    }
                  >
                    {t("liftNmtPreop")}
                  </Button>
                  <Button
                    disabled={!connected || !attached || !state.online || commandBusy}
                    loading={busy === "nmt-stop"}
                    onClick={() =>
                      void command(
                        "nmt-stop",
                        () => api.liftSetNmt("stopped"),
                        "liftNmtSent"
                      )
                    }
                  >
                    {t("liftNmtStopped")}
                  </Button>
                </Space>
              </section>

              <Button
                className="lift-disable"
                danger
                type="primary"
                size="large"
                disabled={!connected || !attached}
                loading={busy === "disable"}
                onClick={() => void disable()}
              >
                {t("liftDisable")}
              </Button>

              <section className="lift-control-section">
                <Typography.Text strong>{t("liftRecovery")}</Typography.Text>
                <Space wrap>
                  <Button
                    type="primary"
                    disabled={!canHome || commandBusy}
                    loading={busy === "home"}
                    onClick={() =>
                      void command("home", api.liftHome, "liftHomeSent")
                    }
                  >
                    {t("liftHome")}
                  </Button>
                  <Button
                    disabled={
                      !connected ||
                      commissionImage ||
                      !attached ||
                      !state.online ||
                      !faulted ||
                      commandBusy
                    }
                    loading={busy === "clear"}
                    onClick={() =>
                      void command(
                        "clear",
                        api.liftClearFault,
                        "liftFaultCleared"
                      )
                    }
                  >
                    {t("liftClearFault")}
                  </Button>
                </Space>
                {!canHome && (
                  <Typography.Text type="secondary" className="lift-inline-blocker">
                    {commonBlockers.map((key) => t(key)).join(" · ")}
                  </Typography.Text>
                )}
              </section>

              <section className="lift-control-section">
                <Typography.Text strong>{t("liftVelocityJog")}</Typography.Text>
                <Typography.Text type="secondary">
                  {t("liftJogHint")}
                </Typography.Text>
                <label className="lift-field">
                  <span>{t("liftJogSpeed")}</span>
                  <InputNumber
                    min={minJog}
                    max={maxJog}
                    step={0.005}
                    precision={3}
                    value={jogSpeed}
                    disabled={commandBusy}
                    addonAfter="m/s"
                    onChange={(value) => {
                      const requested = Math.abs(value ?? LIFT_DEFAULT_JOG_MPS);
                      const ceiling = maxJog ?? Number.POSITIVE_INFINITY;
                      setJogSpeed(
                        Math.min(ceiling, Math.max(minJog, requested)),
                      );
                    }}
                  />
                </label>
                <div className="lift-jog-buttons">
                  <Button
                    className="lift-jog lift-jog--down"
                    size="large"
                    disabled={!canMove || commandBusy}
                    onPointerDown={(event) => void startJog(-1, event)}
                    onPointerUp={() => void releaseMotion()}
                    onPointerCancel={() => void releaseMotion()}
                    onPointerLeave={() => void releaseMotion()}
                  >
                    ↓ {t("liftJogDown")}
                  </Button>
                  <Button
                    className="lift-jog lift-jog--up"
                    size="large"
                    disabled={!canMove || commandBusy}
                    onPointerDown={(event) => void startJog(1, event)}
                    onPointerUp={() => void releaseMotion()}
                    onPointerCancel={() => void releaseMotion()}
                    onPointerLeave={() => void releaseMotion()}
                  >
                    ↑ {t("liftJogUp")}
                  </Button>
                </div>
              </section>

              <section className="lift-control-section">
                <Typography.Text strong>{t("liftPositionGoal")}</Typography.Text>
                <Alert type="warning" showIcon message={t("liftPositionWarning")} />
                <div className="lift-position-row">
                  <InputNumber
                    min={positionBoundsValid ? state.position_min_m : undefined}
                    max={positionBoundsValid ? state.position_max_m : undefined}
                    step={0.005}
                    precision={4}
                    value={positionGoal}
                    disabled={commandBusy}
                    addonAfter="m"
                    onChange={(value) => setPositionGoal(value ?? 0)}
                  />
                  <Button
                    type="primary"
                    disabled={!canMove || !positionBoundsValid || commandBusy}
                    loading={busy === "position"}
                    onClick={() =>
                      void command(
                        "position",
                        () => api.liftSetPosition(positionGoal),
                        "liftGoalSent"
                      )
                    }
                  >
                    {t("liftSendGoal")}
                  </Button>
                </div>
                <div className="lift-position-presets">
                  <Button
                    disabled={!canMove || !positionBoundsValid || commandBusy}
                    loading={busy === "position-min"}
                    onClick={() => {
                      const target = state.position_min_m;
                      setPositionGoal(target);
                      void command(
                        "position-min",
                        () => api.liftSetPosition(target),
                        "liftGoalSent"
                      );
                    }}
                  >
                    ↓ {t("liftGoToMinimum")}
                  </Button>
                  <Button
                    disabled={!canMove || !positionBoundsValid || commandBusy}
                    loading={busy === "position-max"}
                    onClick={() => {
                      const target = state.position_max_m;
                      setPositionGoal(target);
                      void command(
                        "position-max",
                        () => api.liftSetPosition(target),
                        "liftGoalSent"
                      );
                    }}
                  >
                    ↑ {t("liftGoToMaximum")}
                  </Button>
                </div>
                {!canMove && (
                  <Typography.Text type="secondary" className="lift-inline-blocker">
                    {motionBlockers.map((key) => t(key)).join(" · ")}
                  </Typography.Text>
                )}
              </section>
            </Card>

            <div className="lift-data-column">
              <Card title={t("liftTelemetry")}>
                <MetricGrid
                  large
                  items={[
                    [t("liftActualPosition"), finite(state.actual_position_m, 4, " m")],
                    [t("liftActualVelocity"), finite(state.actual_velocity_mps, 4, " m/s")],
                    [t("liftEncoder"), integer(state.encoder_count)],
                    [t("liftTimestamp"), integer(state.sample_timestamp_us) + " µs"],
                    [t("liftDuty"), finite(state.duty_command_permille, 1, " ‰")],
                  ]}
                />
              </Card>

              <Card
                title={t("liftElectrical")}
                className={sensorHealthy ? undefined : "lift-electrical-card--unhealthy"}
                extra={
                  <Space size={4} wrap>
                    <Tag color={state.tpdo2_fresh ? "green" : "red"}>
                      {t("liftTpdo2Frames")}: {t(state.tpdo2_fresh ? "liftFresh" : "liftStale")}
                    </Tag>
                    <Tag color={inaFresh ? "green" : "red"}>
                      {t("liftInaSample")}: {t(inaFresh ? "liftFresh" : "liftStale")}
                    </Tag>
                    <Tag color={sensorSampleValid ? "green" : "red"}>
                      {t("liftCombinedSample")}: {t(sensorSampleValid ? "liftValid" : "liftInvalid")}
                    </Tag>
                  </Space>
                }
              >
                <div className="lift-electrical-stack">
                  {!sensorHealthy && (
                    <Alert
                      type={inaFresh ? "warning" : "error"}
                      showIcon
                      message={t(inaFresh ? "liftSensorUnhealthyTitle" : "liftInaStaleTitle")}
                      description={t(
                        inaFresh ? "liftSensorUnhealthyDescription" : "liftInaStaleDescription"
                      )}
                    />
                  )}
                  <MetricGrid
                    items={[
                      [
                        t("liftBusVoltage"),
                        electricalValue(finite(state.bus_voltage_v, 3, " V")),
                      ],
                      [
                        t("liftBusCurrent"),
                        electricalValue(finite(state.bus_current_a, 3, " A")),
                      ],
                      [t("liftSensorStatus"), hex(state.sensor_status, 2)],
                    ]}
                  />
                  <div className="lift-status-bits">
                    {sensorBits.map(([mask, key, faultBit]) => {
                      const active = (state.sensor_status & mask) !== 0;
                      return (
                        <Tag
                          key={mask}
                          color={active ? (faultBit ? "red" : "green") : "default"}
                        >
                          {active ? "● " : "○ "}
                          {t(key)}
                        </Tag>
                      );
                    })}
                  </div>
                </div>
              </Card>

              <Card title={t("liftEffectiveParameters")}>
                <MetricGrid
                  items={[
                    [t("liftCountsPerMeter"), finite(state.counts_per_meter, 3)],
                    [
                      t("liftPositionRange"),
                      finite(state.position_min_m, 4) +
                        " … " +
                        finite(state.position_max_m, 4) +
                        " m",
                    ],
                    [t("liftVelocityMax"), finite(state.velocity_max_mps, 4, " m/s")],
                    [t("liftVelocityMin"), finite(state.velocity_min_mps, 4, " m/s")],
                  ]}
                />
                <Typography.Text type="secondary" className="lift-inline-blocker">
                  {t("liftEffectiveHint")}
                </Typography.Text>
              </Card>
            </div>
          </div>
        </>
      )}
    </div>
  );
}

function MetricGrid({
  items,
  large = false,
}: {
  items: Array<[string, string]>;
  large?: boolean;
}) {
  return (
    <div className={large ? "lift-metrics lift-metrics--large" : "lift-metrics"}>
      {items.map(([label, value]) => (
        <div className="lift-metric" key={label}>
          <span>{label}</span>
          <strong title={value}>{value}</strong>
        </div>
      ))}
    </div>
  );
}
