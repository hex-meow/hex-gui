import { useCallback, useEffect, useRef, useState } from "react";
import {
  Alert,
  App,
  Button,
  Card,
  Descriptions,
  Empty,
  InputNumber,
  Select,
  Space,
  Switch,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { nid2hex } from "../format";
import { useI18n } from "../i18n";
import type { DeviceSettingsResult, MotorInfo } from "../types";

const NOMINAL_BITRATE = 1_000_000;
const STANDARD_DATA_BITRATES = [1_000_000, 2_000_000, 4_000_000, 5_000_000];
const RESERVED_NODE_IDS = new Set([100, 101, 127]);
const READ_AFTER_SAVE_MS = 20;
const READ_ON_DISCOVERY_MS = 750;

type KnownDeviceType = Exclude<MotorInfo["device_type"], "unknown">;

interface BoundSettingsResult {
  result: DeviceSettingsResult;
  deviceType: KnownDeviceType;
  sessionEpoch: number;
  vendorId: number;
  productCode: number;
  revisionNumber: number;
  serialNumber: number;
}

interface SettingsDraft {
  newNodeId: number | null;
  dataBitrate: number | null;
  transmitPdoBrs: boolean;
  boundResult: BoundSettingsResult | null;
}

export function DeviceSettingsTool({
  device,
  devices,
  connected,
  onBusyChange,
}: {
  device: MotorInfo | null;
  devices: MotorInfo[];
  connected: boolean;
  onBusyChange: (busy: boolean) => void;
}) {
  const { message } = App.useApp();
  const { t } = useI18n();
  const [drafts, setDrafts] = useState<Record<string, SettingsDraft>>({});
  const [settingsBusy, setSettingsBusy] = useState(false);
  const [presetPosition, setPresetPosition] = useState(0);
  const [positionBusy, setPositionBusy] = useState(false);
  const [operationCount, setOperationCount] = useState(0);
  const [positions, setPositions] = useState<Record<string, number>>({});
  const operationCountRef = useRef(0);
  const autoReadAttempted = useRef<Set<string>>(new Set());
  const autoReadTimers = useRef<Map<string, number>>(new Map());
  const readTimers = useRef<Set<number>>(new Set());
  const livePositionKeys = useRef<Set<string>>(new Set());
  const mounted = useRef(true);

  const config =
    device?.can_config.status === "available"
      ? device.can_config.config
      : null;
  const currentSessionKey = device ? settingsSessionKey(device) : null;
  const draft = currentSessionKey ? drafts[currentSessionKey] ?? null : null;
  const newNodeId = draft?.newNodeId ?? null;
  const dataBitrate = draft?.dataBitrate ?? null;
  const transmitPdoBrs = draft?.transmitPdoBrs ?? false;
  const isKnown =
    device != null && device.identity != null && device.device_type !== "unknown";
  const isMotor = isKnown && device.device_type === "cia402_motor";
  const isFd = config?.data_bitrate != null;
  const operationBusy = operationCount > 0;

  const beginOperation = useCallback(() => {
    operationCountRef.current += 1;
    if (mounted.current) setOperationCount(operationCountRef.current);
    if (operationCountRef.current === 1) onBusyChange(true);
  }, [onBusyChange]);

  const endOperation = useCallback(() => {
    operationCountRef.current = Math.max(0, operationCountRef.current - 1);
    if (mounted.current) setOperationCount(operationCountRef.current);
    if (operationCountRef.current === 0) onBusyChange(false);
  }, [onBusyChange]);

  // A draft belongs to one exact online device session. Poll refreshes and
  // sidebar selection changes must not rehydrate it from an older cached
  // config, especially after a Node-ID/bitrate write that needs a restart.
  useEffect(() => {
    if (!connected) {
      setDrafts((current) =>
        Object.keys(current).length === 0 ? current : {},
      );
      return;
    }

    const onlineSessionKeys = new Set(
      devices
        .filter((candidate) => candidate.online && candidate.identity != null)
        .flatMap((candidate) => {
          const key = settingsSessionKey(candidate);
          return key == null ? [] : [key];
        }),
    );
    setDrafts((current) => {
      const entries = Object.entries(current).filter(([key]) =>
        onlineSessionKeys.has(key),
      );
      if (entries.length === Object.keys(current).length) return current;
      return Object.fromEntries(entries);
    });
  }, [connected, devices]);

  useEffect(() => {
    if (
      !connected ||
      !device?.online ||
      !currentSessionKey ||
      config == null
    ) {
      return;
    }
    setDrafts((current) => {
      if (current[currentSessionKey]) return current;
      return {
        ...current,
        [currentSessionKey]: {
          newNodeId: config.stored_node_id,
          dataBitrate: config.data_bitrate,
          transmitPdoBrs: config.transmit_pdo_brs ?? false,
          boundResult: null,
        },
      };
    });
  }, [
    connected,
    currentSessionKey,
    device?.online,
    config?.stored_node_id,
    config?.data_bitrate,
    config?.transmit_pdo_brs,
  ]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      for (const timer of readTimers.current) window.clearTimeout(timer);
      readTimers.current.clear();
      autoReadTimers.current.clear();
      livePositionKeys.current.clear();
      operationCountRef.current = 0;
      onBusyChange(false);
    };
  }, [onBusyChange]);

  const readOne = useCallback(
    async (target: MotorInfo) => {
      const identity = target.identity;
      if (!identity) throw new Error("device identity is unavailable");
      const key = positionKey(target);
      beginOperation();
      try {
        const position = await api.readPosition(
          target.node_id,
          identity.vendor_id,
          identity.product_code,
        );
        if (mounted.current && livePositionKeys.current.has(key)) {
          setPositions((current) => ({
            ...current,
            [key]: position,
          }));
        }
        return position;
      } finally {
        endOperation();
      }
    },
    [beginOperation, endOperation],
  );

  // One automatic position read per motor online session. BootUp and the
  // online→offline edge both change session_epoch in the driver, so neither a
  // stale value nor a failed-attempt marker leaks into a later session.
  useEffect(() => {
    const onlineMotorKeys = connected
      ? new Set(
          devices
            .filter(
              (candidate) =>
                candidate.device_type === "cia402_motor" &&
                candidate.online &&
                candidate.identity != null,
            )
            .map(positionKey),
        )
      : new Set<string>();
    livePositionKeys.current = onlineMotorKeys;

    if (!connected) {
      autoReadAttempted.current.clear();
      for (const timer of autoReadTimers.current.values()) {
        window.clearTimeout(timer);
        readTimers.current.delete(timer);
      }
      autoReadTimers.current.clear();
      setPositions({});
      return;
    }

    for (const key of [...autoReadAttempted.current]) {
      if (!onlineMotorKeys.has(key)) {
        autoReadAttempted.current.delete(key);
        const timer = autoReadTimers.current.get(key);
        if (timer != null) {
          window.clearTimeout(timer);
          readTimers.current.delete(timer);
          autoReadTimers.current.delete(key);
        }
        setPositions((current) => {
          if (!(key in current)) return current;
          const next = { ...current };
          delete next[key];
          return next;
        });
      }
    }

    if (
      !device ||
      device.device_type !== "cia402_motor" ||
      !device.online ||
      !device.identity
    ) {
      return;
    }
    const key = positionKey(device);
    if (autoReadAttempted.current.has(key)) return;
    autoReadAttempted.current.add(key);

    const target = device;
    const timer = window.setTimeout(() => {
      readTimers.current.delete(timer);
      autoReadTimers.current.delete(key);
      void readOne(target).catch(() => {
        // Deliberately no automatic retry during this online cycle.
      });
    }, READ_ON_DISCOVERY_MS);
    autoReadTimers.current.set(key, timer);
    readTimers.current.add(timer);
  }, [connected, device, devices, readOne]);

  if (!device) {
    return (
      <div style={{ paddingTop: 48 }}>
        <Empty description={connected ? t("settingsPickDevice") : t("connectFirst")} />
      </div>
    );
  }

  if (!device.identity) {
    return (
      <Alert
        type="info"
        showIcon
        message={t("settingsIdentityPending")}
        description={t("settingsNoOperations")}
      />
    );
  }

  if (device.device_type === "unknown") {
    return (
      <Alert
        type="warning"
        showIcon
        message={t("settingsUnknownDevice")}
        description={t("settingsNoOperations")}
      />
    );
  }

  if (device.device_type === "meow_motor") {
    return (
      <Alert
        type="info"
        showIcon
        message={device.friendly_name}
        description={t("meowCanSettings")}
      />
    );
  }

  const communicationBlocker = settingsBlocker(device, connected, t);
  const positionBlocker = positionToolBlocker(device, connected, t);
  const nodeIdValid =
    newNodeId != null &&
    Number.isInteger(newNodeId) &&
    newNodeId >= 1 &&
    newNodeId <= 127;
  const nodeIdPolicyError =
    nodeIdValid && newNodeId != null
      ? nodeIdPolicyMessage(device.device_type, newNodeId, t)
      : null;
  const nodeIdCollision =
    nodeIdValid &&
    newNodeId !== device.node_id &&
    devices.some(
      (candidate) => candidate.online && candidate.node_id === newNodeId,
    );
  const dataRateValid =
    !isFd ||
    (dataBitrate != null && STANDARD_DATA_BITRATES.includes(dataBitrate));
  const canApply =
    draft != null &&
    communicationBlocker == null &&
    nodeIdValid &&
    nodeIdPolicyError == null &&
    !nodeIdCollision &&
    dataRateValid &&
    !operationBusy;

  const updateCurrentDraft = (patch: Partial<SettingsDraft>) => {
    if (!currentSessionKey) return;
    setDrafts((current) => {
      const currentDraft = current[currentSessionKey];
      if (!currentDraft) return current;
      return {
        ...current,
        [currentSessionKey]: { ...currentDraft, ...patch },
      };
    });
  };

  const applySettings = async () => {
    if (
      !canApply ||
      newNodeId == null ||
      !currentSessionKey ||
      !draft
    ) {
      return;
    }
    const target = device;
    const targetIdentity = device.identity!;
    const targetKey = currentSessionKey;
    const targetDraft = draft;
    const targetDeviceType = device.device_type as KnownDeviceType;

    beginOperation();
    setSettingsBusy(true);
    try {
      const result = await api.applyDeviceSettings({
        node_id: target.node_id,
        expected_vendor_id: targetIdentity.vendor_id,
        expected_product_code: targetIdentity.product_code,
        new_node_id: targetDraft.newNodeId!,
        nominal_bitrate: NOMINAL_BITRATE,
        data_bitrate: isFd ? targetDraft.dataBitrate : null,
        transmit_pdo_brs: isFd ? targetDraft.transmitPdoBrs : null,
      });
      const boundResult: BoundSettingsResult = {
        result,
        deviceType: targetDeviceType,
        sessionEpoch: target.session_epoch,
        vendorId: targetIdentity.vendor_id,
        productCode: targetIdentity.product_code,
        revisionNumber: targetIdentity.revision_number,
        serialNumber: targetIdentity.serial_number,
      };
      if (mounted.current) {
        setDrafts((current) => {
          const currentDraft = current[targetKey];
          if (!currentDraft) return current;
          const previous = currentDraft.boundResult;
          const keepPendingResult =
            previous != null &&
            (previous.result.restart_required ||
              previous.result.persistence_pending) &&
            !result.changed &&
            !result.restart_required &&
            !result.persistence_pending;
          return {
            ...current,
            [targetKey]: {
              ...currentDraft,
              boundResult: keepPendingResult ? previous : boundResult,
            },
          };
        });
      }
      if (result.changed) message.success(t("settingsApplied"));
      else message.info(t("settingsNoChanges"));
    } catch (error) {
      message.error(`${t("settingsApplyFailed")}: ${errMsg(error)}`);
    } finally {
      if (mounted.current) setSettingsBusy(false);
      endOperation();
    }
  };

  const readPositionNow = async () => {
    if (!isMotor || positionBlocker != null || operationBusy) return;
    const target = device;
    setPositionBusy(true);
    try {
      await readOne(target);
    } catch (error) {
      message.error(`${t("readFailed")}: ${errMsg(error)}`);
    } finally {
      if (mounted.current) setPositionBusy(false);
    }
  };

  const savePosition = async () => {
    if (!isMotor || positionBlocker != null || operationBusy) return;
    const target = device;
    const targetIdentity = device.identity!;
    const targetPreset = presetPosition;

    beginOperation();
    setPositionBusy(true);
    try {
      await api.setPositionPreset(
        target.node_id,
        targetPreset,
        targetIdentity.vendor_id,
        targetIdentity.product_code,
      );
      message.success(
        `${t("zeroDone")} ${nid2hex(target.node_id)} → ${targetPreset.toFixed(4)} rev`,
      );

      // Exactly one delayed confirmation read. A failure is left visible as a
      // stale/empty value and is never turned into an automatic retry loop.
      await new Promise<void>((resolve) => {
        window.setTimeout(resolve, READ_AFTER_SAVE_MS);
      });
      await readOne(target).catch(() => {});
    } catch (error) {
      message.error(`${t("zeroFailed")}: ${errMsg(error)}`);
    } finally {
      if (mounted.current) setPositionBusy(false);
      endOperation();
    }
  };

  const currentPosition = positions[positionKey(device)];

  return (
    <Space direction="vertical" size={12} style={{ width: "100%", maxWidth: 720 }}>
      <Card size="small" title={t("settingsCommunicationTitle")}>
        <DeviceIdentitySummary device={device} />
        {communicationBlocker && (
          <Alert
            type="warning"
            showIcon
            message={communicationBlocker}
            style={{ marginBottom: 12 }}
          />
        )}

        <Space align="end" wrap size={12}>
          <Field label={t("settingsActiveNodeId")}>
            <Typography.Text code>{nid2hex(device.node_id)}</Typography.Text>
          </Field>
          <span style={{ paddingBottom: 5 }}>→</span>
          <Field label={t("settingsStoredTargetNodeId")}>
            <InputNumber
              min={1}
              max={127}
              value={newNodeId}
              status={!nodeIdValid ? "error" : undefined}
              disabled={communicationBlocker != null || operationBusy}
              onChange={(value) =>
                updateCurrentDraft({ newNodeId: value })
              }
              style={{ width: 110 }}
            />
          </Field>
          <Field label={t("settingsNominalBitrate")}>
            <Tag color="blue">
              {formatTimingLabel(NOMINAL_BITRATE, 0.8)}
            </Tag>
          </Field>
          {isFd && (
            <>
              <Field label={t("settingsDataBitrate")}>
                <Select
                  value={dataBitrate}
                  disabled={communicationBlocker != null || operationBusy}
                  onChange={(value) =>
                    updateCurrentDraft({ dataBitrate: value })
                  }
                  style={{ width: 190 }}
                  options={STANDARD_DATA_BITRATES.map((bitrate) => ({
                    value: bitrate,
                    label: formatTimingLabel(
                      bitrate,
                      bitrate === 5_000_000 ? 0.75 : 0.8,
                    ),
                  }))}
                />
              </Field>
              <Field label={t("settingsTransmitPdoBrs")}>
                <Switch
                  checked={transmitPdoBrs}
                  disabled={communicationBlocker != null || operationBusy}
                  checkedChildren="BRS"
                  unCheckedChildren="No BRS"
                  onChange={(checked) =>
                    updateCurrentDraft({ transmitPdoBrs: checked })
                  }
                />
              </Field>
            </>
          )}
        </Space>

        {config && !isFd && (
          <Alert
            type="info"
            showIcon
            message={t("settingsClassicOnly")}
            style={{ marginTop: 12 }}
          />
        )}
        {nodeIdCollision && (
          <Alert
            type="error"
            showIcon
            message={t("settingsNodeIdCollision")}
            style={{ marginTop: 12 }}
          />
        )}
        {!nodeIdValid && (
          <Alert
            type="error"
            showIcon
            message={t("settingsInvalidNodeId")}
            style={{ marginTop: 12 }}
          />
        )}
        {nodeIdPolicyError && (
          <Alert
            type="error"
            showIcon
            message={nodeIdPolicyError}
            style={{ marginTop: 12 }}
          />
        )}
        {!dataRateValid && (
          <Alert
            type="error"
            showIcon
            message={t("settingsInvalidDataRate")}
            style={{ marginTop: 12 }}
          />
        )}

        <Button
          type="primary"
          loading={settingsBusy}
          disabled={!canApply || operationBusy}
          onClick={applySettings}
          style={{ marginTop: 12 }}
        >
          {t("settingsApplyButton")}
        </Button>

        {draft?.boundResult && (
          <SettingsResultAlert
            boundResult={draft.boundResult}
            t={t}
          />
        )}
      </Card>

      {isMotor && (
        <Card size="small" title={t("settingsZeroTitle")}>
          <Typography.Paragraph type="secondary">
            {t("settingsZeroHint")}
          </Typography.Paragraph>
          {positionBlocker && (
            <Alert
              type="warning"
              showIcon
              message={positionBlocker}
              style={{ marginBottom: 12 }}
            />
          )}
          <Space align="end" wrap size={12}>
            <Field label={t("currentId")}>
              <Typography.Text code>{nid2hex(device.node_id)}</Typography.Text>
            </Field>
            <Button
              disabled={positionBlocker != null || operationBusy}
              loading={positionBusy}
              onClick={readPositionNow}
            >
              {t("readPos")}
            </Button>
            <Typography.Text>
              {t("currentPos")}:{" "}
              <b>
                {currentPosition == null
                  ? "—"
                  : `${currentPosition.toFixed(4)} rev`}
              </b>
            </Typography.Text>
          </Space>
          <Space align="end" wrap size={12} style={{ marginTop: 12 }}>
            <Field label={t("presetPos")}>
              <InputNumber
                min={-0.5}
                max={0.5}
                step={0.01}
                value={presetPosition}
                disabled={positionBlocker != null || operationBusy}
                onChange={(value) => setPresetPosition(value ?? 0)}
                style={{ width: 150 }}
              />
            </Field>
            <Button
              type="primary"
              disabled={positionBlocker != null || operationBusy}
              loading={positionBusy}
              onClick={savePosition}
            >
              {t("savePos")}
            </Button>
          </Space>
        </Card>
      )}
    </Space>
  );
}

function DeviceIdentitySummary({ device }: { device: MotorInfo }) {
  const { t } = useI18n();
  const identity = device.identity!;
  return (
    <Descriptions size="small" column={2} style={{ marginBottom: 12 }}>
      <Descriptions.Item label={t("settingsDeviceLabel")}>
        {device.friendly_name}
      </Descriptions.Item>
      <Descriptions.Item label={t("settingsIdentityLabel")}>
        <Typography.Text code>
          0x{identity.vendor_id.toString(16).toUpperCase().padStart(8, "0")}:
          0x{identity.product_code.toString(16).toUpperCase().padStart(8, "0")}
        </Typography.Text>
      </Descriptions.Item>
    </Descriptions>
  );
}

function SettingsResultAlert({
  boundResult,
  t,
}: {
  boundResult: BoundSettingsResult;
  t: ReturnType<typeof useI18n>["t"];
}) {
  const { result, deviceType } = boundResult;
  const details = [
    result.changed ? t("settingsApplied") : t("settingsNoChanges"),
    result.restart_required
      ? deviceType === "cia402_motor" || deviceType === "meow_motor"
        ? t("settingsMotorRestartRequired")
        : t("settingsCanRestartRequired")
      : null,
    result.persistence_pending ? t("settingsPersistencePending") : null,
    result.brs_applied_immediately ? t("settingsBrsImmediate") : null,
  ].filter((item): item is string => item != null);

  return (
    <Alert
      type={
        result.restart_required || result.persistence_pending
          ? "warning"
          : result.changed
            ? "success"
            : "info"
      }
      showIcon
      message={details[0]}
      description={
        details.length > 1 ? (
          <ul style={{ margin: "4px 0 0", paddingLeft: 20 }}>
            {details.slice(1).map((detail) => (
              <li key={detail}>{detail}</li>
            ))}
          </ul>
        ) : undefined
      }
      style={{ marginTop: 12 }}
    />
  );
}

function settingsBlocker(
  device: MotorInfo,
  connected: boolean,
  t: ReturnType<typeof useI18n>["t"],
): string | null {
  if (!connected) return t("connectFirst");
  if (!device.online) return t("settingsDeviceOffline");
  switch (device.can_config.status) {
    case "pending":
      return t("settingsConfigPending");
    case "unsupported":
      return t("settingsConfigUnsupported");
    case "read_failed":
      return `${t("settingsConfigReadFailed")}: ${device.can_config.reason}`;
    case "available":
      if (
        device.nmt_state !== "PreOperational" &&
        device.nmt_state !== "Stopped"
      ) {
        return `${t("settingsNmtRequired")} (${device.nmt_state ?? "—"})`;
      }
      return null;
  }
}

function positionToolBlocker(
  device: MotorInfo,
  connected: boolean,
  t: ReturnType<typeof useI18n>["t"],
): string | null {
  if (!connected) return t("connectFirst");
  if (!device.online) return t("settingsDeviceOffline");
  return null;
}

function nodeIdPolicyMessage(
  deviceType: MotorInfo["device_type"],
  nodeId: number,
  t: ReturnType<typeof useI18n>["t"],
): string | null {
  if (RESERVED_NODE_IDS.has(nodeId)) return t("settingsReservedNodeId");
  if (deviceType === "lift" && (nodeId < 16 || nodeId > 20)) {
    return t("settingsLiftNodeIdRange");
  }
  if (deviceType === "imu" && (nodeId < 31 || nodeId > 40)) {
    return t("settingsImuNodeIdRange");
  }
  return null;
}

function formatTimingLabel(bitrate: number, samplePoint: number): string {
  const rate =
    bitrate % 1_000_000 === 0
      ? `${bitrate / 1_000_000} Mbit/s`
      : `${bitrate} bit/s`;
  return `${rate} · SP ${samplePoint.toFixed(2)}`;
}

function positionKey(device: MotorInfo): string {
  const identity = device.identity;
  if (!identity) return `unknown:${device.node_id}:${device.session_epoch}`;
  return [
    device.session_epoch,
    device.node_id,
    identity.vendor_id,
    identity.product_code,
    identity.revision_number,
    identity.serial_number,
  ].join(":");
}

function settingsSessionKey(device: MotorInfo): string | null {
  if (!device.identity) return null;
  return `settings:${positionKey(device)}`;
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <div style={{ fontSize: 12, color: "#8a93a3", marginBottom: 4 }}>
        {label}
      </div>
      {children}
    </div>
  );
}
