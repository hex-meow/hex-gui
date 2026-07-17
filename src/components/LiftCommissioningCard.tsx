import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type PointerEvent,
} from "react";
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  Checkbox,
  InputNumber,
  Space,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import {
  CHALLENGE_ARM,
  CHALLENGE_CLEAR_FAULT,
  EPOCH_EXHAUSTED,
  EPOCH_READY,
  EPOCH_WRITE_FAILED,
  isLiftCommissionAbi2,
  NMT_OPERATIONAL,
  NMT_PRE_OPERATIONAL,
  canWriteEpochService,
  challengeKindLabel,
  epochStatusLabel,
  shouldOfferEpochServiceAction,
} from "../liftCommissionProtocol";
import { useI18n } from "../i18n";
import type { LiftState } from "../types";

const OPERATOR_LEASE_RENEW_MS = 40;

const STATE_DISARMED = 0;
const STATE_ARMED_IDLE = 1;
const STATE_DRIVING = 2;
const STATE_FOLDBACK = 3;
const STATE_WAIT_RELEASE = 4;
const STATE_FAULT_LATCHED = 0x80;

const FLAG_ARMED = 1 << 0;
const FLAG_LEASE_ACTIVE = 1 << 1;
const FLAG_OUTPUT_ACTIVE = 1 << 2;
const FLAG_SLEW_LIMITED = 1 << 3;
const FLAG_CURRENT_FOLDBACK = 1 << 4;
const FLAG_WAIT_RELEASE = 1 << 5;
const FLAG_PULSE_EXPIRED = 1 << 6;
const FLAG_FAULT = 1 << 7;

type Direction = -1 | 0 | 1;

function integer(value: number): string {
  return Number.isFinite(value) ? Math.trunc(value).toLocaleString() : "—";
}

function finite(value: number, digits = 3, suffix = ""): string {
  return Number.isFinite(value) ? value.toFixed(digits) + suffix : "—";
}

function hex(value: number, width: number): string {
  if (!Number.isFinite(value)) return "—";
  return (
    "0x" +
    (Math.trunc(value) >>> 0).toString(16).toUpperCase().padStart(width, "0")
  );
}

function stateName(value: number): string {
  switch (value) {
    case STATE_DISARMED:
      return "Disarmed";
    case STATE_ARMED_IDLE:
      return "ArmedIdle";
    case STATE_DRIVING:
      return "Driving";
    case STATE_FOLDBACK:
      return "Foldback";
    case STATE_WAIT_RELEASE:
      return "WaitRelease";
    case STATE_FAULT_LATCHED:
      return "FaultLatched";
    default:
      return hex(value, 2);
  }
}

function stopReason(value: number): string {
  const reasons: Record<number, string> = {
    0: "None",
    1: "ManualDisarm",
    2: "NotOperational",
    3: "Parked",
    4: "NameplateInvalid",
    5: "EncoderUnavailable",
    6: "PowerMonitor",
    7: "InaAlert",
    8: "BusUndervoltage",
    9: "BusOvervoltage",
    10: "LeaseExpired",
    11: "PulseExpired",
    12: "Protocol",
    13: "Overcurrent",
    14: "FoldbackTimeout",
    15: "NoProgress",
    16: "HardwareBreak",
  };
  return reasons[value] ?? "Unknown(" + value + ")";
}

async function copyText(text: string): Promise<void> {
  if (!navigator.clipboard) {
    throw new Error("clipboard API is unavailable");
  }
  await navigator.clipboard.writeText(text);
}

function Metrics({ items }: { items: Array<[string, string]> }) {
  return (
    <div className="lift-metrics lift-commission__metrics">
      {items.map(([label, value]) => (
        <div className="lift-metric" key={label}>
          <span>{label}</span>
          <strong title={value}>{value}</strong>
        </div>
      ))}
    </div>
  );
}

export function LiftCommissioningCard({
  state,
  connected,
  attached,
  globalBusy,
}: {
  state: LiftState;
  connected: boolean;
  attached: boolean;
  globalBusy: boolean;
}) {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();
  const commission = state.commissioning;

  const [acknowledged, setAcknowledged] = useState(false);
  const [motorDisconnected, setMotorDisconnected] = useState(false);
  const [duty, setDuty] = useState(() =>
    Math.max(1, Math.min(50, commission.hard_cap_permille || 50))
  );
  const [busy, setBusy] = useState<string | null>(null);
  const directionRef = useRef<Direction>(0);
  const holdActiveRef = useRef(false);

  const compatible = isLiftCommissionAbi2(
    state.device_name,
    commission.abi,
    commission.available
  );
  const armed =
    commission.active_session !== 0 && (commission.flags & FLAG_ARMED) !== 0;
  const faulted =
    commission.state === STATE_FAULT_LATCHED ||
    (commission.flags & FLAG_FAULT) !== 0;
  const telemetryFresh =
    commission.tpdo3_fresh &&
    commission.tpdo4_fresh &&
    commission.pair_fresh;
  const operational = state.nmt_state === NMT_OPERATIONAL;
  const preOperational = state.nmt_state === NMT_PRE_OPERATIONAL;
  const epochReady =
    commission.epoch_status === EPOCH_READY && commission.boot_epoch !== 0;
  const epochServiceOffered = shouldOfferEpochServiceAction(
    commission.epoch_status
  );
  const epochTerminal =
    commission.epoch_status === EPOCH_EXHAUSTED ||
    commission.epoch_status === EPOCH_WRITE_FAILED;
  const fingerprintMismatch = commission.ina_fingerprint_mismatch !== 0;
  const armChallengeReady =
    commission.challenge_kind === CHALLENGE_ARM && commission.challenge !== 0;
  const clearFaultChallengeReady =
    commission.challenge_kind === CHALLENGE_CLEAR_FAULT &&
    commission.challenge !== 0;
  const waitRelease =
    commission.state === STATE_WAIT_RELEASE ||
    (commission.flags & FLAG_WAIT_RELEASE) !== 0;
  const canArm =
    compatible &&
    connected &&
    attached &&
    state.online &&
    operational &&
    telemetryFresh &&
    epochReady &&
    armChallengeReady &&
    !armed &&
    !faulted &&
    acknowledged &&
    commission.hard_cap_permille > 0 &&
    commission.lease_ms > 20 &&
    commission.max_pulse_ms > 0 &&
    !globalBusy &&
    busy === null;
  const canClearFault =
    compatible &&
    connected &&
    attached &&
    state.online &&
    operational &&
    commission.state === STATE_FAULT_LATCHED &&
    !armed &&
    clearFaultChallengeReady &&
    !globalBusy &&
    busy === null;
  const canEpochService =
    compatible &&
    connected &&
    attached &&
    state.online &&
    !globalBusy &&
    busy === null &&
    canWriteEpochService({
      nmtState: state.nmt_state,
      motorDisconnected,
      epochStatus: commission.epoch_status,
      commissionState: commission.state,
      activeSession: commission.active_session,
      flags: commission.flags,
      bootEpoch: commission.boot_epoch,
    });
  const canMaintainHold =
    compatible &&
    connected &&
    attached &&
    state.online &&
    operational &&
    telemetryFresh &&
    armed &&
    !faulted &&
    !waitRelease &&
    !globalBusy &&
    busy === null;
  const canDrive =
    canMaintainHold &&
    commission.state === STATE_ARMED_IDLE &&
    commission.expected_pulse_id !== 0 &&
    commission.gap_remaining_ms === 0 &&
    duty > 0 &&
    duty <= commission.hard_cap_permille;

  useEffect(() => {
    if (armed) return;
    setDuty((current) =>
      Math.max(
        1,
        Math.min(current, Math.max(1, commission.hard_cap_permille || 1))
      )
    );
  }, [armed, commission.hard_cap_permille]);

  useEffect(() => {
    if (!epochServiceOffered) {
      setMotorDisconnected(false);
    }
  }, [epochServiceOffered]);

  const release = useCallback(async () => {
    if (directionRef.current === 0 && !holdActiveRef.current) return;
    directionRef.current = 0;
    holdActiveRef.current = false;
    try {
      await api.liftCommissionRelease();
    } catch (error) {
      message.error(t("liftCommissionReleaseFailed") + ": " + errMsg(error));
    }
  }, [message, t]);

  useEffect(() => {
    const releaseGlobal = () => {
      void release();
    };
    const visibility = () => {
      if (document.visibilityState !== "visible") void release();
    };
    window.addEventListener("pointerup", releaseGlobal);
    window.addEventListener("pointercancel", releaseGlobal);
    window.addEventListener("blur", releaseGlobal);
    document.addEventListener("visibilitychange", visibility);
    return () => {
      window.removeEventListener("pointerup", releaseGlobal);
      window.removeEventListener("pointercancel", releaseGlobal);
      window.removeEventListener("blur", releaseGlobal);
      document.removeEventListener("visibilitychange", visibility);
      directionRef.current = 0;
      holdActiveRef.current = false;
      void api.liftCommissionRelease();
    };
  }, [release]);

  useEffect(() => {
    if (canMaintainHold) return;
    void release();
  }, [canMaintainHold, release]);

  useEffect(() => {
    let alive = true;
    let inFlight = false;
    const renew = async () => {
      if (
        !holdActiveRef.current ||
        directionRef.current === 0 ||
        inFlight
      ) {
        return;
      }
      inFlight = true;
      try {
        await api.liftCommissionRenew();
      } catch (error) {
        directionRef.current = 0;
        holdActiveRef.current = false;
        if (alive) {
          message.error(
            t("liftCommissionLeaseFailed") + ": " + errMsg(error)
          );
        }
        try {
          await api.liftCommissionRelease();
        } catch {
          // The Rust operator lease and firmware lease independently expire.
        }
      } finally {
        inFlight = false;
      }
    };
    const timer = window.setInterval(renew, OPERATOR_LEASE_RENEW_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [message, t]);

  const arm = useCallback(async () => {
    setBusy("arm");
    try {
      const session = await api.liftCommissionArm();
      message.success(t("liftCommissionArmed") + " " + hex(session, 8));
    } catch (error) {
      message.error(t("liftCommissionArmFailed") + ": " + errMsg(error));
    } finally {
      setBusy(null);
    }
  }, [message, t]);

  const clearFault = useCallback(async () => {
    if (!canClearFault) return;
    setBusy("clearFault");
    try {
      await api.liftCommissionClearFault();
      message.success(t("liftCommissionClearFaultSucceeded"));
    } catch (error) {
      message.error(
        t("liftCommissionClearFaultFailed") + ": " + errMsg(error)
      );
    } finally {
      setBusy(null);
    }
  }, [canClearFault, message, t]);

  const serviceEpoch = useCallback(async () => {
    if (!canEpochService) return;
    setBusy("epochService");
    try {
      await api.liftCommissionEpochService(motorDisconnected);
      setMotorDisconnected(false);
      message.success(t("liftCommissionEpochServiceSucceeded"));
    } catch (error) {
      message.error(
        t("liftCommissionEpochServiceFailed") + ": " + errMsg(error)
      );
    } finally {
      setBusy(null);
    }
  }, [canEpochService, message, motorDisconnected, t]);

  const disarm = useCallback(async () => {
    directionRef.current = 0;
    holdActiveRef.current = false;
    setBusy("disarm");
    try {
      await api.liftCommissionRelease();
      await api.liftCommissionDisarm();
      message.success(t("liftCommissionDisarmed"));
    } catch (error) {
      message.error(t("liftCommissionDisarmFailed") + ": " + errMsg(error));
    } finally {
      setBusy(null);
    }
  }, [message, t]);

  const estop = useCallback(async () => {
    directionRef.current = 0;
    holdActiveRef.current = false;
    setBusy("estop");
    try {
      await api.liftCommissionEstop();
      message.warning(t("liftCommissionEstopped"));
    } catch (error) {
      message.error(t("liftCommissionEstopFailed") + ": " + errMsg(error), 0);
    } finally {
      setBusy(null);
    }
  }, [message, t]);

  const start = useCallback(
    async (
      direction: Exclude<Direction, 0>,
      event: PointerEvent<HTMLElement>
    ) => {
      if (!canDrive || directionRef.current !== 0) return;
      event.preventDefault();
      event.currentTarget.setPointerCapture(event.pointerId);
      directionRef.current = direction;
      holdActiveRef.current = false;
      try {
        await api.liftCommissionHold(direction * Math.abs(duty));
        if (directionRef.current === direction) {
          holdActiveRef.current = true;
        } else {
          await api.liftCommissionRelease();
        }
      } catch (error) {
        directionRef.current = 0;
        holdActiveRef.current = false;
        message.error(t("liftCommissionHoldFailed") + ": " + errMsg(error));
        try {
          await api.liftCommissionRelease();
        } catch {
          // Firmware lease is the final stop backstop.
        }
      }
    },
    [canDrive, duty, message, t]
  );

  const copyCsv = useCallback(async () => {
    setBusy("csv");
    try {
      await copyText(await api.liftCommissionCsv());
      message.success(t("liftCommissionCsvCopied"));
    } catch (error) {
      message.error(t("liftCommissionCsvFailed") + ": " + errMsg(error));
    } finally {
      setBusy(null);
    }
  }, [message, t]);

  const flagTags: Array<[number, string]> = [
    [FLAG_ARMED, "ARMED"],
    [FLAG_LEASE_ACTIVE, "LEASE_ACTIVE"],
    [FLAG_OUTPUT_ACTIVE, "OUTPUT_ACTIVE"],
    [FLAG_SLEW_LIMITED, "SLEW_LIMITED"],
    [FLAG_CURRENT_FOLDBACK, "CURRENT_FOLDBACK"],
    [FLAG_WAIT_RELEASE, "WAIT_RELEASE"],
    [FLAG_PULSE_EXPIRED, "PULSE_EXPIRED"],
    [FLAG_FAULT, "FAULT"],
  ];
  const foldback =
    commission.state === STATE_FOLDBACK ||
    (commission.flags & FLAG_CURRENT_FOLDBACK) !== 0;
  if (!compatible) return null;


  return (
    <Card
      className={
        "lift-commission " +
        (faulted
          ? "lift-commission--fault"
          : armed
            ? "lift-commission--armed"
            : "")
      }
      title={t("liftCommissionTitle")}
      extra={
        <Space wrap>
          <Tag color="gold">ABI {commission.abi}</Tag>
          <Tag color={epochReady ? "green" : epochTerminal ? "red" : "orange"}>
            EPOCH {epochStatusLabel(commission.epoch_status)}
          </Tag>
          <Tag color={commission.challenge === 0 ? "default" : "blue"}>
            CHALLENGE {challengeKindLabel(commission.challenge_kind)}
          </Tag>
          <Tag color={faulted ? "red" : armed ? "orange" : "default"}>
            {stateName(commission.state)}
          </Tag>
          <Tag color={telemetryFresh ? "green" : "red"}>
            TPDO3/4 {telemetryFresh ? "FRESH" : "STALE"}
          </Tag>
        </Space>
      }
    >
      <div className="lift-commission__body">
        <Alert
          type={faulted ? "error" : "warning"}
          showIcon
          message={t("liftCommissionWarningTitle")}
          description={t("liftCommissionWarning")}
        />

        {epochServiceOffered && (
          <section className="lift-commission__service">
            <Alert
              type="warning"
              showIcon
              message={t("liftCommissionEpochServiceTitle")}
              description={
                t("liftCommissionEpochServiceDescription") +
                " " +
                epochStatusLabel(commission.epoch_status)
              }
            />
            <Checkbox
              checked={motorDisconnected}
              disabled={armed || busy !== null || globalBusy}
              onChange={(event) =>
                setMotorDisconnected(event.target.checked)
              }
            >
              {t("liftCommissionMotorDisconnected")}
            </Checkbox>
            <Space wrap className="lift-commission__service-actions">
              <Button
                danger
                type="primary"
                disabled={!canEpochService}
                loading={busy === "epochService"}
                onClick={() => void serviceEpoch()}
              >
                {t("liftCommissionEpochService")}
              </Button>
              <Tag color={preOperational ? "green" : "orange"}>
                {preOperational
                  ? t("liftCommissionEpochPreOpReady")
                  : t("liftCommissionEpochPreOpRequired")}
              </Tag>
            </Space>
          </section>
        )}

        {epochTerminal && (
          <Alert
            type="error"
            showIcon
            message={t("liftCommissionEpochTerminalTitle")}
            description={
              commission.epoch_status === EPOCH_EXHAUSTED
                ? t("liftCommissionEpochExhausted")
                : t("liftCommissionEpochWriteFailed")
            }
          />
        )}

        {fingerprintMismatch && (
          <Alert
            type="error"
            showIcon
            message={t("liftCommissionInaFingerprintTitle")}
            description={
              t("liftCommissionInaFingerprintDescription") +
              " " +
              hex(commission.ina_fingerprint_mismatch, 4)
            }
          />
        )}

        {faulted && (
          <section className="lift-commission__fault-actions">
            <Alert
              type="error"
              showIcon
              message={t("liftCommissionClearFaultTitle")}
              description={
                !operational
                  ? t("liftCommissionClearFaultNeedsOperational")
                  : clearFaultChallengeReady
                    ? t("liftCommissionClearFaultReady")
                    : t("liftCommissionClearFaultWaiting")
              }
            />
            <Button
              danger
              disabled={!canClearFault}
              loading={busy === "clearFault"}
              onClick={() => void clearFault()}
            >
              {t("liftCommissionClearFault")}
            </Button>
          </section>
        )}

        <div className="lift-commission__actions">
          <label className="lift-field">
            <span>{t("liftCommissionDuty")}</span>
            <InputNumber
              min={1}
              max={commission.hard_cap_permille || 1}
              precision={0}
              value={duty}
              disabled={armed || busy !== null || globalBusy}
              addonAfter="‰"
              onChange={(value) =>
                setDuty(
                  Math.max(
                    1,
                    Math.min(
                      Math.abs(value ?? 1),
                      commission.hard_cap_permille || 1
                    )
                  )
                )
              }
            />
          </label>
          <Checkbox
            checked={acknowledged}
            disabled={armed || busy !== null}
            onChange={(event) => setAcknowledged(event.target.checked)}
          >
            {t("liftCommissionAcknowledge")}
          </Checkbox>
          <Space wrap>
            <Button
              type="primary"
              danger
              disabled={!canArm}
              loading={busy === "arm"}
              onClick={() => void arm()}
            >
              {t("liftCommissionArm")}
            </Button>
            <Button
              disabled={!armed || busy !== null}
              loading={busy === "disarm"}
              onClick={() => void disarm()}
            >
              {t("liftCommissionDisarm")}
            </Button>
            {!armed && !faulted && (
              <Tag color={armChallengeReady && epochReady ? "blue" : "default"}>
                {armChallengeReady && epochReady
                  ? t("liftCommissionArmChallengeReady")
                  : t("liftCommissionArmChallengeWaiting")}
              </Tag>
            )}
          </Space>
        </div>

        <div className="lift-commission__hold">
          <Typography.Text strong>{t("liftCommissionHoldTitle")}</Typography.Text>
          <Typography.Text type="secondary">
            {t("liftCommissionHoldHint")}
          </Typography.Text>
          <div className="lift-jog-buttons">
            <Button
              className="lift-jog lift-commission__direction"
              size="large"
              disabled={!canDrive && directionRef.current === 0}
              onPointerDown={(event) => void start(-1, event)}
              onPointerUp={() => void release()}
              onPointerCancel={() => void release()}
              onPointerLeave={() => void release()}
            >
              {t("liftCommissionA")} (−)
            </Button>
            <Button
              className="lift-jog lift-commission__direction"
              size="large"
              disabled={!canDrive && directionRef.current === 0}
              onPointerDown={(event) => void start(1, event)}
              onPointerUp={() => void release()}
              onPointerCancel={() => void release()}
              onPointerLeave={() => void release()}
            >
              {t("liftCommissionB")} (+)
            </Button>
          </div>
          {!canDrive && directionRef.current === 0 && (
            <Typography.Text type="secondary" className="lift-inline-blocker">
              {faulted
                ? t("liftCommissionFaulted")
                : !epochReady
                  ? t("liftCommissionEpochNotReady")
                  : !armed
                    ? armChallengeReady
                      ? t("liftCommissionNeedArm")
                      : t("liftCommissionArmChallengeWaiting")
                    : waitRelease
                      ? t("liftCommissionWaitRelease")
                      : !telemetryFresh
                        ? t("liftCommissionTelemetryStale")
                        : commission.gap_remaining_ms > 0
                          ? t("liftCommissionGap")
                          : commission.expected_pulse_id === 0
                            ? t("liftCommissionPulseUnavailable")
                            : t("liftCommissionBlocked")}
            </Typography.Text>
          )}
        </div>

        <Button
          className="lift-commission__estop"
          danger
          type="primary"
          size="large"
          disabled={!connected || !attached}
          loading={busy === "estop"}
          onClick={() => void estop()}
        >
          {t("liftCommissionEstop")}
        </Button>

        <div className="lift-commission__live">
          <Metrics
            items={[
              [t("liftCommissionState"), stateName(commission.state)],
              [t("liftCommissionStopReason"), stopReason(commission.stop_reason)],
              [
                t("liftCommissionActiveSession"),
                hex(commission.active_session, 8),
              ],
              [t("liftCommissionBootEpoch"), integer(commission.boot_epoch)],
              [
                t("liftCommissionEpochStatus"),
                epochStatusLabel(commission.epoch_status),
              ],
              [
                t("liftCommissionChallenge"),
                hex(commission.challenge, 8) +
                  " / " +
                  challengeKindLabel(commission.challenge_kind),
              ],
              [
                t("liftCommissionExpectedPulse"),
                integer(commission.expected_pulse_id),
              ],
              [
                t("liftCommissionPulse"),
                commission.active_pulse === 0xffff
                  ? "—"
                  : integer(commission.active_pulse),
              ],
              [t("liftCommissionEncoderSign"), integer(commission.encoder_sign)],
              [
                t("liftCommissionInaFingerprintMismatch"),
                commission.ina_fingerprint_mismatch === 0
                  ? "0x0000 (OK)"
                  : hex(commission.ina_fingerprint_mismatch, 4),
              ],
              [
                t("liftCommissionCountdown"),
                integer(commission.host_remaining_ms) + " ms",
              ],
              [
                t("liftCommissionPulseElapsed"),
                integer(commission.pulse_elapsed_ms) + " ms",
              ],
              [t("liftCommissionRawCount"), integer(commission.raw_count)],
              [t("liftCommissionTick"), integer(commission.tick)],
              [t("liftCommissionCurrent"), finite(commission.current_a, 3, " A")],
              [
                t("liftCommissionDutyPair"),
                integer(commission.requested_duty_permille) +
                  " / " +
                  integer(commission.applied_duty_permille) +
                  " ‰",
              ],
              [
                t("liftCommissionFoldbackCap"),
                integer(commission.foldback_cap_permille) + " ‰",
              ],
              [
                t("liftCommissionHardCap"),
                integer(commission.hard_cap_permille) + " ‰",
              ],
              [
                t("liftCommissionCurrentLimits"),
                finite(commission.soft_current_a, 2) +
                  " / " +
                  finite(commission.hard_current_a, 2) +
                  " A",
              ],
              [
                t("liftCommissionLease"),
                integer(commission.lease_ms) + " ms",
              ],
              [
                t("liftCommissionMaxPulse"),
                integer(commission.max_pulse_ms) + " ms",
              ],
              [
                t("liftCommissionCommandAge"),
                integer(commission.command_age_ms) + " ms",
              ],
              [
                t("liftCommissionEnergized"),
                integer(commission.energized_ms) + " ms",
              ],
              [
                t("liftCommissionOvercurrent"),
                integer(commission.overcurrent_ms) + " ms",
              ],
              [
                t("liftCommissionGapRemaining"),
                integer(commission.gap_remaining_ms) + " ms",
              ],
            ]}
          />
          <div className="lift-status-bits">
            {flagTags.map(([mask, label]) => {
              const active = (commission.flags & mask) !== 0;
              const dangerous =
                mask === FLAG_FAULT ||
                mask === FLAG_OUTPUT_ACTIVE ||
                mask === FLAG_CURRENT_FOLDBACK;
              return (
                <Tag
                  key={mask}
                  color={active ? (dangerous ? "red" : "orange") : "default"}
                >
                  {active ? "● " : "○ "}
                  {label}
                </Tag>
              );
            })}
            {foldback && <Tag color="red">{t("liftCommissionFoldback")}</Tag>}
          </div>
        </div>

        <div className="lift-commission__csv">
          <Typography.Text type="secondary">
            {t("liftCommissionBuffer")}: {integer(commission.buffered_samples)} / 2000
            {" · "}
            {t("liftCommissionDropped")}: {integer(commission.dropped_pairs)}
          </Typography.Text>
          <Button
            disabled={commission.buffered_samples === 0 || busy !== null}
            loading={busy === "csv"}
            onClick={() => void copyCsv()}
          >
            {t("liftCommissionCopyCsv")}
          </Button>
        </div>
      </div>
    </Card>
  );
}

