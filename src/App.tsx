import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Alert, App as AntdApp, Button, Empty, Layout, Tooltip, Typography } from "antd";
import { listen } from "@tauri-apps/api/event";
import { api, errMsg } from "./api";
import { useI18n } from "./i18n";
import { ConnectBar } from "./components/ConnectBar";
import { Sidebar } from "./components/Sidebar";
import { MotorDetail } from "./components/MotorDetail";
import { MeowMotorPanel } from "./components/MeowMotorPanel";
import { ImuPanel } from "./components/ImuPanel";
import { DeviceSettingsTool } from "./components/DeviceSettingsTool";
import { MotorFrictionCalibrationPanel } from "./components/MotorFrictionCalibrationPanel";
import { MotorTorqueCalibrationPanel } from "./components/MotorTorqueCalibrationPanel";
import { Hopea3Panel } from "./components/Hopea3Panel";
import { LiftPanel } from "./components/LiftPanel";
import { SmartKnobPanel } from "./components/SmartKnobPanel";
import RobotConsole from "./components/RobotConsole";
import { ZenohPanel } from "./components/ZenohPanel";
import { ArmPanel } from "./components/ArmPanel";
import { ControllerConfigPanel } from "./components/ControllerConfigPanel";
import { CanAnalyzerPanel } from "./components/CanAnalyzerPanel";
import { DfuPanel } from "./components/DfuPanel";
import { TutorialModal, TUTORIALS } from "./components/Tutorial";
import { canDfuApi, hpmDfuApi } from "./dfuApi";
import type { MotorInfo } from "./types";
import "./App.css";

type Tool =
  | "control"
  | "settings"
  | "hopea3"
  | "lift"
  | "smartknob"
  | "zenoh"
  | "arm"
  | "config"
  | "canalyzer"
  | "dfu"
  | "calibration"
  | "torqueCalibration"
  | "console";

const DEVICE_POLL_MS = 700;

export default function App() {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();
  // null = landing page (pick a tool before anything else).
  const [tool, setTool] = useState<Tool | null>(null);
  const [connected, setConnected] = useState(false);
  const [devices, setDevices] = useState<MotorInfo[]>([]);
  const [selectedNid, setSelectedNid] = useState<number | null>(null);
  // nid -> csv path (presence means logging is on for that motor)
  const [logging, setLogging] = useState<Record<number, string>>({});
  // Per-app "how to use this tool" modal (its slides live in TUTORIALS[tool]).
  const [tutorialOpen, setTutorialOpen] = useState(false);
  // The DFU backend also enforces this lock. The frontend copy disables Back
  // immediately so a user cannot accidentally unmount a running job.
  const [dfuBusy, setDfuBusy] = useState(false);
  // Device Settings reports every config/preset/read transaction, including
  // the one-shot automatic position reads. Keep navigation and disconnect
  // locked until the backend command has actually returned.
  const [settingsBusy, setSettingsBusy] = useState(false);
  const [calibrationBusy, setCalibrationBusy] = useState(false);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen("device-settings-close-blocked", () => {
      message.warning(t("settingsBusyClose"));
    })
      .then((dispose) => {
        if (disposed) dispose();
        else unlisten = dispose;
      })
      .catch(() => {
        // Browser-only development and an app teardown may lack an event bus.
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [message, t]);

  // Poll the device list while connected.
  useEffect(() => {
    if (!connected) {
      setDevices([]);
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const list = await api.listDevices();
        if (alive) setDevices(list);
      } catch {
        /* ignore transient */
      }
    };
    tick();
    const h = window.setInterval(tick, DEVICE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(h);
    };
  }, [connected]);

  // Auto-select the first discovered device in workspaces that use the
  // sidebar. Device Settings still applies exact identity/config gates.
  useEffect(() => {
    if (
      (tool === "control" || tool === "settings") &&
      selectedNid == null &&
      devices.length > 0
    ) {
      setSelectedNid(devices[0].node_id);
    }
  }, [devices, selectedNid, tool]);

  const onConnChange = useCallback((c: boolean) => {
    setConnected(c);
    if (!c) {
      setSelectedNid(null);
      setLogging({});
    }
  }, []);

  const switchTool = useCallback(async () => {
    if (tool === "dfu" && dfuBusy) {
      message.warning(t("dfuBusyLeave"));
      return;
    }
    if (tool === "settings" && settingsBusy) {
      message.warning(t("settingsBusyLeave"));
      return;
    }
    if ((tool === "calibration" || tool === "torqueCalibration") && calibrationBusy) {
      message.warning(t("calibrationBusyLeave"));
      return;
    }
    try {
      if (tool === "dfu") {
        await Promise.all([hpmDfuApi.leave(), canDfuApi.leave()]);
      } else {
        await api.disconnect();
      }
    } catch (e) {
      message.error(`${t("disconnectFailed")}: ${errMsg(e)}`);
      return;
    }
    setConnected(false);
    setSelectedNid(null);
    setLogging({});
    setDevices([]);
    setTutorialOpen(false);
    setTool(null);
  }, [calibrationBusy, dfuBusy, message, settingsBusy, t, tool]);

  const onToggleLog = useCallback(
    async (nid: number, on: boolean, meowMotor = false) => {
      try {
        if (on) {
          const path = meowMotor
            ? await api.meowStartLog(nid)
            : await api.startLog(nid);
          setLogging((m) => ({ ...m, [nid]: path }));
          message.success(t("startedLog"));
        } else {
          await api.stopLog(nid);
          setLogging((m) => {
            const next = { ...m };
            delete next[nid];
            return next;
          });
          message.info(t("stoppedLog"));
        }
      } catch (e) {
        message.error(`${t("logFailed")}: ${errMsg(e)}`);
      }
    },
    [message, t]
  );

  if (tool == null) return <ToolPicker onPick={setTool} />;

  const selected = devices.find((d) => d.node_id === selectedNid) ?? null;
  const toolMeta = {
    control: { title: t("toolControl"), desc: t("toolControlDesc") },
    settings: { title: t("toolSettings"), desc: t("toolSettingsDesc") },
    hopea3: { title: t("toolHopeA3"), desc: t("toolHopeA3Desc") },
    lift: { title: t("toolLift"), desc: t("toolLiftDesc") },
    smartknob: { title: t("toolSmartKnob"), desc: t("toolSmartKnobDesc") },
    zenoh: { title: t("toolBaseZenoh"), desc: t("toolBaseZenohDesc") },
    arm: { title: t("toolArmZenoh"), desc: t("toolArmZenohDesc") },
    config: { title: t("toolConfig"), desc: t("toolConfigDesc") },
    canalyzer: { title: t("toolCanalyzer"), desc: t("toolCanalyzerDesc") },
    dfu: { title: t("toolDfu"), desc: t("toolDfuDesc") },
    calibration: { title: t("toolCalibration"), desc: t("toolCalibrationDesc") },
    torqueCalibration: {
      title: t("toolTorqueCalibration"),
      desc: t("toolTorqueCalibrationDesc"),
    },
    console: { title: t("toolConsole"), desc: t("toolConsoleDesc") },
  } satisfies Record<Tool, { title: string; desc: string }>;
  const { title: toolTitle, desc: toolDesc } = toolMeta[tool];
  const needsHeartbeat =
    tool === "control" ||
    tool === "hopea3" ||
    tool === "smartknob";
  // hopea3 / smartknob / zenoh / arm / canalyzer 都是整屏面板;zenoh/arm 走 Zenoh,
  // canalyzer 自带总线连接,都不使用顶栏的电机 ConnectBar。
  const showSidebar =
    tool !== "console" &&
    tool !== "hopea3" &&
    tool !== "lift" &&
    tool !== "smartknob" &&
    tool !== "zenoh" &&
    tool !== "arm" &&
    tool !== "config" &&
    tool !== "canalyzer" &&
    tool !== "dfu" &&
    tool !== "calibration" &&
    tool !== "torqueCalibration";
  const showConnectBar =
    tool !== "console" &&
    tool !== "zenoh" &&
    tool !== "arm" &&
    tool !== "config" &&
    tool !== "canalyzer" &&
    tool !== "dfu";

  return (
    <Layout className={`app-shell app-shell--${tool}`}>
      <div className="app-chrome">
        <header className="app-chrome__header">
          <Button
            className="app-chrome__back"
            size="small"
            disabled={
              (tool === "dfu" && dfuBusy) ||
              (tool === "settings" && settingsBusy) ||
              ((tool === "calibration" || tool === "torqueCalibration") && calibrationBusy)
            }
            onClick={switchTool}
          >
            ← {t("backToTools")}
          </Button>
          <div className="app-chrome__identity">
            <Typography.Title level={2} className="app-chrome__title">
              {toolTitle}
            </Typography.Title>
            <Typography.Text type="secondary" className="app-chrome__description">
              {toolDesc}
            </Typography.Text>
          </div>
          <Button
            className="app-chrome__tutorial"
            size="small"
            onClick={() => setTutorialOpen(true)}
          >
            {t("tutorialButton")}
          </Button>
        </header>
        {showConnectBar && (
          <section className="app-command-dock" aria-label={t("connectionDock")}>
            <div className="app-command-dock__label">{t("connectionDock")}</div>
            <ConnectBar
              connected={connected}
              onChange={onConnChange}
              broadcastHeartbeat={needsHeartbeat}
              devices={devices}
              disconnectDisabled={
                (tool === "settings" && settingsBusy) ||
                ((tool === "calibration" || tool === "torqueCalibration") && calibrationBusy)
              }
            />
          </section>
        )}
      </div>
      <Layout className="app-main">
        {showSidebar && (
          <Layout.Sider width={288} theme="dark" className="app-sidebar">
            <Sidebar
              devices={devices}
              selectedNid={selectedNid}
              onSelect={setSelectedNid}
              connected={connected}
              tool={tool as "control" | "settings"}
              disabled={tool === "settings" && settingsBusy}
            />
          </Layout.Sider>
        )}
        <Layout.Content className="app-content">
          {tool === "console" ? (
            <RobotConsole />
          ) : tool === "hopea3" ? (
            <Hopea3Panel connected={connected} />
          ) : tool === "lift" ? (
            <LiftPanel connected={connected} />
          ) : tool === "smartknob" ? (
            <SmartKnobPanel
              connected={connected}
              devices={devices.filter(
                (device) => device.device_type === "cia402_motor",
              )}
            />
          ) : tool === "zenoh" ? (
            <ZenohPanel />
          ) : tool === "arm" ? (
            <ArmPanel />
          ) : tool === "config" ? (
            <ControllerConfigPanel />
          ) : tool === "canalyzer" ? (
            <CanAnalyzerPanel />
          ) : tool === "dfu" ? (
            <DfuPanel onBusyChange={setDfuBusy} />
          ) : tool === "calibration" ? (
            <MotorFrictionCalibrationPanel
              connected={connected}
              devices={devices}
              onRunningChange={setCalibrationBusy}
            />
          ) : tool === "torqueCalibration" ? (
            <MotorTorqueCalibrationPanel
              connected={connected}
              devices={devices}
              onRunningChange={setCalibrationBusy}
            />
          ) : tool === "settings" ? (
            selected?.device_type === "meow_motor" ? (
              <MeowMotorPanel
                key={selected.node_id}
                info={selected}
                connected={connected}
                settingsOnly
                onBusyChange={setSettingsBusy}
              />
            ) : (
              <DeviceSettingsTool
                device={selected}
                devices={devices}
                connected={connected}
                onBusyChange={setSettingsBusy}
              />
            )
          ) : selected && selected.device_type === "imu" ? (
            <ImuPanel key={selected.node_id} info={selected} connected={connected} />
          ) : selected && selected.device_type === "lift" ? (
            <div style={{ padding: 24 }}>
              <Alert type="info" showIcon message={t("canUseLiftTool")} />
            </div>
          ) : selected && selected.device_type === "unknown" ? (
            <div style={{ padding: 24 }}>
              <Alert type="warning" showIcon message={t("canUnknownDevice")} />
            </div>
          ) : selected && selected.device_type === "meow_motor" ? (
            <MeowMotorPanel
              key={selected.node_id}
              info={selected}
              connected={connected}
              logging={logging[selected.node_id] != null}
              logPath={logging[selected.node_id] ?? null}
              onToggleLog={(on) => onToggleLog(selected.node_id, on, true)}
              onBusyChange={setSettingsBusy}
            />
          ) : selected && selected.device_type === "cia402_motor" ? (
            <MotorDetail
              key={selected.node_id}
              info={selected}
              connected={connected}
              logging={logging[selected.node_id] != null}
              logPath={logging[selected.node_id] ?? null}
              onToggleLog={(on) => onToggleLog(selected.node_id, on)}
            />
          ) : (
            <div style={{ paddingTop: 80 }}>
              <Empty description={connected ? t("selectMotor") : t("connectFirst")} />
            </div>
          )}
        </Layout.Content>
      </Layout>
      <TutorialModal
        open={tutorialOpen}
        onClose={() => setTutorialOpen(false)}
        title={`${toolTitle} · ${t("tutorialButton")}`}
        slides={TUTORIALS[tool]}
      />
    </Layout>
  );
}

function ToolPicker({ onPick }: { onPick: (t: Tool) => void }) {
  const { message } = AntdApp.useApp();
  const { t, lang, toggle } = useI18n();
  const [developerMode, setDeveloperMode] = useState(() => {
    try {
      return window.localStorage.getItem("hex-motor-gui.developer-tools") === "enabled";
    } catch {
      return false;
    }
  });
  const titleTap = useRef({ count: 0, lastAt: 0 });

  const unlockDeveloperTools = () => {
    if (developerMode) return;
    const now = Date.now();
    if (now - titleTap.current.lastAt > 1_500) titleTap.current.count = 0;
    titleTap.current.lastAt = now;
    titleTap.current.count += 1;
    const remaining = 7 - titleTap.current.count;
    if (remaining <= 0) {
      try {
        window.localStorage.setItem("hex-motor-gui.developer-tools", "enabled");
      } catch {
        // The mode remains enabled for this WebView session.
      }
      setDeveloperMode(true);
      message.success(t("developerModeEnabled"));
      return;
    }
    if (remaining <= 3) {
      message.info(t("developerModeRemaining").replace("{count}", String(remaining)));
    }
  };
  return (
    <div className="tool-picker">
      <div className="tool-picker__actions">
        <Tooltip title={lang === "en" ? "切换到中文" : "Switch to English"}>
          <Button className="tool-picker__language" onClick={toggle}>
            {lang === "en" ? "A中" : "En"}
          </Button>
        </Tooltip>
      </div>

      <div className="tool-picker__inner">
        <header className="tool-picker__hero">
          <p className="tool-picker__eyebrow">{t("toolPickerEyebrow")}</p>
          <h1
            onClick={unlockDeveloperTools}
            style={{ cursor: developerMode ? "default" : "pointer", userSelect: "none" }}
          >
            {t("toolPickerTitle")}
          </h1>
          <p>{t("toolPickerLead")}</p>
        </header>

        <ToolSection title={t("catMotorControl")} hint={t("catMotorControlHint")}>
          <ToolCard
            title={t("toolControl")}
            desc={t("toolControlDesc")}
            tag={t("tagLiveControl")}
            accent="blue"
            onClick={() => onPick("control")}
          />
        </ToolSection>

        <ToolSection title={t("catRawCanApp")} hint={t("catRawCanAppHint")}>
          <ToolCard
            title={t("toolSmartKnob")}
            desc={t("toolSmartKnobDesc")}
            tag={t("tagHaptics")}
            accent="lime"
            onClick={() => onPick("smartknob")}
          />
          <ToolCard
            title={t("toolHopeA3")}
            desc={t("toolHopeA3Desc")}
            tag={t("tagMobileBase")}
            accent="orange"
            onClick={() => onPick("hopea3")}
          />
          <ToolCard
            title={t("toolLift")}
            desc={t("toolLiftDesc")}
            tag={t("tagLift")}
            accent="green"
            onClick={() => onPick("lift")}
          />
        </ToolSection>

        <ToolSection title={t("catRobotApp")} hint={t("catRobotAppHint")}>
          {/* TODO: Robot Console is unfinished; keep it hidden from the launcher. */}
          <ToolCard
            title={t("toolBaseZenoh")}
            desc={t("toolBaseZenohDesc")}
            tag={t("tagRobotApi")}
            accent="purple"
            onClick={() => onPick("zenoh")}
          />
          <ToolCard
            title={t("toolArmZenoh")}
            desc={t("toolArmZenohDesc")}
            tag={t("tagManipulator")}
            accent="pink"
            onClick={() => onPick("arm")}
          />
          <ToolCard
            title={t("toolConfig")}
            desc={t("toolConfigDesc")}
            tag={t("tagConfig")}
            accent="slate"
            onClick={() => onPick("config")}
          />
        </ToolSection>

        <ToolSection title={t("catTools")} hint={t("catToolsHint")}>
          {developerMode && (
            <>
              <ToolCard
                title={t("toolCalibration")}
                desc={t("toolCalibrationDesc")}
                tag={t("tagCalibration")}
                accent="pink"
                onClick={() => onPick("calibration")}
              />
              <ToolCard
                title={t("toolTorqueCalibration")}
                desc={t("toolTorqueCalibrationDesc")}
                tag={t("tagCalibration")}
                accent="orange"
                onClick={() => onPick("torqueCalibration")}
              />
            </>
          )}
          <ToolCard
            title={t("toolSettings")}
            desc={t("toolSettingsDesc")}
            tag={t("tagFactorySetup")}
            accent="amber"
            onClick={() => onPick("settings")}
          />
          <ToolCard
            title={t("toolCanalyzer")}
            desc={t("toolCanalyzerDesc")}
            tag={t("tagDebug")}
            accent="cyan"
            onClick={() => onPick("canalyzer")}
          />
          <ToolCard
            title={t("toolDfu")}
            desc={t("toolDfuDesc")}
            tag={t("tagFirmware")}
            accent="orange"
            onClick={() => onPick("dfu")}
          />
        </ToolSection>
      </div>
    </div>
  );
}

function ToolSection({ title, hint, children }: { title: string; hint: string; children: ReactNode }) {
  return (
    <section className="tool-section">
      <div className="tool-section__heading">
        <h2>{title}</h2>
        <span>{hint}</span>
      </div>
      <div className="tool-section__grid">{children}</div>
    </section>
  );
}

function ToolCard({
  title,
  desc,
  tag,
  accent,
  onClick,
}: {
  title: string;
  desc: string;
  tag: string;
  accent: "blue" | "amber" | "green" | "cyan" | "purple" | "pink" | "orange" | "lime" | "slate";
  onClick: () => void;
}) {
  return (
    <button className={`tool-card tool-card--${accent}`} type="button" onClick={onClick}>
      <span className="tool-card__title">{title}</span>
      <span className="tool-card__desc">{desc}</span>
      <span className="tool-card__tag">{tag}</span>
    </button>
  );
}
