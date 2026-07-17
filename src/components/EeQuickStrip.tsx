// 附属 EE 快捷条(用户裁决 2026-07-12):控制臂时顺手开合它的爪——渲染在臂面板上方,
// 与完整 EePanel 共用同一 zenoh_ee 会话/命令流(同一 GUI 内不冲突,焦点归本条)。
// 无臂的独立 EE 仍走树选中 → 完整 EePanel。底盘+升降将来同构(WASDQE 顺手 Q/E 动升降)。

import { useCallback, useEffect, useRef, useState } from "react";
import { App as AntdApp, Button, Card, Slider, Space, Tag } from "antd";
import { api } from "../api";
import type { RobotNode, ZenohEeState } from "../types";

function graspColor(s: string): string {
  return s === "HOLDING" ? "success" : s === "MOVING" ? "processing" : s === "LOST" ? "error" : "default";
}

export function EeQuickStrip({ node }: { node: RobotNode }) {
  const { message } = AntdApp.useApp();
  const [st, setSt] = useState<ZenohEeState | null>(null);
  const [q, setQ] = useState(0);
  const dragging = useRef(false);

  useEffect(() => {
    api.eeSetFocus(node.prefix).catch(() => {});
    const iv = setInterval(async () => {
      try {
        const s = await api.eeGetState();
        setSt(s);
        if (!dragging.current && s.q.length) setQ(s.q[0]);
      } catch { /* transient */ }
    }, 50);
    return () => clearInterval(iv);
  }, [node.prefix]);

  const controlling = !!st?.controlling && st.prefix === node.prefix;
  const qMin = st?.pos_min[0] ?? 0;
  const qMax = st?.pos_max[0] ?? 0.6;

  const acquire = useCallback(async () => {
    try { await api.eeAcquire(node.prefix, node.model); } catch (e) { message.error(String(e)); }
  }, [node, message]);
  const goto_ = useCallback((v: number) => { api.eeGoto(v).catch((e) => message.error(String(e))); }, [message]);

  return (
    <Card size="small" styles={{ body: { padding: "6px 12px" } }}>
      <Space wrap style={{ width: "100%" }} size={10}>
        <Tag color="cyan" style={{ marginInlineEnd: 0 }}>EE</Tag>
        <span style={{ fontSize: 13 }}>{node.robot_index} · {node.model}</span>
        <Tag color={graspColor(st?.grasp_state ?? "")}>{st?.grasp_state || "—"}</Tag>
        <div style={{ flex: "1 1 220px", minWidth: 180, display: "inline-block" }}>
          <Slider min={qMin} max={qMax} step={0.001} value={q} disabled={!controlling}
            tooltip={{ formatter: (v) => `${(v ?? 0).toFixed(3)}` }}
            onChange={(v: number) => { dragging.current = true; setQ(v); goto_(v); }}
            onChangeComplete={(v: number) => { dragging.current = false; goto_(v); }} />
        </div>
        <Button size="small" disabled={!controlling} onClick={() => goto_(qMax)}>全开</Button>
        <Button size="small" disabled={!controlling} onClick={() => goto_(qMin)}>闭合</Button>
        {controlling
          ? <Button size="small" danger onClick={() => api.eeRelease().catch(() => {})}>释放</Button>
          : <Button size="small" type="primary" disabled={!!st && st.holder !== 0} onClick={acquire}>
              {st && st.holder !== 0 ? `被占 #${st.holder}` : "取控 EE"}</Button>}
      </Space>
    </Card>
  );
}
