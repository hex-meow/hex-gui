// EE(夹爪)控制面板 —— 机器人控制台骨架的第一个租户(P2)。
// 由 RobotConsole 托管:连接已建立(ee_* 命令共享 zenoh_ee 会话),本组件收 node props,
// 负责 观察聚焦 / 取控 / 开合滑条(50Hz 流)/ grasp_state 徽标 / estop 行为。
// 设计对应 robot-overall-design/11-ee-api.md。

import { useCallback, useEffect, useRef, useState } from "react";
import { App as AntdApp, Button, Card, Descriptions, Select, Slider, Space, Tag, InputNumber } from "antd";
import { api } from "../api";
import type { RobotNode, ZenohEeState } from "../types";
import { useI18n } from "../i18n";

const POLL_MS = 33;

/** width(q) = Σ poly[i]·q^i(OpeningMap,米→毫米显示)。poly 空 → null。 */
function openingMm(poly: number[], q: number): number | null {
  if (!poly.length) return null;
  let w = 0, p = 1;
  for (const c of poly) { w += c * p; p *= q; }
  return w * 1000;
}

function GraspTag({ s }: { s: string }) {
  const map: Record<string, { color: string; text: string }> = {
    MOVING: { color: "processing", text: "MOVING 运动中" },
    AT_POSITION: { color: "default", text: "AT_POSITION 空抓/到位" },
    HOLDING: { color: "success", text: "HOLDING 夹住" },
    LOST: { color: "error", text: "LOST 物体丢失" },
  };
  const m = map[s];
  return <Tag color={m?.color ?? "default"}>{m?.text ?? "—"}</Tag>;
}

export default function EePanel({ node }: { node: RobotNode }) {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();
  const [st, setSt] = useState<ZenohEeState | null>(null);
  const [q, setQ] = useState(0);          // 滑条本地值(拖动时立即回显)
  const [kp, setKp] = useState<number | null>(null); // null = 控制器默认增益
  const dragging = useRef(false);

  // 观察聚焦(只读即生效)+ 状态轮询
  useEffect(() => {
    api.eeSetFocus(node.prefix).catch(() => {});
    const iv = setInterval(async () => {
      try {
        const s = await api.eeGetState();
        setSt(s);
        if (!dragging.current && s.q.length) setQ(s.q[0]); // 未拖动时滑条跟随实测
      } catch { /* 未连接等瞬态 */ }
    }, POLL_MS);
    return () => clearInterval(iv);
  }, [node.prefix]);

  const controlling = st?.controlling && st.prefix === node.prefix;
  const qMin = st?.pos_min[0] ?? 0;
  const qMax = st?.pos_max[0] ?? 0.6;
  const mm = st ? openingMm(st.opening_poly, q) : null;

  const acquire = useCallback(async () => {
    try { await api.eeAcquire(node.prefix, node.model); message.success(t("eeAcquired")); }
    catch (e) { message.error(String(e)); }
  }, [node, message, t]);

  const release = useCallback(async () => { await api.eeRelease().catch(() => {}); }, []);

  const goto_ = useCallback(async (target: number) => {
    try { await api.eeGoto(target, kp ?? undefined); }
    catch (e) { message.error(String(e)); }
  }, [kp, message]);

  return (
    <Space direction="vertical" style={{ width: "100%" }} size={12}>
      <Card size="small">
        <Space wrap>
          <b>{node.robot_index} · {node.model}</b>
          <Tag>{st?.robot_mode || "—"}</Tag>
          {st?.fatal && <Tag color="error">FATAL</Tag>}
          {st && <GraspTag s={st.grasp_state} />}
          <span style={{ marginLeft: "auto" }} />
          {controlling
            ? <Button danger onClick={release}>{t("eeRelease")}</Button>
            : <Button type="primary" onClick={acquire}
                disabled={!!st && st.holder !== 0}>{st && st.holder !== 0 ? `${t("eeBusy")} #${st.holder}` : t("eeAcquire")}</Button>}
        </Space>
      </Card>

      <Card size="small" title={t("eeOpening")}>
        <Slider
          min={qMin} max={qMax} step={0.001} value={q}
          disabled={!controlling}
          tooltip={{ formatter: (v) => `${(v ?? 0).toFixed(3)} rad` }}
          onChange={(v: number) => { dragging.current = true; setQ(v); goto_(v); }}
          onChangeComplete={(v: number) => { dragging.current = false; goto_(v); }}
        />
        <Space wrap>
          <span>q = <b>{q.toFixed(3)}</b> {node.model === "gp80" ? "m" : "rad"}(0=全闭)</span>
          {mm != null && <span>开口 ≈ <b>{mm.toFixed(1)} mm</b></span>}
          <Button size="small" disabled={!controlling} onClick={() => goto_(qMax)}>{t("eeOpen")}</Button>
          <Button size="small" disabled={!controlling} onClick={() => goto_(qMin)}>{t("eeClose")}</Button>
          <Button size="small" disabled={!controlling} onClick={() => api.eeSetMode(1)}>{t("eeDisable")}</Button>
          {st?.fatal && <Button size="small" danger onClick={() => api.eeClearFault().catch((e) => message.error(String(e)))}>clear_fault</Button>}
        </Space>
      </Card>

      <Card size="small" title={t("eeAdvanced")}>
        <Space wrap size={16}>
          <span>
            kp(空=默认;小值=柔顺/限力抓取):
            <InputNumber size="small" min={0} step={1} value={kp} style={{ width: 90 }}
              onChange={(v) => setKp(v)} placeholder="默认" />
          </span>
          <span>
            estop 行为:
            <Select size="small" style={{ width: 190 }} disabled={!controlling}
              value={st?.estop_behavior || 1}
              onChange={(v) => api.eeSetEstopBehavior(v).catch((e) => message.error(String(e)))}
              options={[
                { value: 1, label: "HOLD_POSITION 保位" },
                { value: 2, label: "RELEASE 松开" },
                { value: 3, label: "KEEP_GRIP 抗拒张开" },
              ]} />
          </span>
        </Space>
        <Descriptions size="small" column={3} style={{ marginTop: 10 }}
          items={[
            { key: "q", label: "q", children: st?.q[0]?.toFixed(3) ?? "—" },
            { key: "dq", label: "dq", children: st?.dq[0]?.toFixed(3) ?? "—" },
            { key: "tau", label: "τ_est", children: st?.tau[0]?.toFixed(2) ?? "—" },
          ]} />
      </Card>
    </Space>
  );
}
