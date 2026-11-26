import { useEffect, useMemo, useRef, useState } from "react";
import { Alert, App, Button, Card, Checkbox, Input, InputNumber, Segmented, Select, Slider, Space, Tag } from "antd";
import { errMsg } from "../api";
import { isGsUsbSpec, STANDARD_DATA_BITRATES } from "../canProfile";
import { useI18n } from "../i18n";
import { assembledUrdf, defaultGravityScale, foldedQ, importProductUrdf, rawArm, type ArmPreview, type ArmStatus, type ArmTarget, type Pose, type Product, type RawModel, type ToolKind } from "../rawArm";
import { URDF_ASSET_PRODUCTS } from "../urdfModelLoader";
import { RawArmPreview } from "./RawArmPreview";
import { RawArmGripper } from "./RawArmGripper";
import { RawArmSettings, preferenceKey, readPreferences, replaceAt } from "./RawArmSettings";
import "./RawArmPanel.css";

interface OpenModel { info: RawModel; xml: string }
interface Source { xml: string; name: string }

function isGravityScale(value: number | null): value is number {
  return value !== null && Number.isFinite(value) && value >= 0;
}

async function productXml(product: Product, signal: AbortSignal) {
  const response = await fetch(URDF_ASSET_PRODUCTS[product].urdfUrl, { signal });
  if (!response.ok) throw new Error(`${product} URDF · HTTP ${response.status}`);
  return response.text();
}

function PoseRow({ label, pose }: { label: string; pose?: Pose }) {
  return <tr><th>{label}</th>{[0, 1, 2].map(i => <td key={i}>{pose ? (pose.xyz[i] * 1000).toFixed(1) : "—"}</td>)}</tr>;
}

export function RawArmPanel() {
  const { lang } = useI18n();
  const zh = lang === "zh";
  const { message } = App.useApp();
  const [assembly, setAssembly] = useState<ToolKind | "bare">("gp80");
  const [toolTarget, setToolTarget] = useState<number | null>(null);
  const [sources, setSources] = useState<Partial<Record<Product, Source>>>({});
  const fileInput = useRef<HTMLInputElement>(null);
  const [model, setModel] = useState<OpenModel | null>(null);
  const [modelError, setModelError] = useState("");
  const [modelReady, setModelReady] = useState(false);
  const [iface, setIface] = useState(() => /linux/i.test(navigator.userAgent) ? "can0" : "gs_usb0");
  const [bitrate, setBitrate] = useState(5_000_000);
  const [connected, setConnected] = useState(false);
  const [initializedView, setInitializedView] = useState(false);
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const [status, setStatus] = useState<ArmStatus | null>(null);
  const [gravityDraft, setGravityDraft] = useState<(number | null)[] | null>(null);
  const [commandError, setCommandError] = useState("");
  const [factory, setFactory] = useState<(number | null)[]>([]);
  const [prefs, setPrefs] = useState(readPreferences);
  const [entry, setEntry] = useState<"joints" | "tcp">("joints");
  const [q, setQ] = useState<number[]>([]);
  const [xyz, setXyz] = useState([0, 0, 0]);
  const [rpy, setRpy] = useState([0, 0, 0]);
  const [useOrientation, setUseOrientation] = useState(false);
  const [revision, setRevision] = useState(0);
  const seedFromBus = useRef(false);
  const feedback = status?.feedback ?? null;
  const liveSeed = feedback?.preview_seed;
  const measuredTarget = liveSeed && "q" in liveSeed ? liveSeed : null;
  const feedbackError = liveSeed && "error" in liveSeed ? liveSeed.error : null;
  const mode = status?.mode ?? "monitor";
  const appliedScale = status?.gravity_scale ?? defaultGravityScale;
  const gravityScale = gravityDraft ?? appliedScale;
  const validScale = gravityScale.every(isGravityScale) ? gravityScale : null;
  const scaleChanged = gravityScale.some((v, i) => v !== appliedScale[i]);

  function seed(joints: number[], tcp: Pose) {
    setQ([...joints]); setXyz([...tcp.xyz]); setRpy([...tcp.rpy]);
  }

  function clearModelView() {
    setModel(null);
    setToolTarget(null);
    setModelReady(false);
    setModelError("");
  }

  useEffect(() => {
    let live = true;
    void rawArm.session().then(session => {
      if (!live) return;
      if (session) {
        const xml = assembledUrdf(session.arm_xml, session.tool_xml);
        const kind = session.model.tool?.kind;
        setSources({ firefly_y6: { xml: session.arm_xml, name: "Firefly Y6" },
          ...(kind && session.tool_xml && { [kind]: { xml: session.tool_xml, name: kind.toUpperCase() } }) });
        setAssembly(kind ?? "bare"); setModel({ info: session.model, xml });
        setIface(session.iface); setBitrate(session.data_bitrate); setFactory(session.factory);
        setPrefs(p => ({ ...p, installation: session.installation }));
        seedFromBus.current = true; setConnected(true);
      }
      setInitializedView(true);
    }).catch(e => { if (live) setModelError(errMsg(e)); });
    return () => { live = false; };
  }, []);

  useEffect(() => {
    try { localStorage.setItem(preferenceKey, JSON.stringify(prefs)); } catch { /* Optional user preferences. */ }
  }, [prefs]);

  useEffect(() => {
    if (!initializedView || connected) return;
    const abort = new AbortController();
    clearModelView();
    void (async () => {
      const [arm, tool] = await Promise.all([
        sources.firefly_y6?.xml ?? productXml("firefly_y6", abort.signal),
        assembly === "bare" ? null : (sources[assembly]?.xml ?? productXml(assembly, abort.signal)),
      ]);
      if (abort.signal.aborted) return;
      const xml = assembledUrdf(arm, tool);
      const info = await rawArm.model(arm, tool);
      if (abort.signal.aborted) return;
      setModel({ info, xml });
      seed(info.home, info.tcp);
    })().catch(e => { if (!abort.signal.aborted) setModelError(errMsg(e)); });
    return () => abort.abort();
  }, [sources, assembly, initializedView, connected]);

  async function openFile(file: File) {
    try {
      const { product, xml } = importProductUrdf(await file.text());
      clearModelView();
      if (product !== "firefly_y6") setAssembly(product);
      setSources(old => ({ ...old, [product]: { xml, name: file.name } }));
    } catch (e) { setModelError(errMsg(e)); }
  }

  useEffect(() => {
    if (!connected) return;
    let alive = true;
    let timer = 0;
    const poll = async () => {
      try {
        const s = await rawArm.status();
        if (!alive) return;
        setStatus(s);
        const nextSeed = s.feedback?.preview_seed;
        if (seedFromBus.current && nextSeed && "q" in nextSeed) {
          seedFromBus.current = false;
          seed(nextSeed.q, nextSeed.tcp);
        }
      } catch (e) {
        if (alive) setStatus(old => ({ mode: old?.mode ?? "monitor", gravity_scale: old?.gravity_scale ?? defaultGravityScale,
          feedback: null, error: errMsg(e) }));
      } finally { if (alive) timer = window.setTimeout(poll, 50); }
    };
    void poll();
    return () => { alive = false; window.clearTimeout(timer); };
  }, [connected]);

  const target = useMemo<ArmTarget>(() => entry === "joints"
    ? { kind: "joints", q } : { kind: "tcp", xyz, rpy: useOrientation ? rpy : null }, [entry, q, xyz, rpy, useOrientation]);
  // Tie the rendered result to the exact draft. A keystroke immediately removes
  // the previous execute action, even before the debounced planner has started.
  const available = !connected || !!measuredTarget;
  const request = useMemo(() => ({ model, target, limits: prefs.limits, available, connected, revision }),
    [model, target, prefs.limits, available, connected, revision]);
  const [result, setResult] = useState<{ request: typeof request; preview?: ArmPreview; error?: string } | null>(null);
  const lastPlanAt = useRef(0);
  useEffect(() => {
    if (!request.model || !request.available) return;
    let live = true;
    const timer = window.setTimeout(async () => {
      lastPlanAt.current = performance.now();
      const t = request.target;
      const values = t.kind === "joints" ? t.q : [...t.xyz, ...(t.rpy ?? [])];
      if (!values.length || !values.every(Number.isFinite)) {
        setResult({ request, error: zh ? "请补全目标坐标。" : "Complete the target coordinates." });
        return;
      }
      try {
        const preview = await rawArm.preview(t, request.limits);
        if (live) setResult({ request, preview });
      } catch (e) { if (live) setResult({ request, error: errMsg(e) }); }
    }, Math.max(0, 70 - (performance.now() - lastPlanAt.current)));
    return () => { live = false; window.clearTimeout(timer); };
  }, [request, zh]);
  const preview = result?.request === request ? result.preview ?? null : null;
  const planError = result?.request === request ? result.error : null;

  async function action(run: () => Promise<void>) {
    if (busyRef.current) return;
    busyRef.current = true; setBusy(true); setCommandError("");
    try { await run(); } catch (e) { setCommandError(errMsg(e)); }
    finally { busyRef.current = false; setBusy(false); }
  }
  async function connect() {
    const factors = await rawArm.connect(iface.trim(), bitrate, prefs.installation);
    setFactory(factors); seedFromBus.current = true; setConnected(true);
  }
  async function disconnect() {
    await rawArm.disconnect();
    setConnected(false); setStatus(null); setGravityDraft(null); setFactory([]);
    if (model) seed(model.info.home, model.info.tcp);
  }
  async function applyOffsets(offset: number[]) {
    await rawArm.offsets(offset);
    setPrefs(p => ({ ...p, installation: { ...p.installation, offset } }));
    seedFromBus.current = true; setRevision(n => n + 1);
    message.success(zh ? "已更新上位机关节零偏" : "Host joint offsets updated");
  }
  async function applyGravityScale() {
    if (!validScale) throw new Error(zh ? "请输入六个非负的补偿系数。" : "Enter six nonnegative gravity scales.");
    await rawArm.setGravityScale(validScale);
    setStatus(s => s && { ...s, gravity_scale: validScale });
    setGravityDraft(null);
  }
  async function control(next: "hold" | "float") {
    try {
      await rawArm.control(next, prefs.gains);
      setStatus(await rawArm.status());
    } finally { setRevision(n => n + 1); }
  }
  const locked = busy || connected || !initializedView;
  const actual = feedback?.tcp;
  const activeSources = [sources.firefly_y6?.name, assembly === "bare" ? null : sources[assembly]?.name].filter(Boolean);
  const modeText = !connected ? (zh ? "离线预览" : "Offline preview") : {
    monitor: zh ? "已初始化 · 监视" : "Initialized · monitor",
    hold: feedback?.moving ? (zh ? "轨迹执行中" : "Moving") : (zh ? "位置保持" : "Position hold"),
    float: zh ? "零重力拖动" : "Gravity-compensated hand-guiding",
    recovery: zh ? "请检查各轴模式" : "Check drive modes",
  }[mode];
  return <div className="raw-arm">
    <div className="raw-arm__connection">
      <label>{zh ? "CAN 接口" : "CAN interface"}<Input value={iface} onChange={e => setIface(e.target.value)} disabled={locked} /></label>
      {isGsUsbSpec(iface) && <label>{zh ? "数据速率" : "Data rate"}<Select value={bitrate} onChange={setBitrate} disabled={locked}
        options={STANDARD_DATA_BITRATES.map(value => ({ value, label: `${value / 1e6} Mbit/s` }))} /></label>}
      <label>{zh ? "装配模型" : "Assembly"}<Select value={assembly} onChange={v => { clearModelView(); setAssembly(v); }} disabled={locked}
        options={[{ value: "gp80", label: "Firefly Y6 + GP80" }, { value: "gr80", label: "Firefly Y6 + GR80" }, { value: "bare", label: "Firefly Y6" }]} /></label>
      <Button disabled={locked} title={zh ? "打开引用产品网格的 Firefly Y6、GP80 或 GR80 URDF" : "Open a Firefly Y6, GP80 or GR80 URDF using the product meshes"}
        onClick={() => fileInput.current?.click()}>{zh ? "打开 URDF" : "Open URDF"}</Button>
      <input type="file" hidden ref={fileInput} accept=".urdf,.xml" onChange={e => {
        const file = e.currentTarget.files?.[0]; e.currentTarget.value = ""; if (file) void action(() => openFile(file));
      }} />
      {Object.keys(sources).length > 0 && <Button disabled={locked} onClick={() => { clearModelView(); setSources({}); }}>{zh ? "恢复内置模型" : "Use bundled models"}</Button>}
      <Button type={connected ? "default" : "primary"} loading={busy && !connected} disabled={busy || (!connected && (!model || !iface.trim()))}
        onClick={() => void action(connected ? disconnect : connect)}>
        {connected ? (zh ? "断开" : "Disconnect") : (zh ? "连接并初始化" : "Connect and initialize")}
      </Button>
      <Tag color={connected ? "cyan" : "default"}>{modeText}</Tag>
    </div>
    {modelError && <Alert type="error" showIcon message={modelError} />}
    {(commandError || status?.error) && <Alert type="error" showIcon message={[commandError, status?.error].filter(Boolean).join(" · ")} />}
    <div className="raw-arm__workspace">
      <Card className="raw-arm__model" title={zh ? "机械臂视图" : "Arm view"}
        extra={<span className="raw-arm__legend"><i />{zh ? "实测" : "Measured"}<i />{zh ? "规划轨迹" : "Planned"}</span>}>
        {model ? <RawArmPreview model={model.info} xml={model.xml} toolTarget={toolTarget} preview={preview}
          feedback={feedback} onReady={setModelReady} /> : <div className="raw-arm__empty">{modelError || (zh ? "正在打开 URDF…" : "Opening URDF…")}</div>}
        <table className="raw-arm__tcp"><thead><tr><th>{zh ? "基座系 TCP" : "TCP in base frame"}</th><th>X · mm</th><th>Y · mm</th><th>Z · mm</th></tr></thead>
          <tbody><PoseRow label={zh ? "实测" : "Measured"} pose={actual} /><PoseRow label={zh ? "目标" : "Target"} pose={preview?.tcp} /></tbody></table>
        {feedback?.gravity_nm && <div className="raw-arm__detail">{zh ? "重力前馈指令" : "Commanded gravity feedforward"} · N·m　
          {feedback.gravity_nm.map((v, i) => `J${i + 1} ${v.toFixed(2)}`).join("　")}</div>}
        {activeSources.length > 0 && <div className="raw-arm__detail">{activeSources.join(" + ")}</div>}
      </Card>
      <Card title={zh ? "运动控制" : "Motion control"}>
        <Segmented value={entry} block options={[{ value: "joints", label: zh ? "关节正解" : "Joint FK" }, { value: "tcp", label: zh ? "末端逆解" : "TCP IK" }]}
          onChange={v => setEntry(v as typeof entry)} />
        {entry === "joints" ? <div className="raw-arm__joints">
          {model?.info.joints.map((j, i) => <div className="raw-arm__joint" key={j.name}>
            <label htmlFor={`raw-joint-${i}`}>J{i + 1} · rad</label>
            <Slider ariaLabelForHandle={j.name} min={j.lower} max={j.upper} step={0.001}
              value={Number.isFinite(q[i]) ? q[i] : model.info.home[i]} onChange={v => setQ(replaceAt(q, i, v))} />
            <InputNumber id={`raw-joint-${i}`} value={Number.isFinite(q[i]) ? q[i] : null}
              step={0.01} onChange={v => setQ(replaceAt(q, i, v ?? NaN))} />
            <span title={zh ? "实测关节角 · rad" : "Measured joint angle · rad"}>{feedback?.q[i].toFixed(3) ?? "—"}</span>
          </div>)}</div> : <div className="raw-arm__cartesian">
          <h3>{zh ? "基座坐标系" : "Base frame"} · {assembly === "bare" ? (zh ? "法兰" : "Flange") : `${assembly.toUpperCase()} TCP`}</h3>
          <div className="raw-arm__fields">{["X", "Y", "Z"].map((k, i) => <label key={k}>{k} · mm
            <InputNumber value={Number.isFinite(xyz[i]) ? xyz[i] * 1000 : null} step={1}
              onChange={v => setXyz(replaceAt(xyz, i, v === null ? NaN : v / 1000))} /></label>)}</div>
          <Checkbox checked={useOrientation} onChange={e => setUseOrientation(e.target.checked)}>{zh ? "同时指定末端姿态" : "Constrain tool orientation"}</Checkbox>
          {useOrientation && <div className="raw-arm__fields">{["Roll", "Pitch", "Yaw"].map((k, i) => <label key={k}>{k} · °
            <InputNumber value={Number.isFinite(rpy[i]) ? rpy[i] * 180 / Math.PI : null} step={1}
              onChange={v => setRpy(replaceAt(rpy, i, v === null ? NaN : v * Math.PI / 180))} /></label>)}</div>}
        </div>}
        {(feedbackError || planError) && <Alert type="warning" showIcon message={feedbackError || planError} description={
          feedbackError
            ? (zh ? "实测关节角超出当前 URDF 范围。可在安装标定中核对方向与零偏。" :
              "A measured joint is outside the URDF range. Check its sign and offset in installation calibration.")
            : undefined} />}
        <Space wrap className="raw-arm__actions">
          <Button disabled={!model || busy || !available} onClick={() => {
            if (measuredTarget) seed(measuredTarget.q, measuredTarget.tcp); else if (model) seed(model.info.home, model.info.tcp);
          }}>{connected ? (zh ? "取实测位姿" : "Use measured pose") : (zh ? "取参考位姿" : "Use reference pose")}</Button>
          <Button type="primary" disabled={busy || !connected || !feedback || !preview || !modelReady || (mode !== "hold" && mode !== "monitor")}
            onClick={() => { if (preview) void action(() => rawArm.execute(preview.id, prefs.gains)); }}>{zh ? "执行预览轨迹" : "Execute previewed move"}</Button>
          <Button disabled={!connected || busy || mode !== "hold"} onClick={() => void action(() => rawArm.stop(prefs.limits))}>
            {zh ? "减速并保持" : "Brake and hold"}
          </Button>
        </Space>
        <h3>{zh ? "重力补偿" : "Gravity compensation"}</h3>
        <Space wrap>
          <Button disabled={!connected || busy} onClick={() => void action(() => control("hold"))}>{zh ? "保持当前位置" : "Hold current pose"}</Button>
          <Button disabled={!connected || !status || busy} type={mode === "float" ? "primary" : "default"}
            onClick={() => void action(() => control("float"))}>{zh ? "零重力拖动" : "Zero-gravity hand-guiding"}</Button>
          <Button danger disabled={!connected || busy || mode === "monitor"} onClick={() => void action(async () => {
            try { await rawArm.release(); setStatus(await rawArm.status()); } finally { setRevision(n => n + 1); }
          })}>{model?.info.tool ? (zh ? "机械臂与夹爪失能" : "Release arm and gripper") : (zh ? "手动失能" : "Release drives")}</Button>
        </Space>
        <p className="raw-arm__detail">{zh ? "位置控制持续施加重力前馈。拖动模式将位置刚度置零，保留重力前馈和可调阻尼。" :
          "Position control continuously applies gravity feedforward. Hand-guiding uses zero position stiffness, gravity feedforward and adjustable damping."}</p>
        {model?.info.tool && <RawArmGripper key={model.info.tool.kind} tool={model.info.tool}
          feedback={feedback?.tool ?? null} target={toolTarget} setTarget={setToolTarget}
          canMove={!busy && !!feedback?.tool && (mode === "hold" || mode === "float")} canDisable={connected && !busy}
          move={motion => void action(async () => {
            try { await rawArm.moveGripper(motion); } finally { setRevision(n => n + 1); }
          })} disable={() => void action(rawArm.disableGripper)} />}
      </Card>
    </div>
    <RawArmSettings prefs={prefs} change={setPrefs} connected={locked} toolKind={assembly === "bare" ? null : assembly} factory={factory}
      canAlign={!!feedback && mode === "monitor" && !busy}
      applyOffsets={offsets => void action(() => applyOffsets(offsets))}
      align={() => { if (feedback) void action(() => applyOffsets(prefs.installation.offset.map((o, i) => o + foldedQ[i] - feedback.q[i]))); }}
      gravityControls={<>
        <div className="raw-arm__fields raw-arm__gravity-scales">
          {gravityScale.map((value, i) => <label key={i}>J{i + 1}
            <InputNumber aria-label={`J${i + 1} ${zh ? "重力前馈补偿系数" : "gravity feedforward scale"}`}
              value={value} size="small" step={0.01} disabled={busy} status={isGravityScale(value) ? undefined : "error"}
              onChange={v => setGravityDraft(prev => replaceAt(prev ?? appliedScale, i, v))} />
            {connected && value !== appliedScale[i] && <span>{zh ? "已应用" : "Applied"} ×{appliedScale[i]}</span>}
          </label>)}
        </div>
        <Space wrap>
          <Button size="small" disabled={!connected || !status || busy || !validScale || !scaleChanged}
            onClick={() => void action(applyGravityScale)}>{zh ? "应用系数" : "Apply scales"}</Button>
          <Button size="small" disabled={busy || gravityScale.every(v => v === 1)} onClick={() => setGravityDraft(defaultGravityScale)}>
            {zh ? "全部填 1" : "Fill with 1"}</Button>
        </Space>
        <p>{zh ? "点击「应用系数」后，位置保持、轨迹运动、减速停止和拖动共用这组六轴系数。1 表示使用模型重力矩，0 表示关闭该轴的重力前馈。" :
          "Select Apply scales to use these values for holding, trajectory motion, braking and hand-guiding. A scale of 1 uses the model gravity torque. A scale of 0 disables gravity feedforward for that joint."}</p>
      </>} />
  </div>;
}
