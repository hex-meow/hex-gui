import { useEffect, useState } from "react";
import { Alert, Card, Modal, Select, Space, Typography } from "antd";
import { recoveryApi, type RecoveryProfile, type RecoverySelection } from "../recoveryApi";
import { useI18n } from "../i18n";
const families = [
  { id: "hpm", name: "HPM", image: "/recovery/hpm-evk.png", zh: "HPM V3 控制板 · 图为 HPM5300EVK", en: "HPM V3 controller · HPM5300EVK shown" },
  { id: "stm32", name: "STM32", image: "/recovery/stm32-board.svg", zh: "STM32 CANopen DFU · 控制板示意图", en: "STM32 CANopen DFU · board illustration" },
  { id: "motor_iap", name: "Motor IAP / HEX", image: "/recovery/hex-4310.jpg", zh: "HEX 电机 · 图为 HEX-4310，需核对具体型号", en: "HEX motor · HEX-4310 shown; check exact model" },
] as const;
export function CanRecoveryChooser({ open, onCancel, onBeforeEnter, onSelected }: {
  open: boolean; onCancel: () => void; onBeforeEnter: () => Promise<void>; onSelected: (s: RecoverySelection) => void;
}) {
  const { lang } = useI18n(); const zh = lang === "zh";
  const [profiles, setProfiles] = useState<RecoveryProfile[]>([]);
  const [family, setFamily] = useState<string | null>(null); const [model, setModel] = useState<string | null>(null);
  const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  useEffect(() => { if (!open) return; setFamily(null); setModel(null); setError(null);
    let live = true; void recoveryApi.profiles().then(p => { if (live) setProfiles(p); }).catch(e => { if (live) setError(String(e)); });
    return () => { live = false; }; }, [open]);
  const selected = profiles.find(p => p.id === model);
  const enter = async () => { if (!selected?.available) return; setBusy(true); setError(null);
    try { await onBeforeEnter(); const scope = await recoveryApi.begin(selected.id); onSelected(scope); }
    catch (e) { setError(String(e)); } finally { setBusy(false); } };
  return <Modal open={open} width={900} title={zh ? "选择需要恢复的设备" : "Choose the device to recover"}
    onCancel={onCancel} closable={!busy} maskClosable={!busy} confirmLoading={busy}
    okText={zh ? "确认设备，进入恢复模式" : "Confirm device and enter recovery"}
    okButtonProps={{ disabled: !selected?.available }} onOk={() => void enter()}>
    <Space direction="vertical" size="middle" style={{ width: "100%" }}>
      <Alert type="warning" showIcon message={zh ? "请核对实物与准确型号，仅连接待恢复设备。" : "Check the physical device and exact model; connect only the target."}
        description={zh ? "选择完成前不会发送 LSS 或 IAP 探测。Motor IAP 恢复会主动探测，无法核对原有 CANopen 序列号。" : "No LSS/IAP probes are sent before confirmation. Motor IAP recovery actively probes and cannot verify the original CANopen serial."} />
      <div className="dfu-recovery-choices">{families.map(f => <button key={f.id} type="button" disabled={busy} aria-pressed={family === f.id}
        className={`dfu-recovery-choice${family === f.id ? " dfu-recovery-choice--selected" : ""}`}
        onClick={() => { setFamily(f.id); setModel(null); }}>
        <img src={f.image} alt={zh ? f.zh : f.en} /><strong>{f.name}</strong><span>{zh ? f.zh : f.en}</span>
      </button>)}</div>
      {family && <Select aria-label={zh ? "具体设备型号" : "Exact device model"} style={{ width: "100%" }} value={model}
        placeholder={zh ? "请选择具体型号" : "Select the exact model"} disabled={busy} onChange={setModel}
        options={profiles.filter(p => p.family === family).map(p => ({ value: p.id, label: `${p.name}${p.available ? "" : ` · ${p.reason}`}`, disabled: !p.available }))} />}
      {selected && <Card size="small"><Typography.Text strong>{selected.name}</Typography.Text><Typography.Paragraph>{selected.description}</Typography.Paragraph></Card>}
      {error && <Alert type="error" message={error} />}
    </Space>
  </Modal>;
}
