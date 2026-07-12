// 机器人控制台(P1 骨架)—— 单一入口:连接 → 设备树(自动发现,按 controller 分组)→
// 按 kind 路由到面板。EE 是第一个租户(EePanel);arm/base 面板 P3 迁入(暂给占位卡 +
// 指回主页旧面板);数字孪生 tab P4(待 base↔lift↔arm 安装变换出处定稿,12 §10.2)。
// 连接复用 zenoh_ee 模块的会话(ee_discover_all 做全量发现,所有 kind 一次拿全)。

import { useCallback, useEffect, useState } from "react";
import { App as AntdApp, Button, Card, Empty, Input, Layout, Menu, Space, Tag } from "antd";
import { api } from "../api";
import type { RobotNode } from "../types";
import { useI18n } from "../i18n";
import EePanel from "./EePanel";

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

  const connect = useCallback(async () => {
    try {
      await api.eeConnect(endpoint);
      setConnected(true);
      message.success(t("consoleConnected"));
    } catch (e) { message.error(String(e)); }
  }, [endpoint, message, t]);

  const disconnect = useCallback(async () => {
    await api.eeDisconnect().catch(() => {});
    setConnected(false); setNodes([]); setSel(null);
  }, []);

  // 周期全量发现(在线/离线以"出现在发现结果里"为准;liveliness 精细三态是后续优化)
  useEffect(() => {
    if (!connected) return;
    let stop = false;
    const tick = async () => {
      try { const ns = await api.eeDiscoverAll(); if (!stop) setNodes(ns); } catch { /* transient */ }
    };
    tick();
    const iv = setInterval(tick, DISCOVER_MS);
    return () => { stop = true; clearInterval(iv); };
  }, [connected]);

  // 断开/切出时释放会话(防呆:面板卸载不残留控制权)
  useEffect(() => () => { api.eeRelease().catch(() => {}); api.eeDisconnect().catch(() => {}); }, []);

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
        </Space>
      ),
    })),
  }));

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
        {connected && nodes.length === 0 && <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={t("consoleSearching")} />}
        <Menu
          mode="inline"
          style={{ borderInlineEnd: 0 }}
          defaultOpenKeys={menuItems.map((m) => m.key)}
          selectedKeys={sel ? [sel.prefix] : []}
          items={menuItems}
          onClick={({ key }) => setSel(nodes.find((n) => n.prefix === key) ?? null)}
        />
      </Sider>
      <Content style={{ padding: 14, overflow: "auto" }}>
        {!sel && (
          <Empty description={connected ? t("consolePickRobot") : t("consoleConnectFirst")} style={{ marginTop: 80 }} />
        )}
        {sel && sel.kind_name === "ee" && <EePanel key={sel.prefix} node={sel} />}
        {sel && sel.kind_name !== "ee" && (
          <Card size="small" title={`${sel.robot_index} · ${sel.model}`}>
            <p>{t("consoleKindPending")}</p>
            <p style={{ opacity: 0.6, fontSize: 12 }}>{t("consoleKindPendingHint")}</p>
          </Card>
        )}
      </Content>
    </Layout>
  );
}
