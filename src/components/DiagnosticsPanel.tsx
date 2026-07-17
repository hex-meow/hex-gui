// 诊断视图共享组件(Arm/Base Zenoh 面板复用):FATAL 故障条 + Events 查看 + Logs 查看。
// 后端(hex-controller)契约:events 可靠层(<prefix>/events + events/recent),logs 尽力层
// (hexmeow/<cid>/*/log + log/recent)。见 src-tauri/src/diag.rs。
import { useCallback, useEffect, useRef, useState } from "react";
import { App as AntdApp, Alert, Button, Empty, Segmented, Space, Table, Tag, Tooltip, Typography } from "antd";
import { ReloadOutlined } from "@ant-design/icons";
import type { EventsSnapshot, LogLine } from "../types";
import { errMsg } from "../api";
import { useI18n } from "../i18n";
import type { I18nKey } from "../i18n";

// severity: 1=INFO 2=WARNING 3=ERROR 4=FATAL(对齐 proto EventSeverity)。
function sevMeta(sev: number): { color: string; key: I18nKey } {
  switch (sev) {
    case 4: return { color: "magenta", key: "diagSevFatal" };
    case 3: return { color: "red", key: "diagSevError" };
    case 2: return { color: "gold", key: "diagSevWarn" };
    default: return { color: "default", key: "diagSevInfo" };
  }
}

const levelColor = (lv: string): string | undefined => {
  const u = lv.toUpperCase();
  if (u === "ERROR") return "#ff7875";
  if (u === "WARN" || u === "WARNING") return "#ffc53d";
  return undefined; // INFO/DEBUG/TRACE/未解析 → 默认色
};

// ───────────────────────── FATAL 故障条 + 清除故障 ─────────────────────────

export function FaultAlert({
  fatal,
  controlling,
  onClear,
}: {
  fatal: boolean;
  controlling: boolean;
  onClear: () => Promise<void>;
}) {
  const { t } = useI18n();
  const { message } = AntdApp.useApp();
  const [busy, setBusy] = useState(false);
  if (!fatal) return null;

  const clear = async () => {
    setBusy(true);
    try {
      await onClear();
      message.success(t("diagFaultCleared"));
    } catch (e) {
      message.error(errMsg(e));
    } finally {
      setBusy(false);
    }
  };

  const clearBtn = controlling ? (
    <Button danger size="small" loading={busy} onClick={clear}>
      {t("diagClearFault")}
    </Button>
  ) : (
    <Tooltip title={t("diagClearNeedControl")}>
      <span>
        <Button danger size="small" disabled>
          {t("diagClearFault")}
        </Button>
      </span>
    </Tooltip>
  );

  return (
    <Alert
      type="error"
      showIcon
      message={t("diagFaultTitle")}
      description={t("diagFaultDesc")}
      action={clearBtn}
    />
  );
}

// ───────────────────────── 控制器 RobotMode(只读观察)─────────────────────────

// 控制器上报的 RobotMode(设计 §3),base/arm 统一展示。与"我方是否在控/holder"是**不同维度**:
// 例如只读观察(未取控)时仍能看到机器 RUNNING(别人在控)/ STANDBY / OVERTAKEN / FATAL_ERROR。
// mode 取后端 diag::robot_mode_name(...) 的稳定短名;空/UNSPECIFIED → 不渲染。
export function RobotModeTag({ mode, overtaken }: { mode?: string; overtaken?: string }) {
  const { t } = useI18n();
  const meta: Record<string, { color: string; key: I18nKey }> = {
    STANDBY: { color: "default", key: "rmStandby" },
    RUNNING: { color: "green", key: "rmRunning" },
    OVERTAKEN: { color: "orange", key: "rmOvertaken" },
    FATAL_ERROR: { color: "red", key: "rmFatal" },
  };
  const m = mode ? meta[mode] : undefined;
  if (!m) return null;
  const label = t(m.key) + (mode === "OVERTAKEN" && overtaken ? `: ${overtaken}` : "");
  return (
    <Tooltip title={t("rmTip")}>
      <Tag color={m.color}>{label}</Tag>
    </Tooltip>
  );
}

// ───────────────────────── Events 查看 ─────────────────────────

function EventsTab({ enabled, getEvents }: { enabled: boolean; getEvents: () => Promise<EventsSnapshot> }) {
  const { t } = useI18n();
  const { notification } = AntdApp.useApp();
  const [snap, setSnap] = useState<EventsSnapshot>({ events: [], baseline_seq: 0 });
  // 只对 seq >= baseline 且尚未通知过的 ERROR/FATAL 弹通知(历史/重拉的旧事件恒被抑制)。
  const notifiedSeq = useRef<number>(-1);

  // 重连会让后端 event_seq 从 0 重新计数(新 ZenohConn),而本组件常驻不卸载、ref 不复位;
  // 若不重置,旧高水位会把重连后小 seq 的实时 ERROR/FATAL 全判成"已通知"而静默吞掉。
  // 只随 enabled(连接)变化复位;切机器/切语言 enabled 不变 → 不复位 → 不会重复弹已通知事件。
  // (声明在轮询 effect 之前,保证同一次 commit 里先复位再起新轮询。)
  useEffect(() => { notifiedSeq.current = -1; }, [enabled]);

  useEffect(() => {
    if (!enabled) {
      setSnap({ events: [], baseline_seq: 0 });
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const s = await getEvents();
        if (!alive) return;
        setSnap(s);
        let maxSeq = notifiedSeq.current;
        for (const ev of s.events) {
          if (ev.seq >= s.baseline_seq && ev.seq > notifiedSeq.current && ev.severity >= 3) {
            const fatal = ev.severity >= 4;
            (fatal ? notification.error : notification.warning)({
              message: `${t(sevMeta(ev.severity).key)} · ${ev.code}`,
              description: ev.text || undefined,
              duration: fatal ? 0 : 6,
            });
          }
          if (ev.seq > maxSeq) maxSeq = ev.seq;
        }
        notifiedSeq.current = Math.max(notifiedSeq.current, maxSeq, s.baseline_seq - 1);
      } catch {
        /* transient */
      }
    };
    tick();
    const h = window.setInterval(tick, 700);
    return () => {
      alive = false;
      window.clearInterval(h);
    };
  }, [enabled, getEvents, notification, t]);

  const rows = [...snap.events].reverse(); // 最新在上

  return (
    <Table
      size="small"
      pagination={false}
      scroll={{ y: 300 }}
      locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={t("diagNoEvents")} /> }}
      dataSource={rows.map((e) => ({ ...e, key: e.seq }))}
      columns={[
        {
          title: t("diagColSeverity"),
          dataIndex: "severity",
          width: 96,
          render: (s: number) => {
            const m = sevMeta(s);
            return <Tag color={m.color}>{t(m.key)}</Tag>;
          },
        },
        {
          title: t("diagColCode"),
          dataIndex: "code",
          width: 180,
          render: (c: string) => <Typography.Text code>{c}</Typography.Text>,
        },
        { title: t("diagColText"), dataIndex: "text" },
        {
          title: t("diagColInfo"),
          dataIndex: "kv",
          render: (kv: [string, string][]) =>
            kv.length ? (
              <Space size={4} wrap>
                {kv.map(([k, v]) => (
                  <Tag key={k}>{k}: {v}</Tag>
                ))}
              </Space>
            ) : (
              <Typography.Text type="secondary">—</Typography.Text>
            ),
        },
      ]}
    />
  );
}

// ───────────────────────── Logs 查看 ─────────────────────────

function LogsTab({ enabled, getLogs }: { enabled: boolean; getLogs: () => Promise<LogLine[]> }) {
  const { t } = useI18n();
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [level, setLevel] = useState<"all" | "warn" | "error">("all");
  const boxRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true); // 用户停在底部时才自动跟随新日志

  useEffect(() => {
    if (!enabled) {
      setLogs([]);
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const l = await getLogs();
        if (alive) setLogs(l);
      } catch {
        /* transient */
      }
    };
    tick();
    const h = window.setInterval(tick, 800);
    return () => {
      alive = false;
      window.clearInterval(h);
    };
  }, [enabled, getLogs]);

  const shown = logs.filter((l) => {
    if (level === "all") return true;
    const u = l.level.toUpperCase();
    if (level === "error") return u === "ERROR";
    return u === "ERROR" || u === "WARN" || u === "WARNING";
  });

  // 跟随最新日志:依赖 logs(每次轮询都是新数组引用,即使可见行数不变、内容却在滚动),
  // 这样"行数稳定但内容变化"时也会重新贴底(仅当用户停在底部 stick=true)。
  useEffect(() => {
    const el = boxRef.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [logs, level]);

  const onScroll = () => {
    const el = boxRef.current;
    if (!el) return;
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
  };

  return (
    <Space direction="vertical" size={8} style={{ width: "100%" }}>
      <Segmented
        size="small"
        value={level}
        onChange={(v) => setLevel(v as "all" | "warn" | "error")}
        options={[
          { value: "all", label: t("diagLevelAll") },
          { value: "warn", label: "WARN+" },
          { value: "error", label: "ERROR" },
        ]}
      />
      <div
        ref={boxRef}
        onScroll={onScroll}
        style={{
          height: 300,
          overflow: "auto",
          background: "#0b0e14",
          border: "1px solid #262b35",
          borderRadius: 6,
          padding: "8px 10px",
          fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
          fontSize: 12,
          lineHeight: 1.5,
          whiteSpace: "pre-wrap",
          wordBreak: "break-word",
        }}
      >
        {shown.length === 0 ? (
          <Typography.Text type="secondary">{t("diagNoLogs")}</Typography.Text>
        ) : (
          shown.map((l, i) => (
            <div key={i} style={{ color: levelColor(l.level) }}>
              <span style={{ color: "#7a8494" }}>[{l.proc}]</span> {l.level && `${l.level} `}
              {l.target && <span style={{ color: "#7a8494" }}>{l.target} </span>}
              {l.msg}
            </div>
          ))
        )}
      </div>
    </Space>
  );
}

// ───────────────────────── 诊断卡片(Events / Logs Tabs + 刷新历史)─────────────────────────

export function DiagnosticsCard({
  enabled,
  getEvents,
  getLogs,
  onRefresh,
}: {
  enabled: boolean;
  getEvents: () => Promise<EventsSnapshot>;
  getLogs: () => Promise<LogLine[]>;
  onRefresh: () => Promise<void>;
}) {
  const { t } = useI18n();
  const { message } = AntdApp.useApp();
  const [tab, setTab] = useState<"events" | "logs">("events");
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    setBusy(true);
    try {
      await onRefresh();
    } catch (e) {
      message.error(errMsg(e));
    } finally {
      setBusy(false);
    }
  }, [onRefresh, message]);

  return (
    <div style={{ border: "1px solid #262b35", borderRadius: 8, padding: 12 }}>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 8 }}>
        <Segmented
          value={tab}
          onChange={(v) => setTab(v as "events" | "logs")}
          options={[
            { value: "events", label: t("diagEvents") },
            { value: "logs", label: t("diagLogs") },
          ]}
        />
        <Tooltip title={t("diagRefreshHint")}>
          <Button size="small" icon={<ReloadOutlined />} loading={busy} disabled={!enabled} onClick={refresh}>
            {t("diagRefresh")}
          </Button>
        </Tooltip>
      </div>
      {/* 两个 tab 都常挂载:各自轮询,切换不丢流(enabled 控制轮询启停)。 */}
      <div style={{ display: tab === "events" ? "block" : "none" }}>
        <EventsTab enabled={enabled} getEvents={getEvents} />
      </div>
      <div style={{ display: tab === "logs" ? "block" : "none" }}>
        <LogsTab enabled={enabled} getLogs={getLogs} />
      </div>
    </div>
  );
}
