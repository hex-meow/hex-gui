import { useEffect, useRef, useState } from "react";
import { Alert, App, Button, Card, Descriptions, Progress, Space, Typography } from "antd";
import { useI18n } from "../i18n";
import { recoveryApi, type RecoveryHit, type RecoveryPrepared, type RecoveryProgress, type RecoverySelection } from "../recoveryApi";
export function MotorIapRecoveryFlow({ scope, spec, onBusyChange }: { scope: RecoverySelection; spec: string; onBusyChange: (b: boolean) => void }) {
  const { lang } = useI18n(); const zh = lang === "zh"; const { modal } = App.useApp();
  const input = useRef<HTMLInputElement>(null); const [busy, setBusy] = useState(false);
  const [hits, setHits] = useState<RecoveryHit[]>([]); const [node, setNode] = useState<number | null>(null);
  const [prepared, setPrepared] = useState<RecoveryPrepared | null>(null); const [progress, setProgress] = useState<RecoveryProgress | null>(null);
  const [error, setError] = useState<string | null>(null); const [result, setResult] = useState<string | null>(null);
  useEffect(() => { onBusyChange(busy); return () => onBusyChange(false); }, [busy, onBusyChange]);
  useEffect(() => () => { void recoveryApi.leave(scope.token).catch(() => {}); }, [scope.token]);
  const scan = async () => { setBusy(true); setError(null); setHits([]); setNode(null); setPrepared(null); setResult(null); setProgress(null);
    try { setHits(await recoveryApi.scan(spec)); } catch (e) { setError(String(e)); } finally { setBusy(false); } };
  const select = async (id: number) => { setBusy(true); setError(null); try { await recoveryApi.select(id); setNode(id); setPrepared(null); } catch (e) { setError(String(e)); } finally { setBusy(false); } };
  const file = async (f?: File) => { if (!f) return; setBusy(true); setError(null); setPrepared(null);
    try { if (f.size > 2 * 1024 * 1024) throw new Error(zh ? "文件超过 2 MiB" : "File exceeds 2 MiB"); setPrepared(await recoveryApi.prepare(new Uint8Array(await f.arrayBuffer()))); }
    catch (e) { setError(String(e)); } finally { setBusy(false); if (input.current) input.current.value = ""; } };
  const run = async () => { if (!prepared) return; setBusy(true); setError(null); setResult(null);
    try { setResult(await recoveryApi.start(prepared.token, setProgress)); } catch (e) { setError(String(e)); }
    finally { setBusy(false); setPrepared(null); setNode(null); setProgress(null); } };
  const start = () => { if (!prepared) return; modal.confirm({ title: zh ? "确认恢复并写入固件？" : "Write recovery firmware?",
    content: `${prepared.model} · Node-ID ${prepared.node_id} · SHA-256 ${prepared.sha256}`, okButtonProps: { danger: true }, onOk: () => { void run(); } }); };
  return <div className="dfu-panel__grid">
    <Card title={zh ? "1 · 捕获故障设备" : "1 · Capture failed device"} className="dfu-card"><Space direction="vertical" className="dfu-stack">
      <Typography.Text strong>{scope.profile.name} · {spec}</Typography.Text>
      <Alert type="warning" showIcon message={zh ? "先开始捕获，再给唯一的目标设备重新上电。" : "Start capture, then power-cycle the isolated target."}
        description={zh ? "恢复模式会主动发送 Enter-IAP。Node-ID 100/101 不参与探测；全 FF 或型号不匹配的应答禁止写入。" : "Recovery actively sends Enter-IAP. Node-IDs 100/101 are excluded; all-FF or mismatched identities cannot be written."} />
      <Button type="primary" loading={busy && hits.length === 0} disabled={busy} onClick={() => void scan()}>{zh ? "开始恢复捕获" : "Start recovery capture"}</Button>
      {hits.map(h => <Card key={h.node_id} size="small" title={`Node-ID ${h.node_id}`}><Typography.Paragraph>{h.reason}</Typography.Paragraph>
        <Button disabled={busy || !h.selectable} type={node === h.node_id ? "primary" : "default"} onClick={() => void select(h.node_id)}>{node === h.node_id ? (zh ? "已选中" : "Selected") : (zh ? "选择" : "Select")}</Button></Card>)}
    </Space></Card>
    <Card title={zh ? "2 · 固件文件" : "2 · Firmware file"} className="dfu-card"><Space direction="vertical" className="dfu-stack">
      <input ref={input} type="file" accept=".img" hidden onChange={e => void file(e.currentTarget.files?.[0])} />
      <Button disabled={node == null || busy} onClick={() => input.current?.click()}>{zh ? "选择匹配型号的 IMG" : "Choose the matching IMG"}</Button>
      {prepared && <Descriptions column={1} size="small"><Descriptions.Item label={zh ? "型号" : "Model"}>{prepared.model}</Descriptions.Item>
        <Descriptions.Item label="Device-ID">0x{prepared.device_id.toString(16)}</Descriptions.Item><Descriptions.Item label="Firmware-ID">0x{prepared.firmware_id.toString(16)}</Descriptions.Item>
        <Descriptions.Item label="SHA-256"><Typography.Text copyable>{prepared.sha256}</Typography.Text></Descriptions.Item><Descriptions.Item label={zh ? "写入字节" : "Wire bytes"}>{prepared.wire_bytes}</Descriptions.Item></Descriptions>}
    </Space></Card>
    <Card title={zh ? "3 · 恢复升级" : "3 · Recover firmware"} className="dfu-card"><Space direction="vertical" className="dfu-stack">
      <Space><Button type="primary" danger disabled={!prepared || busy} onClick={start}>{zh ? "开始恢复升级" : "Start recovery update"}</Button>
        <Button disabled={!busy || progress?.cancellable === false} onClick={() => void recoveryApi.cancel()}>{zh ? "取消" : "Cancel"}</Button></Space>
      {progress && <Progress percent={Math.round(progress.completed / Math.max(1, progress.total) * 100)} />}
      {result && <Alert type="warning" showIcon message={zh ? "已收到 Verify ACK，应用启动尚未确认" : "Verify ACK received; application startup is unconfirmed"} />}
      {error && <Alert type="error" showIcon message={error} />}
    </Space></Card>
  </div>;
}
