import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  Col,
  Collapse,
  Empty,
  Input,
  InputNumber,
  Row,
  Select,
  Space,
  Spin,
  Statistic,
  Switch,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import { nid2hex } from "../format";
import type {
  KnobConfig,
  SmartKnobDevice,
  SmartKnobEffortUnit,
  SmartKnobProfile,
  SmartKnobTarget,
  SmartKnobTelemetry,
  SmartKnobTuning,
  UnifiedSmartKnobState,
} from "../types";

const STATE_POLL_MS = 40;
const DEVICE_POLL_MS = 500;
const EDIT_GRACE_MS = 1200;

interface TargetEditor {
  targetKey: string;
  modeIndex: number;
  customConfig: KnobConfig | null;
  tuning: SmartKnobTuning;
  telemetry: SmartKnobTelemetry;
  perModeTuning: Map<number, SmartKnobTuning>;
}

export function SmartKnobPanel({ connected }: { connected: boolean }) {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();
  const [devices, setDevices] = useState<SmartKnobDevice[]>([]);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [selectionConfirmed, setSelectionConfirmed] = useState(false);
  const [profile, setProfile] = useState<SmartKnobProfile | null>(null);
  const [profileLoading, setProfileLoading] = useState(false);
  const [editor, setEditor] = useState<TargetEditor | null>(null);
  const [running, setRunning] = useState(false);
  const [starting, setStarting] = useState(false);
  const [modeSwitching, setModeSwitching] = useState(false);
  const [state, setState] = useState<UnifiedSmartKnobState | null>(null);
  const [probeNodeId, setProbeNodeId] = useState(0xa8);
  const [probing, setProbing] = useState(false);

  const editorCache = useRef<Map<string, TargetEditor>>(new Map());
  const lastEditRef = useRef(0);
  const liveCommandSeqRef = useRef<Promise<void>>(Promise.resolve());
  const modeSwitchingRef = useRef(false);
  const onlineSetRef = useRef<string[]>([]);
  const monitorLifecycleRef = useRef<Promise<void>>(Promise.resolve());

  const onlineDevices = useMemo(() => devices.filter((device) => device.online), [devices]);
  const selectedDevice = useMemo(
    () => devices.find((device) => targetKey(device.target) === selectedKey) ?? null,
    [devices, selectedKey],
  );
  const readyProfile =
    profile && selectedKey === targetKey(profile.target) ? profile : null;
  const readyEditor = editor?.targetKey === selectedKey ? editor : null;

  const commitEditor = useCallback((next: TargetEditor) => {
    const copy = cloneEditor(next);
    editorCache.current.set(next.targetKey, copy);
    setEditor(next);
  }, []);

  // All writes to an active haptic session share one queue. In particular,
  // tuning/custom writes must never overtake a mode selection and land in the
  // previous mode's firmware-side slot.
  const enqueueLiveCommand = useCallback(<T,>(command: () => Promise<T>): Promise<T> => {
    const result = liveCommandSeqRef.current.then(command, command);
    liveCommandSeqRef.current = result.then(() => undefined, () => undefined);
    return result;
  }, []);

  // RollerCAN SmartKnob discovery emits CAN-FD+BRS probes. Keep that traffic
  // scoped to this mounted workspace so the stock Classic CAN control window
  // can share the application without its device seeing any FD frames.
  useEffect(() => {
    if (!connected) return;
    const started = monitorLifecycleRef.current.then(() => api.smartknobMonitorStart());
    monitorLifecycleRef.current = started.catch(() => undefined);
    started.catch((error) => {
      message.error(`${t("skDiscoveryFailed")}: ${errMsg(error)}`);
    });
    return () => {
      // Put stop on the same queue. The next effect setup also appends to this
      // queue, so rapid disconnect/reconnect and development remounts cannot
      // reorder a new start ahead of the preceding stop.
      const stopped = monitorLifecycleRef.current.then(() => api.smartknobMonitorStop());
      monitorLifecycleRef.current = stopped.catch(() => undefined);
    };
  }, [connected, message, t]);

  // Discover both CANopen and RollerCAN knobs through the shared bus manager.
  useEffect(() => {
    if (!connected) {
      setDevices([]);
      setSelectedKey(null);
      setSelectionConfirmed(false);
      onlineSetRef.current = [];
      setProfile(null);
      setEditor(null);
      setRunning(false);
      setState(null);
      return;
    }

    let alive = true;
    let inFlight = false;
    const tick = async () => {
      if (!alive || inFlight) return;
      inFlight = true;
      try {
        const list = await api.smartknobListDevices();
        if (!alive) return;
        setDevices(list);
        const onlineKeys = list
          .filter((device) => device.online)
          .map((device) => targetKey(device.target))
          .sort();
        const previousOnlineKeys = onlineSetRef.current;
        const onlineSetChanged =
          previousOnlineKeys.length !== onlineKeys.length
          || previousOnlineKeys.some((key, index) => key !== onlineKeys[index]);
        onlineSetRef.current = onlineKeys;
        if (onlineKeys.length === 0) {
          setSelectionConfirmed(false);
        } else if (onlineKeys.length === 1) {
          // A unique target is safe to auto-select and implicitly confirm.
          setSelectionConfirmed(true);
        } else if (onlineSetChanged) {
          // Preserve a still-valid selection, but require an explicit user
          // confirmation whenever the set becomes ambiguous or changes.
          setSelectionConfirmed(false);
        }
        setSelectedKey((current) => {
          if (running && current) return current;
          if (current && onlineKeys.includes(current)) return current;
          return onlineKeys.length === 1 ? onlineKeys[0] : null;
        });
      } catch {
        // Discovery is best-effort; a later poll normally recovers.
      } finally {
        inFlight = false;
      }
    };

    void tick();
    const handle = window.setInterval(tick, DEVICE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(handle);
    };
  }, [connected, running]);

  // Profiles are protocol-specific. Keep an independent editor/tuning cache
  // per protocol-qualified target so switching devices never leaks settings.
  useEffect(() => {
    if (!connected || !selectedDevice?.online) {
      setProfile(null);
      setEditor(null);
      return;
    }

    let alive = true;
    const key = targetKey(selectedDevice.target);
    setProfileLoading(true);
    setProfile(null);
    setEditor(null);
    api.smartknobGetProfile(selectedDevice.target)
      .then((nextProfile) => {
        if (!alive) return;
        const cached = editorCache.current.get(key);
        const nextEditor = cached
          ? cloneEditor(cached)
          : createEditor(key, nextProfile);
        editorCache.current.set(key, cloneEditor(nextEditor));
        setProfile(nextProfile);
        setEditor(nextEditor);
      })
      .catch((error) => {
        if (alive) message.error(`${t("skProfileFailed")}: ${errMsg(error)}`);
      })
      .finally(() => {
        if (alive) setProfileLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [connected, selectedDevice?.online, selectedKey, message, t]);

  // The host loop and firmware loop both expose one unit-aware state shape.
  useEffect(() => {
    if (!running) return;
    let alive = true;
    let inFlight = false;
    const tick = async () => {
      if (!alive || inFlight) return;
      inFlight = true;
      try {
        const nextState = await api.smartknobGetState();
        if (!alive) return;
        setState(nextState);
        if (!nextState.running) setRunning(false);

        if (performance.now() - lastEditRef.current > EDIT_GRACE_MS) {
          setEditor((current) => {
            if (!current) return current;
            const nextTuning: SmartKnobTuning = {
              ...current.tuning,
              pGain: nextState.p_gain,
              dGain: nextState.d_gain,
              strengthScale: nextState.strength_scale,
              effortLimit: nextState.effortLimit,
              maxOutputPermille: nextState.maxOutputPermille,
              frictionCompensation: nextState.friction_compensation,
              clickEffort: nextState.click_torque_nm,
            };
            const nextTelemetry: SmartKnobTelemetry = {
              enabled: nextState.telemetryEnabled ?? current.telemetry.enabled,
              rateHz: nextState.telemetryRateHz ?? current.telemetry.rateHz,
            };
            const next = { ...current, tuning: nextTuning, telemetry: nextTelemetry };
            editorCache.current.set(current.targetKey, cloneEditor(next));
            return next;
          });
        }
      } catch {
        // A transient state read must not stop a running haptic session.
      } finally {
        inFlight = false;
      }
    };

    void tick();
    const handle = window.setInterval(tick, STATE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(handle);
    };
  }, [running]);

  const start = useCallback(async () => {
    if (
      !selectedDevice?.online
      || !readyProfile
      || !readyEditor
      || (onlineDevices.length > 1 && !selectionConfirmed)
    ) return;
    const config = readyProfile.configs[readyEditor.modeIndex];
    if (!config) return;

    setStarting(true);
    try {
      const request = {
        target: selectedDevice.target,
        configIndex: readyEditor.modeIndex,
        tuning: readyEditor.tuning,
        ...(readyEditor.modeIndex === 0 && readyEditor.customConfig
          ? { customConfig: configWithTuning(readyEditor.customConfig, readyEditor.tuning) }
          : {}),
        ...(readyProfile.supportsTelemetry ? { telemetry: readyEditor.telemetry } : {}),
      };
      await api.smartknobStart(request);
      setState(null);
      setRunning(true);
      message.success(t("skRunning"));
    } catch (error) {
      // A RollerCAN start can fail after firmware accepted enable while the
      // bus then disappears and rollback also fails. The backend deliberately
      // retains that session so Stop can be retried; reflect it instead of
      // hiding the safety control behind a generic start error.
      const failure = errMsg(error);
      const uncertainState = await api.smartknobGetState().catch(() => null);
      setState(uncertainState);
      setRunning(Boolean(uncertainState?.running) || failure.includes("may still be active"));
      message.error(`${t("skStartFailed")}: ${failure}`);
    } finally {
      setStarting(false);
    }
  }, [selectedDevice, readyProfile, readyEditor, onlineDevices.length, selectionConfirmed, message, t]);

  const stop = useCallback(async () => {
    try {
      await api.smartknobStop();
      setRunning(false);
      setModeSwitching(false);
      setState(null);
    } catch (error) {
      // The backend retains a RollerCAN active marker when zero/disable
      // fails, so keep Stop visible and let the user retry safely.
      message.error(`${t("skStopFailed")}: ${errMsg(error)}`);
    }
  }, [message, t]);

  const clearError = useCallback(async () => {
    try {
      await api.smartknobClearError();
      message.success(t("skCleared"));
    } catch (error) {
      message.error(errMsg(error));
    }
  }, [message, t]);

  const probeRollerCan = useCallback(async () => {
    setProbing(true);
    try {
      const found = await api.smartknobProbe(probeNodeId);
      setDevices((current) => upsertDevice(current, found));
      if (found.online) {
        setSelectedKey(targetKey(found.target));
        setSelectionConfirmed(true);
      }
      message.success(`${t("skProbeFound")}: ${formatDevice(found)}`);
    } catch (error) {
      message.error(`${t("skProbeFailed")}: ${errMsg(error)}`);
    } finally {
      setProbing(false);
    }
  }, [probeNodeId, message, t]);

  const syncEditorFromState = useCallback((nextState: UnifiedSmartKnobState, fallback: TargetEditor) => {
    if (!readyProfile) return;
    const index = nextState.config_index;
    const preset = readyProfile.configs[index];
    if (!preset) {
      commitEditor(fallback);
      return;
    }
    const tuning: SmartKnobTuning = {
      pGain: nextState.p_gain,
      dGain: nextState.d_gain,
      strengthScale: nextState.strength_scale,
      effortLimit: nextState.effortLimit,
      maxOutputPermille: nextState.maxOutputPermille,
      frictionCompensation: nextState.friction_compensation,
      clickEffort: nextState.click_torque_nm,
    };
    const perModeTuning = new Map(fallback.perModeTuning);
    perModeTuning.set(index, tuning);
    const customConfig = index === 0 && nextState.config
      ? { ...nextState.config, detent_positions: [...nextState.config.detent_positions] }
      : fallback.customConfig;
    commitEditor({
      ...fallback,
      modeIndex: index,
      tuning,
      customConfig,
      perModeTuning,
    });
    setState(nextState);
  }, [readyProfile, commitEditor]);

  const pickMode = useCallback(async (index: number) => {
    if (modeSwitchingRef.current || !readyProfile || !readyEditor || !readyProfile.configs[index]) return;
    const previous = cloneEditor(readyEditor);
    lastEditRef.current = 0;
    const saved = readyEditor.perModeTuning.get(index);
    const nextTuning = saved ?? tuningForConfig(readyProfile.configs[index], readyProfile);
    const next = {
      ...readyEditor,
      modeIndex: index,
      tuning: nextTuning,
    };
    commitEditor(next);

    if (running) {
      const custom = index === 0 && next.customConfig
        ? configWithTuning(next.customConfig, nextTuning)
        : null;
      modeSwitchingRef.current = true;
      setModeSwitching(true);
      try {
        await enqueueLiveCommand(async () => {
          await api.smartknobSetConfig(index);
          if (custom) await api.smartknobSetCustomConfig(custom);
          if (saved) await api.smartknobSetTuning(saved);
        });
      } catch (error) {
        message.error(`${t("skModeChangeFailed")}: ${errMsg(error)}`);
        try {
          const nextState = await api.smartknobGetState();
          syncEditorFromState(nextState, previous);
        } catch {
          commitEditor(previous);
        }
      } finally {
        modeSwitchingRef.current = false;
        setModeSwitching(false);
      }
    }
  }, [modeSwitching, readyProfile, readyEditor, running, commitEditor, enqueueLiveCommand, message, t, syncEditorFromState]);

  const applyTuning = useCallback((patch: Partial<SmartKnobTuning>) => {
    if (!readyEditor || modeSwitchingRef.current) return;
    lastEditRef.current = performance.now();
    const tuning = { ...readyEditor.tuning, ...patch };
    const perModeTuning = new Map(readyEditor.perModeTuning);
    perModeTuning.set(readyEditor.modeIndex, tuning);
    const customConfig = readyEditor.modeIndex === 0 && readyEditor.customConfig
      ? configWithTuning(readyEditor.customConfig, tuning)
      : readyEditor.customConfig;
    commitEditor({ ...readyEditor, tuning, customConfig, perModeTuning });
    if (running) {
      void enqueueLiveCommand(() => api.smartknobSetTuning(tuning)).catch((error) => {
        message.error(errMsg(error));
      });
    }
  }, [readyEditor, modeSwitching, running, commitEditor, enqueueLiveCommand, message]);

  const applyCustomConfig = useCallback((updates: Partial<KnobConfig>) => {
    if (!readyEditor?.customConfig || modeSwitchingRef.current) return;
    lastEditRef.current = performance.now();
    let tuning = readyEditor.tuning;
    let customConfig = configWithTuning(
      { ...readyEditor.customConfig, ...updates },
      tuning,
    );
    let perModeTuning = readyEditor.perModeTuning;

    if (updates.detent_strength_unit !== undefined) {
      tuning = {
        ...tuning,
        pGain: recommendedPGain(customConfig),
        dGain: recommendedDGain(customConfig),
      };
      customConfig = configWithTuning(customConfig, tuning);
      perModeTuning = new Map(perModeTuning);
      perModeTuning.set(0, tuning);
    }

    commitEditor({ ...readyEditor, tuning, customConfig, perModeTuning });
    if (running && readyEditor.modeIndex === 0) {
      void enqueueLiveCommand(async () => {
        if (updates.detent_strength_unit !== undefined) {
          await api.smartknobSetTuning(tuning);
        }
        await api.smartknobSetCustomConfig(customConfig);
      }).catch((error) => {
        message.error(errMsg(error));
      });
    }
  }, [readyEditor, modeSwitching, running, commitEditor, enqueueLiveCommand, message]);

  const applyRecommendedGains = useCallback(() => {
    if (!readyProfile || !readyEditor) return;
    const config = readyEditor.modeIndex === 0
      ? readyEditor.customConfig
      : readyProfile.configs[readyEditor.modeIndex];
    if (!config) return;
    applyTuning({ pGain: recommendedPGain(config), dGain: recommendedDGain(config) });
  }, [readyProfile, readyEditor, applyTuning]);

  const applyTelemetry = useCallback((patch: Partial<SmartKnobTelemetry>) => {
    if (!readyEditor || modeSwitchingRef.current) return;
    const telemetry = {
      ...readyEditor.telemetry,
      ...patch,
      rateHz: clamp(Math.round(patch.rateHz ?? readyEditor.telemetry.rateHz), 1, 100),
    };
    commitEditor({ ...readyEditor, telemetry });
    if (running) {
      enqueueLiveCommand(() => api.smartknobSetTelemetry(telemetry)).catch((error) => {
        message.error(`${t("skTelemetryFailed")}: ${errMsg(error)}`);
      });
    }
  }, [readyEditor, modeSwitching, running, commitEditor, enqueueLiveCommand, message, t]);

  if (!connected) {
    return (
      <div style={{ paddingTop: 80 }}>
        <Empty description={t("skConnectFirst")} />
      </div>
    );
  }

  const activeIndex = running ? state?.config_index ?? readyEditor?.modeIndex ?? 0 : readyEditor?.modeIndex ?? 0;
  const activeConfig = activeIndex === 0 && readyEditor?.customConfig
    ? readyEditor.customConfig
    : state?.config ?? readyProfile?.configs[activeIndex] ?? null;
  const unit = effortUnitLabel(readyProfile?.effortUnit ?? state?.effortUnit ?? "Nm");
  const canStart = Boolean(
    selectedDevice?.online
      && readyProfile
      && readyEditor
      && readyProfile.configs.length > 0
      && (onlineDevices.length <= 1 || selectionConfirmed),
  );

  return (
    <Space direction="vertical" size={16} style={{ width: "100%" }}>
      <Card>
        <Space direction="vertical" size={12} style={{ width: "100%" }}>
          <Space wrap>
            {!running ? (
              <>
                <Typography.Text type="secondary">{t("skMotor")}:</Typography.Text>
                <Select
                  style={{ minWidth: 300 }}
                  placeholder={onlineDevices.length > 1 ? t("skChooseDevice") : t("skNoMotors")}
                  value={selectedKey ?? undefined}
                  disabled={starting}
                  onChange={(key) => {
                    setSelectedKey(key);
                    setSelectionConfirmed(true);
                  }}
                  options={devices.map((device) => ({
                    value: targetKey(device.target),
                    label: formatDevice(device),
                    disabled: !device.online,
                  }))}
                />
                {onlineDevices.length > 1 && selectedKey && !selectionConfirmed && (
                  <Button onClick={() => setSelectionConfirmed(true)}>
                    {t("skConfirmDevice")}
                  </Button>
                )}
                <Button type="primary" loading={starting} disabled={!canStart} onClick={start}>
                  {starting ? t("skStarting") : t("skStart")}
                </Button>
              </>
            ) : (
              <>
                <Button danger onClick={stop}>{t("skStop")}</Button>
                <Button onClick={clearError}>{t("skClearError")}</Button>
              </>
            )}
            <Tag color={running ? "green" : "default"}>
              {running ? t("skRunning") : t("skStopped")}
            </Tag>
            {readyProfile && (
              <Tag color={readyProfile.controlSide === "host" ? "blue" : "purple"}>
                {readyProfile.controlSide === "host" ? t("skHostControlled") : t("skFirmwareControlled")}
              </Tag>
            )}
            {state?.error && <Tag color="red">{state.error}</Tag>}
          </Space>

          {readyProfile?.supportsTelemetry && readyEditor && (
            <Space wrap>
              <Typography.Text type="secondary">{t("skFirmwareTelemetry")}</Typography.Text>
              <Switch
                checked={readyEditor.telemetry.enabled}
                disabled={modeSwitching}
                onChange={(enabled) => applyTelemetry({ enabled })}
              />
              <InputNumber
                addonAfter="Hz"
                min={1}
                max={100}
                value={readyEditor.telemetry.rateHz}
                disabled={modeSwitching || !readyEditor.telemetry.enabled}
                onChange={(rateHz) => applyTelemetry({ rateHz: rateHz ?? 50 })}
                style={{ width: 120 }}
              />
            </Space>
          )}

          {onlineDevices.length > 1 && (
            <Alert
              type="info"
              showIcon
              message={selectedKey == null
                ? t("skChooseDevice")
                : selectionConfirmed
                  ? t("skMultipleDevices")
                  : t("skConfirmDevice")}
            />
          )}

          <Collapse
            ghost
            size="small"
            items={[{
              key: "probe",
              label: t("skAdvancedProbe"),
              children: (
                <Space wrap>
                  <InputNumber
                    addonBefore={t("skRollerNode")}
                    min={0}
                    max={255}
                    value={probeNodeId}
                    disabled={running}
                    onChange={(value) => setProbeNodeId(value ?? 0xa8)}
                  />
                  <Tag>{nid2hex(probeNodeId)}</Tag>
                  <Button loading={probing} disabled={running} onClick={probeRollerCan}>
                    {t("skProbe")}
                  </Button>
                </Space>
              ),
            }]}
          />
        </Space>
      </Card>

      {profileLoading ? (
        <div style={{ display: "grid", placeItems: "center", minHeight: 320 }}><Spin /></div>
      ) : !readyProfile || !readyEditor ? (
        <div style={{ paddingTop: 48 }}>
          <Empty description={onlineDevices.length === 0 ? t("skNoCompatible") : t("skChooseDevice")} />
        </div>
      ) : (
        <Row gutter={16}>
          <Col xs={24} lg={11}>
            <Card>
              <Dial config={activeConfig} state={state} />
            </Card>

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
                        disabled={modeSwitching || activeIndex !== 0}
                        value={activeConfig?.text ?? ""}
                        onChange={(event) => applyCustomConfig({ text: event.target.value })}
                        placeholder={t("skCustomName")}
                      />
                    </Labeled>
                  </Col>
                </Row>
                <Row gutter={8}>
                  <Col span={12}>
                    <Labeled label={t("skLedHue")}>
                      <InputNumber
                        disabled={modeSwitching || activeIndex !== 0}
                        min={0}
                        max={255}
                        step={1}
                        value={activeConfig?.led_hue ?? 120}
                        onChange={(value) => applyCustomConfig({ led_hue: value ?? 120 })}
                        style={{ width: "100%" }}
                      />
                    </Labeled>
                  </Col>
                  <Col span={12}>
                    <Labeled label={t("skSnapPoint")}>
                      <InputNumber
                        disabled={modeSwitching || activeIndex !== 0}
                        min={0.5}
                        max={1.1}
                        step={0.01}
                        value={activeConfig?.snap_point ?? 0.55}
                        onChange={(value) => applyCustomConfig({ snap_point: value ?? 0.55 })}
                        style={{ width: "100%" }}
                      />
                    </Labeled>
                  </Col>
                </Row>
                <Row gutter={8}>
                  <Col span={8}>
                    <Labeled label={t("skMinPos")}>
                      <InputNumber
                        disabled={modeSwitching || activeIndex !== 0}
                        value={activeConfig?.min_position ?? 0}
                        onChange={(value) => applyCustomConfig({ min_position: value ?? 0 })}
                        style={{ width: "100%" }}
                      />
                    </Labeled>
                  </Col>
                  <Col span={8}>
                    <Labeled label={t("skMaxPos")}>
                      <InputNumber
                        disabled={modeSwitching || activeIndex !== 0}
                        value={activeConfig?.max_position ?? -1}
                        onChange={(value) => applyCustomConfig({ max_position: value ?? -1 })}
                        style={{ width: "100%" }}
                      />
                    </Labeled>
                  </Col>
                  <Col span={8}>
                    <Labeled label={t("skPosWidth")}>
                      <InputNumber
                        disabled={modeSwitching || activeIndex !== 0}
                        min={0.5}
                        step={1}
                        value={Math.round(radToDeg(activeConfig?.position_width_radians ?? 0.1745) * 10) / 10}
                        onChange={(value) => applyCustomConfig({ position_width_radians: degToRad(value ?? 10) })}
                        style={{ width: "100%" }}
                      />
                    </Labeled>
                  </Col>
                </Row>
                <Row gutter={8}>
                  <Col span={8}>
                    <Labeled label={t("skDetentStrength")}>
                      <InputNumber
                        disabled={modeSwitching || activeIndex !== 0}
                        min={0}
                        step={0.1}
                        value={activeConfig?.detent_strength_unit ?? 0}
                        onChange={(value) => applyCustomConfig({ detent_strength_unit: value ?? 0 })}
                        style={{ width: "100%" }}
                      />
                    </Labeled>
                  </Col>
                  <Col span={8}>
                    <Labeled label={t("skEndstopStrength")}>
                      <InputNumber
                        disabled={modeSwitching || activeIndex !== 0}
                        min={0}
                        step={0.1}
                        value={activeConfig?.endstop_strength_unit ?? 1}
                        onChange={(value) => applyCustomConfig({ endstop_strength_unit: value ?? 1 })}
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
                {readyProfile.configs.map((config, index) => (
                  <Col xs={12} sm={8} key={index}>
                    <ModeButton
                      config={config}
                      active={index === activeIndex}
                      disabled={modeSwitching}
                      onClick={() => pickMode(index)}
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
                    disabled={modeSwitching}
                    min={0}
                    step={0.1}
                    value={readyEditor.tuning.pGain}
                    onChange={(value) => applyTuning({ pGain: value ?? 0 })}
                  />
                </Labeled>
                <Labeled label={t("skDGain")}>
                  <InputNumber
                    disabled={modeSwitching}
                    min={0}
                    step={0.001}
                    value={readyEditor.tuning.dGain}
                    onChange={(value) => applyTuning({ dGain: value ?? 0 })}
                  />
                </Labeled>
                <Button disabled={modeSwitching} onClick={applyRecommendedGains}>{t("skRecommendedGains")}</Button>
                <Labeled label={`${t("skStrength")} (${unit}/unit)`}>
                  <InputNumber
                    disabled={modeSwitching}
                    min={0}
                    step={readyProfile.effortUnit === "A" ? 0.005 : 0.01}
                    value={readyEditor.tuning.strengthScale}
                    onChange={(value) => applyTuning({ strengthScale: value ?? 0 })}
                  />
                </Labeled>
                <Labeled label={`${t("skFrictionComp")} (${unit})`}>
                  <InputNumber
                    disabled={modeSwitching}
                    min={0}
                    max={0.5}
                    step={0.005}
                    value={readyEditor.tuning.frictionCompensation}
                    onChange={(value) => applyTuning({ frictionCompensation: value ?? 0 })}
                  />
                </Labeled>
                <Labeled label={`${t("skClickTorque")} (${unit})`}>
                  <InputNumber
                    disabled={modeSwitching}
                    min={0}
                    max={readyProfile.effortUnit === "A" ? 0.8 : 2}
                    step={readyProfile.effortUnit === "A" ? 0.005 : 0.01}
                    value={readyEditor.tuning.clickEffort}
                    onChange={(value) => applyTuning({ clickEffort: value ?? 0 })}
                  />
                </Labeled>
              </Space>
            </Card>

            <Card title={t("skTuningSafety")} size="small" style={{ marginTop: 16 }}>
              <Space wrap align="end">
                <Labeled label={`${t("skTorqueLimit")} (${unit})`}>
                  <InputNumber
                    disabled={modeSwitching}
                    min={0}
                    max={readyProfile.effortLimitMax}
                    step={readyProfile.effortUnit === "A" ? 0.05 : 0.1}
                    value={readyEditor.tuning.effortLimit}
                    onChange={(value) => applyTuning({ effortLimit: value ?? 0 })}
                  />
                </Labeled>
                <Labeled label={`${t("skMaxTorque")} (‰)`}>
                  <InputNumber
                    disabled={modeSwitching}
                    min={0}
                    max={1000}
                    step={50}
                    value={readyEditor.tuning.maxOutputPermille}
                    onChange={(value) => applyTuning({ maxOutputPermille: value ?? 0 })}
                  />
                </Labeled>
              </Space>
            </Card>

            {running && (
              <Card title={`${t("skTorque")} (${unit})`} size="small" style={{ marginTop: 16 }}>
                <Row gutter={8}>
                  <Col span={8}>
                    <Statistic title={`${t("skAngle")} (°)`} value={fmt(degOf(state?.shaft_angle_rad), 1)} />
                  </Col>
                  <Col span={8}>
                    <Statistic title={`${t("skCommanded")} (${unit})`} value={fmt(state?.appliedEffort)} />
                  </Col>
                  <Col span={8}>
                    <Statistic title={`${t("skMeasured")} (${unit})`} value={fmt(state?.measuredEffort)} />
                  </Col>
                </Row>
                <Row gutter={8} style={{ marginTop: 8 }}>
                  <Col span={8}>
                    <Statistic
                      title={t("skMotor")}
                      value={state?.online ? (state.enabled ? "on" : "idle") : "off"}
                    />
                  </Col>
                  {readyProfile.supportsTemperature && (
                    <>
                      <Col span={8}>
                        <Statistic title={t("driverTemp")} value={fmt(state?.driver_temp_c, 1)} />
                      </Col>
                      <Col span={8}>
                        <Statistic title={t("motorTemp")} value={fmt(state?.motor_temp_c, 1)} />
                      </Col>
                    </>
                  )}
                </Row>
              </Card>
            )}
          </Col>
        </Row>
      )}
    </Space>
  );
}

const SIZE = 340;
const CENTER = SIZE / 2;
const RADIUS = 150;
const GAUGE_SPAN = 300;

function Dial({ config, state }: { config: KnobConfig | null; state: UnifiedSmartKnobState | null }) {
  const { t } = useI18n();
  const hue = config ? (config.led_hue / 255) * 360 : 210;
  const accent = `hsl(${hue}, 70%, 58%)`;
  const dim = `hsl(${hue}, 30%, 32%)`;
  const count = state?.num_positions ?? (config ? positionCount(config) : 0);
  const position = state?.current_position ?? config?.position ?? 0;
  const subPosition = state?.sub_position_unit ?? 0;
  const minPosition = state?.min_position ?? config?.min_position ?? 0;
  const maxPosition = state?.max_position ?? config?.max_position ?? 0;
  const atEndstop = state?.at_endstop ?? false;
  const value = position + clamp(subPosition, -0.5, 0.5);
  const gauge = count >= 2 && count <= 49;
  const ticks: JSX.Element[] = [];
  let needleDeg = 0;

  if (gauge) {
    const start = 90 + (360 - GAUGE_SPAN) / 2;
    const fraction = count > 1 ? (maxPosition - value) / (count - 1) : 0;
    needleDeg = start + clamp(fraction, 0, 1) * GAUGE_SPAN;
    for (let index = 0; index < count; index += 1) {
      const degrees = start + ((count - 1 - index) / (count - 1)) * GAUGE_SPAN;
      const active = index === position - minPosition;
      ticks.push(<Tick key={index} degrees={degrees} color={active ? accent : dim} long={active} />);
    }
  } else {
    needleDeg = degOf(state?.shaft_angle_rad ?? 0) - 90;
    const width = config?.position_width_radians ?? Math.PI / 18;
    const tickCount = Math.min(72, Math.max(12, Math.round((2 * Math.PI) / width)));
    const baseDeg = needleDeg + (subPosition * width * 180) / Math.PI;
    const stepDeg = Math.max(360 / tickCount, (width * 180) / Math.PI);
    for (let index = -Math.ceil(180 / stepDeg); index <= Math.ceil(180 / stepDeg); index += 1) {
      const degrees = baseDeg + index * stepDeg;
      ticks.push(<Tick key={index} degrees={degrees} color={index === 0 ? accent : dim} long={index === 0} />);
    }
  }

  const effort = state?.appliedEffort ?? 0;
  const limit = state?.effortLimit || 1;
  const effortFraction = clamp(Math.abs(effort) / limit, 0, 1);

  return (
    <div style={{ display: "flex", flexDirection: "column", alignItems: "center" }}>
      <svg viewBox={`0 0 ${SIZE} ${SIZE}`} style={{ width: "100%", maxWidth: SIZE, aspectRatio: "1 / 1" }}>
        <circle cx={CENTER} cy={CENTER} r={RADIUS} fill="none" stroke="#222831" strokeWidth={2} />
        {ticks}
        <line
          x1={CENTER}
          y1={CENTER}
          {...lineEnd(needleDeg, RADIUS - 18)}
          stroke={atEndstop ? "#ff4d4f" : accent}
          strokeWidth={4}
          strokeLinecap="round"
        />
        <circle cx={CENTER} cy={CENTER} r={8} fill={atEndstop ? "#ff4d4f" : accent} />
        <circle
          cx={CENTER}
          cy={CENTER}
          r={RADIUS + 10}
          fill="none"
          stroke={effort >= 0 ? accent : "#ff7875"}
          strokeWidth={4}
          strokeOpacity={0.7}
          strokeDasharray={`${effortFraction * 2 * Math.PI * (RADIUS + 10)} ${2 * Math.PI * (RADIUS + 10)}`}
          transform={`rotate(-90 ${CENTER} ${CENTER})`}
          strokeLinecap="round"
        />
      </svg>
      <div style={{ textAlign: "center", marginTop: 4 }}>
        <Typography.Title level={1} style={{ margin: 0, lineHeight: 1, color: accent }}>
          {state?.running ? position : "—"}
        </Typography.Title>
        <Typography.Text type="secondary">
          {config ? `${t("skValue")} ${value.toFixed(2)}` : ""}
          {atEndstop ? ` · ${t("skEndstop")}` : count === 0 ? ` · ${t("skUnbounded")}` : ""}
        </Typography.Text>
        <div style={{ marginTop: 6, whiteSpace: "pre-line", fontWeight: 500 }}>
          {config?.text ?? ""}
        </div>
      </div>
    </div>
  );
}

function Tick({ degrees, color, long }: { degrees: number; color: string; long: boolean }) {
  const inner = long ? RADIUS - 22 : RADIUS - 12;
  const outerPoint = lineEnd(degrees, RADIUS - 2);
  const innerPoint = lineEnd(degrees, inner);
  return (
    <line
      x1={innerPoint.x2}
      y1={innerPoint.y2}
      x2={outerPoint.x2}
      y2={outerPoint.y2}
      stroke={color}
      strokeWidth={long ? 4 : 2}
      strokeLinecap="round"
    />
  );
}

function ModeButton({
  config,
  active,
  disabled,
  onClick,
}: {
  config: KnobConfig;
  active: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  const hue = (config.led_hue / 255) * 360;
  return (
    <Button
      block
      disabled={disabled}
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
      {config.text}
    </Button>
  );
}

function Labeled({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <div><Typography.Text type="secondary" style={{ fontSize: 12 }}>{label}</Typography.Text></div>
      {children}
    </div>
  );
}

function targetKey(target: SmartKnobTarget): string {
  return `${target.kind}:${target.nodeId}`;
}

function formatDevice(device: SmartKnobDevice): string {
  const protocol = device.target.kind === "canopen" ? "CANopen" : "RollerCAN";
  const offline = device.online ? "" : " · offline";
  return `[${protocol}] ${nid2hex(device.target.nodeId)} · ${device.name}${offline}`;
}

function upsertDevice(devices: SmartKnobDevice[], found: SmartKnobDevice): SmartKnobDevice[] {
  const key = targetKey(found.target);
  const index = devices.findIndex((device) => targetKey(device.target) === key);
  if (index < 0) return [...devices, found];
  const next = [...devices];
  next[index] = found;
  return next;
}

function effortUnitLabel(unit: SmartKnobEffortUnit): string {
  return unit === "Nm" ? "N·m" : "A";
}

function createEditor(key: string, profile: SmartKnobProfile): TargetEditor {
  const first = profile.configs[0] ?? null;
  return {
    targetKey: key,
    modeIndex: 0,
    customConfig: first ? { ...first, detent_positions: [...first.detent_positions] } : null,
    tuning: tuningForConfig(first, profile),
    telemetry: {
      enabled: profile.telemetryEnabled ?? true,
      rateHz: profile.telemetryRateHz ?? 50,
    },
    perModeTuning: new Map(),
  };
}

function cloneEditor(editor: TargetEditor): TargetEditor {
  return {
    ...editor,
    customConfig: editor.customConfig
      ? { ...editor.customConfig, detent_positions: [...editor.customConfig.detent_positions] }
      : null,
    tuning: { ...editor.tuning },
    telemetry: { ...editor.telemetry },
    perModeTuning: new Map(
      [...editor.perModeTuning].map(([index, tuning]) => [index, { ...tuning }]),
    ),
  };
}

function tuningForConfig(config: KnobConfig | null, profile: SmartKnobProfile): SmartKnobTuning {
  return {
    pGain: config?.p_gain ?? 0,
    dGain: config?.d_gain ?? 0,
    strengthScale: config?.strength_scale ?? (profile.effortUnit === "A" ? 0.04 : 0.15),
    effortLimit: Math.min(profile.effortLimitMax, profile.effortUnit === "A" ? 0.45 : 2),
    // Keep the established motor-side safety default even if an older backend
    // advertises only the protocol's absolute 1000-permille capability.
    maxOutputPermille: Math.min(profile.maxOutputPermille, 700),
    frictionCompensation: config?.friction_compensation ?? 0,
    clickEffort: config?.click_torque_nm ?? 0,
  };
}

function configWithTuning(config: KnobConfig, tuning: SmartKnobTuning): KnobConfig {
  return {
    ...config,
    detent_positions: [...config.detent_positions],
    strength_scale: tuning.strengthScale,
    friction_compensation: tuning.frictionCompensation,
    click_torque_nm: tuning.clickEffort,
    p_gain: tuning.pGain,
    d_gain: tuning.dGain,
  };
}

const DEG = Math.PI / 180;
const CLICK_WIDTH_THRESHOLD_RAD = 3 * DEG;

function recommendedPGain(config: KnobConfig): number {
  return config.detent_strength_unit * 4;
}

function recommendedDGain(config: KnobConfig): number {
  if (config.detent_positions.length > 0) return 0;
  if (config.click_torque_nm > 0 || config.position_width_radians < CLICK_WIDTH_THRESHOLD_RAD) return 0;
  const lower = config.detent_strength_unit * 0.08;
  const upper = config.detent_strength_unit * 0.02;
  const widthLower = 3 * DEG;
  const widthUpper = 8 * DEG;
  const raw = lower + ((upper - lower) / (widthUpper - widthLower)) * (config.position_width_radians - widthLower);
  return clamp(raw, Math.min(lower, upper), Math.max(lower, upper));
}

function lineEnd(degrees: number, radius: number): { x2: number; y2: number } {
  const radians = (degrees * Math.PI) / 180;
  return {
    x2: CENTER + radius * Math.cos(radians),
    y2: CENTER + radius * Math.sin(radians),
  };
}

function positionCount(config: KnobConfig): number {
  return config.max_position >= config.min_position
    ? config.max_position - config.min_position + 1
    : 0;
}

function degOf(radians: number | null | undefined): number {
  return radians == null ? 0 : (radians * 180) / Math.PI;
}

function radToDeg(radians: number): number {
  return (radians * 180) / Math.PI;
}

function degToRad(degrees: number): number {
  return (degrees * Math.PI) / 180;
}

function clamp(value: number, minimum: number, maximum: number): number {
  return Math.max(minimum, Math.min(maximum, value));
}

function fmt(value: number | null | undefined, digits = 3): string {
  return value == null || Number.isNaN(value) ? "—" : value.toFixed(digits);
}
