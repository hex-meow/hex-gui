import { useCallback, useEffect, useState, type ReactNode } from "react";
import { App as AntdApp, Button, Empty, Layout, Tooltip, Typography } from "antd";
import { api, errMsg } from "./api";
import { useI18n } from "./i18n";
import { ConnectBar } from "./components/ConnectBar";
import { Sidebar } from "./components/Sidebar";
import { MotorDetail } from "./components/MotorDetail";
import { ImuPanel } from "./components/ImuPanel";
import { ChangeIdTool } from "./components/ChangeIdTool";
import { ZeroTool } from "./components/ZeroTool";
import { Hopea3Panel } from "./components/Hopea3Panel";
import { LiftPanel } from "./components/LiftPanel";
import { SmartKnobPanel } from "./components/SmartKnobPanel";
import RobotConsole from "./components/RobotConsole";
import { ZenohPanel } from "./components/ZenohPanel";
import { ArmPanel } from "./components/ArmPanel";
import { ControllerConfigPanel } from "./components/ControllerConfigPanel";
import { CanAnalyzerPanel } from "./components/CanAnalyzerPanel";
import { TutorialModal, TUTORIALS } from "./components/Tutorial";
import type { MotorInfo } from "./types";
import "./App.css";

type Tool = "control" | "changeId" | "zero" | "hopea3" | "lift" | "smartknob" | "zenoh" | "arm" | "config" | "canalyzer" | "console";

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

  // Auto-select the first motor once one appears (control mode only).
  useEffect(() => {
    if (tool === "control" && selectedNid == null && devices.length > 0) {
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
    try {
      await api.disconnect();
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
  }, [message, t]);

  const onToggleLog = useCallback(
    async (nid: number, on: boolean) => {
      try {
        if (on) {
          const path = await api.startLog(nid);
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
    changeId: { title: t("toolChangeId"), desc: t("toolChangeIdDesc") },
    zero: { title: t("toolZero"), desc: t("toolZeroDesc") },
    hopea3: { title: t("toolHopeA3"), desc: t("toolHopeA3Desc") },
    lift: { title: t("toolLift"), desc: t("toolLiftDesc") },
    smartknob: { title: t("toolSmartKnob"), desc: t("toolSmartKnobDesc") },
    zenoh: { title: t("toolBaseZenoh"), desc: t("toolBaseZenohDesc") },
    arm: { title: t("toolArmZenoh"), desc: t("toolArmZenohDesc") },
    config: { title: t("toolConfig"), desc: t("toolConfigDesc") },
    canalyzer: { title: t("toolCanalyzer"), desc: t("toolCanalyzerDesc") },
    console: { title: t("toolConsole"), desc: t("toolConsoleDesc") },
  } satisfies Record<Tool, { title: string; desc: string }>;
  const { title: toolTitle, desc: toolDesc } = toolMeta[tool];
  const needsHeartbeat = tool === "control" || tool === "hopea3" || tool === "smartknob";
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
    tool !== "canalyzer";
  const showConnectBar = tool !== "console" && tool !== "zenoh" && tool !== "arm" && tool !== "config" && tool !== "canalyzer";

  return (
    <Layout className={`app-shell app-shell--${tool}`}>
      <div className="app-chrome">
        <header className="app-chrome__header">
          <Button className="app-chrome__back" size="small" onClick={switchTool}>
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
              tool={tool as "control" | "changeId" | "zero"}
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
            <SmartKnobPanel connected={connected} devices={devices} />
          ) : tool === "zenoh" ? (
            <ZenohPanel />
          ) : tool === "arm" ? (
            <ArmPanel />
          ) : tool === "config" ? (
            <ControllerConfigPanel />
          ) : tool === "canalyzer" ? (
            <CanAnalyzerPanel />
          ) : tool === "changeId" ? (
            <ChangeIdTool devices={devices} selectedNid={selectedNid} connected={connected} />
          ) : tool === "zero" ? (
            <ZeroTool devices={devices} selectedNid={selectedNid} connected={connected} />
          ) : selected && selected.device_type === "imu" ? (
            <ImuPanel key={selected.node_id} info={selected} connected={connected} />
          ) : selected ? (
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
  const { t, lang, toggle } = useI18n();
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
          <h1>{t("toolPickerTitle")}</h1>
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
          <ToolCard
            title={t("toolSmartKnob")}
            desc={t("toolSmartKnobDesc")}
            tag={t("tagHaptics")}
            accent="lime"
            onClick={() => onPick("smartknob")}
          />
        </ToolSection>

        <ToolSection title={t("catRobotApp")} hint={t("catRobotAppHint")}>
          <ToolCard
            title={t("toolConsole")}
            desc={t("toolConsoleDesc")}
            tag={t("tagRobotApi")}
            accent="cyan"
            onClick={() => onPick("console")}
          />
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
          <ToolCard
            title={t("toolConfig")}
            desc={t("toolConfigDesc")}
            tag={t("tagConfig")}
            accent="slate"
            onClick={() => onPick("config")}
          />
        </ToolSection>

        <ToolSection title={t("catTools")} hint={t("catToolsHint")}>
          <ToolCard
            title={t("toolChangeId")}
            desc={t("toolChangeIdDesc")}
            tag={t("tagFactorySetup")}
            accent="amber"
            onClick={() => onPick("changeId")}
          />
          <ToolCard
            title={t("toolZero")}
            desc={t("toolZeroDesc")}
            tag={t("tagCalibration")}
            accent="green"
            onClick={() => onPick("zero")}
          />
          <ToolCard
            title={t("toolCanalyzer")}
            desc={t("toolCanalyzerDesc")}
            tag={t("tagDebug")}
            accent="cyan"
            onClick={() => onPick("canalyzer")}
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
