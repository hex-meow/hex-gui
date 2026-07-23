// 机器人控制台(P1 骨架)—— 单一入口:连接 → 设备树(自动发现,按 controller 分组)→
// 按 kind 路由到面板。EE 是第一个租户(EePanel);arm/base 面板 P3 迁入(暂给占位卡 +
// 指回主页旧面板);数字孪生 tab P4(待 base↔lift↔arm 安装变换出处定稿,12 §10.2)。
// 连接复用 zenoh_ee 模块的会话(ee_discover_all 做全量发现,所有 kind 一次拿全)。

import { useCallback, useEffect, useState } from "react";
import { App as AntdApp, Button, Card, Empty, Input, InputNumber, Layout, Menu, Segmented, Space, Tag } from "antd";
import { api } from "../api";
import type { RobotNode } from "../types";
import type { MountEdge } from "../types";
import { useI18n } from "../i18n";
import EePanel from "./EePanel";
import { ArmPanel } from "./ArmPanel";
import { ZenohPanel } from "./ZenohPanel";
import { MachineViewer } from "./MachineViewer";
import { EeQuickStrip } from "./EeQuickStrip";
import type { SceneRobot } from "../types";
import { WifiSettingsDrawer } from "./WifiSettingsDrawer";

const { Sider, Content } = Layout;
const DISCOVER_MS = 3000;

const KIND_COLOR: Record<string, string> = { arm: "geekblue", base: "purple", lift: "gold", ee: "cyan" };

export default function RobotConsole() {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();
  const [endpoint, setEndpoint] = useState("tcp/127.0.0.1:7447");
  const [connected, setConnected] = useState(false);
  const [nodes, setNodes] = useState<RobotNode[]>([]);
  const [sel, setSel] = useState<RobotNode | null>(null);
  const [scene, setScene] = useState<SceneRobot[]>([]);
  const [held, setHeld] = useState<Set<string>>(new Set());
  const [machines, setMachines] = useState<Record<string, MountEdge[]>>({});
  const [focusMode, setFocusMode] = useState<"ghost" | "hide" | "off">(
    () => (localStorage.getItem("console.focusMode") as "ghost" | "hide" | "off") || "ghost");
  const [spacing, setSpacing] = useState<number>(() => Number(localStorage.getItem("console.spacing")) || 2);
  const [wifiOpen, setWifiOpen] = useState(false);

  const connect = useCallback(async () => {
    try {
      await api.eeConnect(endpoint);
      setConnected(true);
      message.success(t("consoleConnected"));
    } catch (e) { message.error(String(e)); }
  }, [endpoint, message, t]);

  const disconnect = useCallback(async () => {
    await Promise.allSettled([api.armRelease(), api.zenohRelease(), api.eeRelease()]);
    await Promise.allSettled([api.armDisconnect(), api.zenohDisconnect(), api.eeDisconnect()]);
    setConnected(false); setNodes([]); setSel(null); setHeld(new Set()); setWifiOpen(false);
  }, []);

  // 周期全量发现(在线/离线以"出现在发现结果里"为准;liveliness 精细三态是后续优化)
  useEffect(() => {
    if (!connected) return;
    let stop = false;
    const tick = async () => {
      try { const ns = await api.eeDiscoverAll(); if (!stop) setNodes(ns); } catch { /* transient */ }
      try { const ms = await api.eeMachines(); if (!stop) setMachines(ms); } catch { /* transient */ }
    };
    tick();
    const iv = setInterval(tick, DISCOVER_MS);
    return () => { stop = true; clearInterval(iv); };
  }, [connected]);

  // 场景快照轮询(M2 常驻 3D:全 kind 关节聚合,纯读缓存,30Hz)
  useEffect(() => {
    if (!connected) { setScene([]); return; }
    const iv = setInterval(async () => {
      try { setScene(await api.eeScene()); } catch { /* transient */ }
    }, 33);
    return () => clearInterval(iv);
  }, [connected]);

  // 持有徽标(1Hz):三个后端模块各自的 controlling+prefix → 树上"控制中"(会话跨切换保持后必须可见)
  useEffect(() => {
    if (!connected) { setHeld(new Set()); return; }
    const iv = setInterval(async () => {
      const hs = new Set<string>();
      try { const a = await api.armGetState(); if (a.controlling && a.prefix) hs.add(a.prefix); } catch { /* */ }
      try { const b = await api.zenohGetState(); if (b.controlling && b.prefix) hs.add(b.prefix); } catch { /* */ }
      try { const e = await api.eeGetState(); if (e.controlling && e.prefix) hs.add(e.prefix); } catch { /* */ }
      setHeld(hs);
    }, 1000);
    return () => clearInterval(iv);
  }, [connected]);

  const releaseAll = async () => {
    await Promise.allSettled([api.armRelease(), api.zenohRelease(), api.eeRelease()]);
    setHeld(new Set());
  };

  // console 退出 = 统一收口:释放三模块会话 + 断开(embedded 面板卸载不再各自释放——
  // 会话跨切换保持,切走臂不掉,用户裁决 2026-07-12)
  useEffect(() => () => {
    void Promise.allSettled([api.armRelease(), api.zenohRelease(), api.eeRelease()]).then(() => {
      api.armDisconnect().catch(() => {});
      api.zenohDisconnect().catch(() => {});
      api.eeDisconnect().catch(() => {});
    });
  }, []);

  // 设备树:按 cid 分组
  const byCid = new Map<string, RobotNode[]>();
  for (const n of nodes) {
    if (!byCid.has(n.cid)) byCid.set(n.cid, []);
    byCid.get(n.cid)!.push(n);
  }
  const menuItems = [...byCid.entries()].map(([cid, ns]) => ({
    key: `cid:${cid}`,
    label: `controller ${cid.slice(0, 8)}…`,
    children: ns.map((n) => ({
      key: n.prefix,
      label: (
        <Space size={6}>
          <span>{n.robot_index}</span>
          <Tag color={KIND_COLOR[n.kind_name] ?? "default"} style={{ marginInlineEnd: 0 }}>{n.kind_name}</Tag>
          <span style={{ opacity: 0.65, fontSize: 12 }}>{n.model}</span>
          {held.has(n.prefix) && <Tag color="green" style={{ marginInlineEnd: 0 }}>控制中</Tag>}
        </Space>
      ),
    })),
  }));
  const wifiCids = [...byCid.keys()].map((cid) => `hexmeow/${cid}`);

  return (
    <Layout style={{ height: "100%", background: "transparent" }}>
      <Sider width={270} theme="light" style={{ borderRight: "1px solid rgba(128,128,128,.25)", padding: 8 }}>
        <Space.Compact style={{ width: "100%", marginBottom: 8 }}>
          <Input size="small" value={endpoint} onChange={(e) => setEndpoint(e.target.value)}
            placeholder="tcp/host:7447" disabled={connected} />
          {connected
            ? <Button size="small" onClick={disconnect}>{t("consoleDisconnect")}</Button>
            : <Button size="small" type="primary" onClick={connect}>{t("consoleConnect")}</Button>}
        </Space.Compact>
        <Button
          size="small"
          block
          disabled={!connected}
          style={{ marginBottom: 8 }}
          onClick={() => setWifiOpen(true)}
        >
          {t("wifiSettings")}
        </Button>
        {connected && nodes.length === 0 && <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={t("consoleSearching")} />}
        {connected && (
          <div style={{ fontSize: 12, opacity: 0.75, padding: "2px 4px 6px" }}>
            {held.size > 0 && (
              <Button size="small" danger style={{ marginBottom: 6, width: "100%" }} onClick={releaseAll}>
                全部释放({held.size})
              </Button>
            )}
            <div style={{ marginBottom: 6 }}>
              <Segmented size="small" value={focusMode}
                onChange={(v) => { const m = v as "ghost" | "hide" | "off"; setFocusMode(m); localStorage.setItem("console.focusMode", m); }}
                options={[
                  { label: "幽灵", value: "ghost" },
                  { label: "隐藏", value: "hide" },
                  { label: "关", value: "off" },
                ]} />
            </div>
            {t("consoleSpacing")}
            <InputNumber size="small" min={0.5} step={0.5} value={spacing} style={{ width: 70, marginLeft: 6 }}
              onChange={(v) => { const x = v ?? 2; setSpacing(x); localStorage.setItem("console.spacing", String(x)); }} /> m
          </div>
        )}
        <Menu
          mode="inline"
          style={{ borderInlineEnd: 0 }}
          defaultOpenKeys={menuItems.map((m) => m.key)}
          selectedKeys={sel ? [sel.prefix] : []}
          items={menuItems}
          onClick={({ key }) => setSel(nodes.find((n) => n.prefix === key) ?? null)}
        />
      </Sider>
      <Content style={{ padding: 14, overflow: "auto", display: "flex", flexDirection: "column", gap: 12 }}>
        {connected && (
          <Card size="small" styles={{ body: { padding: 6 } }}>
            <MachineViewer robots={scene} selected={sel?.prefix ?? null} spacing={spacing}
              machines={machines} focusMode={focusMode}
              onSelect={(prefix) => { const n = nodes.find((x) => x.prefix === prefix); if (n) setSel(n); }}
              height={340} />
          </Card>
        )}
        {!sel && (
          <Empty description={connected ? t("consolePickRobot") : t("consoleConnectFirst")} style={{ marginTop: connected ? 24 : 80 }} />
        )}
        {sel && sel.kind_name === "ee" && <EePanel key={sel.prefix} node={sel} />}
        {sel && sel.kind_name === "arm" && (() => {
          const boundEe = nodes.find((n) => n.kind_name === "ee" && n.cid === sel.cid); // 精确 ee↔arm 映射 TODO(多臂时按 machine/EE_KEY)
          return (<>
            {boundEe && <EeQuickStrip key={boundEe.prefix} node={boundEe} />}
            <ArmPanel key={sel.prefix} embedded={{ endpoint, prefix: sel.prefix, model: sel.model }} />
          </>);
        })()}
        {sel && sel.kind_name === "base" && (
          <ZenohPanel key={sel.prefix} embedded={{ endpoint, prefix: sel.prefix, model: sel.model }} />
        )}
        {sel && !["ee", "arm", "base"].includes(sel.kind_name) && (
          <Card size="small" title={`${sel.robot_index} · ${sel.model}`}>
            <p>{t("consoleKindPending")}</p>
            <p style={{ opacity: 0.6, fontSize: 12 }}>{t("consoleKindPendingHint")}</p>
          </Card>
        )}
      </Content>
      <WifiSettingsDrawer
        open={wifiOpen}
        connected={connected}
        fallbackCids={wifiCids}
        onClose={() => setWifiOpen(false)}
      />
    </Layout>
  );
}
