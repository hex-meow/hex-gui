// Arm(Zenoh)工具:发现/取控 + 3D 数字孪生 + 关节状态 + GRAVITY_COMP/设重力/预设位姿。
// 镜像 ZenohPanel 的连接/取控流;控制部分换成机械臂特有。
import { useCallback, useEffect, useState } from "react";
import { App as AntdApp, Button, Card, Input, InputNumber, Select, Space, Table, Tag, Tooltip, Typography } from "antd";
import { api, errMsg } from "../api";
import type { ArmInfo, ZenohArmState } from "../types";
import { ArmViewer } from "./ArmViewer";
import { DiagnosticsCard, FaultAlert } from "./DiagnosticsPanel";
import { useI18n } from "../i18n";

const POLL_MS = 33; // ~30fps for the twin

// 编译期预设位姿(rad)。TODO:之后可加“轨迹”(多点 chunk / 依次 goto)。
const PRESETS: { name: string; q: number[] }[] = [
  { name: "Home", q: [0, 0, 0, 0, 0, 0] },
  { name: "Ready", q: [0, -0.6, 1.2, 0, 0.6, 0] },
  { name: "Reach", q: [0, 0.8, 0.5, 0, 0.3, 0] },
  { name: "Tuck", q: [0, 1.4, 2.4, 0, 0.6, 0] },
];

export function ArmPanel() {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();
  const [endpoint, setEndpoint] = useState("");
  const [connected, setConnected] = useState(false);
  const [arms, setArms] = useState<ArmInfo[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [st, setSt] = useState<ZenohArmState | null>(null);
  const [gx, setGx] = useState(0);
  const [gy, setGy] = useState(0);
  const [gz, setGz] = useState(-9.81);
  const [busy, setBusy] = useState(false);
  const [urdfXml, setUrdfXml] = useState<string | null>(null); // 选中臂的 URDF(整机 arm+EE 或臂-only);null=退到捆的 firefly
  const [assembled, setAssembled] = useState(false); // URDF 含 EE(整机)→ 显示“整机(含EE)”标签
  const [previewQ, setPreviewQ] = useState<number[] | null>(null); // 预设悬浮预览
  const [kp, setKp] = useState(10); // host 侧增益(控制器忠实执行);有重力前馈后 kp=10 已够,更柔和
  const [kd, setKd] = useState(1.5);
  const [gMode, setGMode] = useState<"xyz" | "quat">("xyz"); // 重力输入方式:XYZ 分量 / 四元数朝向
  const [gMag, setGMag] = useState(9.81); // |g|
  const [qx, setQx] = useState(0); // 整臂安装朝向四元数(x,y,z,w);默认 (0,0,0,1)=竖直安装
  const [qy, setQy] = useState(0);
  const [qz, setQz] = useState(0);
  const [qw, setQw] = useState(1);

  useEffect(() => {
    if (!connected) { setSt(null); return; }
    let alive = true;
    const tick = async () => { try { const s = await api.armGetState(); if (alive) setSt(s); } catch { /* transient */ } };
    tick();
    const h = window.setInterval(tick, POLL_MS);
    return () => { alive = false; window.clearInterval(h); };
  }, [connected]);
  useEffect(() => () => { api.armDisconnect().catch(() => {}); }, []);
  // 选中(手动或自动)某臂即诊断聚焦:订阅其 events/logs + 播种历史(与取控解耦,只读也生效)。
  useEffect(() => { if (connected && selected) api.armSetDiagFocus(selected).catch(() => {}); }, [connected, selected]);
  // 选中即拉一次 URDF 供 3D 渲染(整机 arm+EE 或臂-only);取回 null/空则退到捆的 firefly。
  useEffect(() => {
    if (!connected || !selected) { setUrdfXml(null); setAssembled(false); return; }
    let alive = true;
    api.armGetUrdf(selected).then((u) => {
      if (!alive) return;
      setUrdfXml(u?.xml || null);
      setAssembled(!!u?.assembled);
    }).catch(() => { if (alive) { setUrdfXml(null); setAssembled(false); } });
    return () => { alive = false; };
  }, [connected, selected]);

  const connect = useCallback(async () => {
    setBusy(true);
    try {
      await api.armConnect(endpoint.trim());
      setConnected(true);
      let list = await api.armDiscover();
      if (!list.length) { await new Promise((r) => setTimeout(r, 900)); list = await api.armDiscover(); }
      setArms(list); setSelected(list[0]?.prefix ?? null);
      if (!list.length) message.warning("没发现机械臂(arm_controller 起了吗?)");
    } catch (e) { message.error(errMsg(e)); }
    finally { setBusy(false); }
  }, [endpoint, message]);

  const disconnect = useCallback(async () => {
    try { await api.armDisconnect(); } catch { /* ignore */ }
    setConnected(false); setArms([]); setSelected(null); setSt(null);
  }, []);
  const acquire = useCallback(async () => {
    const a = arms.find((x) => x.prefix === selected);
    if (!a) return;
    try { await api.armAcquire(a.prefix, a.model); message.success("已取得控制权"); } catch (e) { message.error(errMsg(e)); }
  }, [arms, selected, message]);
  const release = useCallback(async () => { try { await api.armRelease(); } catch (e) { message.error(errMsg(e)); } }, [message]);
  const setMode = useCallback(async (m: number) => { try { await api.armSetMode(m); } catch (e) { message.error(errMsg(e)); } }, [message]);
  const setGravity = useCallback(async () => { try { await api.armSetGravity([gx, gy, gz]); } catch (e) { message.error(errMsg(e)); } }, [gx, gy, gz, message]);
  const goTo = useCallback(async (q: number[]) => { try { await api.armGoto(q, kp, kd); } catch (e) { message.error(errMsg(e)); } }, [kp, kd, message]);

  // 四元数(整臂安装朝向)→ 基座系重力向量:臂按 q 旋转、世界重力恒朝下,故 base 系重力 = |g|·(q⁻¹·下)。
  // q⁻¹·(0,0,-1) 的解析式(单位四元数):(2(wy−xz), −2(wx+yz), 2(x²+y²)−1)。XYZ 仍是下发控制器的单一真值。
  const applyQuat = (m: number, x: number, y: number, z: number, w: number) => {
    const n = Math.hypot(x, y, z, w) || 1;
    const [ux, uy, uz, uw] = [x / n, y / n, z / n, w / n];
    const dx = 2 * (uw * uy - ux * uz);
    const dy = -2 * (uw * ux + uy * uz);
    const dz = 2 * (ux * ux + uy * uy) - 1;
    setGx(+(m * dx).toFixed(4)); setGy(+(m * dy).toFixed(4)); setGz(+(m * dz).toFixed(4));
  };
  const switchGMode = (mode: "xyz" | "quat") => {
    if (mode === "quat") { // 进 quat:从当前重力方向反解最小旋转(无 roll)作初值,避免切模式时画面跳变
      const m = Math.hypot(gx, gy, gz) || 9.81;
      setGMag(+m.toFixed(3));
      const d: [number, number, number] = [gx / m, gy / m, gz / m]; // 重力方向
      const dot = -d[2]; // d·(0,0,-1)
      if (dot > 0.9999) { setQx(0); setQy(0); setQz(0); setQw(1); }
      else if (dot < -0.9999) { setQx(1); setQy(0); setQz(0); setQw(0); }
      else {
        const ax = [-d[1], d[0], 0]; // cross(d, 下)
        const al = Math.hypot(ax[0], ax[1], ax[2]) || 1;
        const ang = Math.acos(Math.max(-1, Math.min(1, dot)));
        const s = Math.sin(ang / 2);
        setQx(+(ax[0] / al * s).toFixed(4)); setQy(+(ax[1] / al * s).toFixed(4)); setQz(+(ax[2] / al * s).toFixed(4)); setQw(+Math.cos(ang / 2).toFixed(4));
      }
    }
    setGMode(mode);
  };

  const controlling = !!st?.controlling;
  const dof = st?.dof || 6;
  const grav = [gx, gy, gz] as [number, number, number]; // 本地实时值 → 3D 预览随编辑即时倾斜(“设”才下发控制器)
  const armQuat = gMode === "quat" ? ([qx, qy, qz, qw] as [number, number, number, number]) : null;
  const names = st?.joint_names ?? Array.from({ length: dof }, (_, i) => `joint_${i + 1}`);
  const rows = names.map((n, i) => ({
    key: i, name: n,
    q: st?.q[i] ?? 0, dq: st?.dq[i] ?? 0, tau: st?.tau[i] ?? 0,
    temp: st?.temp?.[i] ?? null,
    lim: st?.pos_min?.[i] != null && st?.pos_max?.[i] != null ? `[${st.pos_min[i].toFixed(2)}, ${st.pos_max[i].toFixed(2)}]` : "",
  }));

  return (
    <Space direction="vertical" size={16} className="arm-panel" style={{ width: "100%", maxWidth: 1100 }}>
      <Card size="small" className="app-command-card">
        <Space wrap>
          <Typography.Text>Endpoint</Typography.Text>
          <Input style={{ width: 240 }} value={endpoint} disabled={connected} placeholder="留空=组播扫描,或 tcp/IP:7447" onChange={(e) => setEndpoint(e.target.value)} />
          {connected ? <Button onClick={disconnect}>断开</Button> : <Button type="primary" loading={busy} onClick={connect}>连接</Button>}
          {connected && (
            // 取控期间锁定选择:否则切换会让诊断聚焦(events/logs/故障灯/清障目标)漂到另一台,而
            // 关节/位姿/控制命令仍在原机器上 → 混淆视图 + 清障打错机器。要换机器先释放。
            <Select style={{ width: 340 }} value={selected ?? undefined} onChange={setSelected} disabled={controlling}
              options={arms.map((a) => ({ value: a.prefix, label: `${a.model} — ${a.prefix}${a.has_ee ? ` +EE(${a.ee_model || "?"})` : ""}` }))} />
          )}
          {connected && (controlling
            ? <Button danger onClick={release}>释放</Button>
            : <Button type="primary" disabled={!selected} onClick={acquire}>取控</Button>)}
        </Space>
      </Card>

      {connected && <FaultAlert fatal={!!st?.fatal} controlling={controlling} onClear={api.armClearFault} />}

      <Card size="small">
        <div style={{ display: "flex", gap: 16, flexWrap: "wrap" }}>
          <div style={{ flex: "1 1 480px", minWidth: 380 }}>
            <ArmViewer q={st?.q ?? []} gravity={grav} jointNames={st?.joint_names ?? []} previewQ={previewQ} armQuat={armQuat} urdfXml={urdfXml} />
          </div>
          <div style={{ flex: "1 1 320px", minWidth: 300 }}>
            <Space direction="vertical" size={12} style={{ width: "100%" }}>
              <Space wrap>
                {controlling ? <Tag color="green">控制中</Tag> : st && st.holder !== 0 ? <Tag color="orange">被占 #{st.holder}</Tag> : <Tag>未取控</Tag>}
                <Tag color="blue">{st?.mode ?? "—"}</Tag>
                {st?.has_ee ? <Tag color="purple">EE: {st.ee_model || "?"}</Tag> : <Tag>无 EE</Tag>}
                {assembled && <Tag color="green">整机(含EE)</Tag>}
              </Space>

              <Typography.Text strong>模式</Typography.Text>
              <Space wrap>
                <Button disabled={!controlling} onClick={() => setMode(4)}>GRAVITY_COMP</Button>
                <Button disabled={!controlling} onClick={() => setMode(3)}>PASSIVE</Button>
                <Button danger disabled={!controlling} onClick={() => setMode(1)}>DISABLE</Button>
              </Space>

              <Space wrap>
                <Typography.Text strong>重力向量 (m/s²)</Typography.Text>
                <Select size="small" style={{ width: 130 }} value={gMode} onChange={switchGMode}
                  options={[{ value: "xyz", label: "XYZ 分量" }, { value: "quat", label: "四元数朝向" }]} />
                <Button disabled={!controlling} onClick={setGravity}>设</Button>
              </Space>
              {gMode === "xyz" ? (
                <Space wrap>
                  <InputNumber style={{ width: 96 }} value={gx} step={0.5} prefix="x" onChange={(v) => setGx(v ?? 0)} />
                  <InputNumber style={{ width: 96 }} value={gy} step={0.5} prefix="y" onChange={(v) => setGy(v ?? 0)} />
                  <InputNumber style={{ width: 96 }} value={gz} step={0.5} prefix="z" onChange={(v) => setGz(v ?? 0)} />
                </Space>
              ) : (
                <>
                  <Space wrap>
                    <InputNumber style={{ width: 110 }} value={gMag} min={0} step={0.5} prefix="|g|" onChange={(v) => { const m = v ?? 0; setGMag(m); applyQuat(m, qx, qy, qz, qw); }} />
                    <InputNumber style={{ width: 96 }} value={qx} step={0.05} prefix="x" onChange={(v) => { const x = v ?? 0; setQx(x); applyQuat(gMag, x, qy, qz, qw); }} />
                    <InputNumber style={{ width: 96 }} value={qy} step={0.05} prefix="y" onChange={(v) => { const y = v ?? 0; setQy(y); applyQuat(gMag, qx, y, qz, qw); }} />
                    <InputNumber style={{ width: 96 }} value={qz} step={0.05} prefix="z" onChange={(v) => { const z = v ?? 0; setQz(z); applyQuat(gMag, qx, qy, z, qw); }} />
                    <InputNumber style={{ width: 96 }} value={qw} step={0.05} prefix="w" onChange={(v) => { const w = v ?? 0; setQw(w); applyQuat(gMag, qx, qy, qz, w); }} />
                  </Space>
                  <Typography.Link href="https://quaternions.online/" target="_blank" rel="noreferrer">四元数可视化工具 ↗</Typography.Link>
                </>
              )}
              <Typography.Text type="secondary">四元数 = 整臂安装朝向(x,y,z,w,Hamilton 约定,自动归一化);3D 随编辑即时旋转。斜装直接照真实安装姿态填;测试用 |g| 调小(如 2.9)更安全。</Typography.Text>

              <Typography.Text strong>增益 kp/kd(host 侧给,控制器忠实执行;调低 kp 移动柔和便于观察)</Typography.Text>
              <Space wrap>
                <InputNumber style={{ width: 120 }} value={kp} min={0} step={1} prefix="kp" onChange={(v) => setKp(v ?? 0)} />
                <InputNumber style={{ width: 120 }} value={kd} min={0} step={0.5} prefix="kd" onChange={(v) => setKd(v ?? 0)} />
              </Space>

              <Typography.Text strong>预设位姿(悬浮预览幽灵臂,点击移动)</Typography.Text>
              <Space wrap>
                {PRESETS.map((p) => (
                  <Tooltip key={p.name} title={`q = [${p.q.map((v) => v.toFixed(2)).join(", ")}] rad`}>
                    <Button
                      disabled={!controlling}
                      onMouseEnter={() => setPreviewQ(p.q)}
                      onMouseLeave={() => setPreviewQ(null)}
                      onClick={() => goTo(p.q)}
                    >{p.name}</Button>
                  </Tooltip>
                ))}
              </Space>
            </Space>
          </div>
        </div>
      </Card>

      <Card size="small" title="关节状态">
        <Table size="small" pagination={false} dataSource={rows}
          columns={[
            { title: "关节", dataIndex: "name" },
            { title: "q (rad)", dataIndex: "q", render: (v: number) => v.toFixed(3) },
            { title: "dq (rad/s)", dataIndex: "dq", render: (v: number) => v.toFixed(3) },
            { title: "τ (Nm)", dataIndex: "tau", render: (v: number) => v.toFixed(2) },
            { title: t("diagTemp"), dataIndex: "temp", render: (v: number | null) => v == null ? "—" : v.toFixed(1) },
            { title: "限位 (rad)", dataIndex: "lim" },
          ]} />
      </Card>

      {connected && (
        <DiagnosticsCard
          enabled={!!selected}
          getEvents={api.armGetEvents}
          getLogs={api.armGetLogs}
          onRefresh={api.armRefreshDiag}
        />
      )}
    </Space>
  );
}
