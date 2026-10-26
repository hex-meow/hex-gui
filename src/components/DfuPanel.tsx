import { useEffect, useState } from "react";
import { App, Segmented, Space, Typography } from "antd";
import { listen } from "@tauri-apps/api/event";
import { useI18n } from "../i18n";
import { canDfuApi, hpmDfuApi } from "../dfuApi";
import { recoveryApi } from "../recoveryApi";
import { CanDfuFlow } from "./CanDfuFlow";
import { HpmDfuFlow } from "./HpmDfuFlow";
import "./DfuPanel.css";

export function DfuPanel({ onBusyChange }: { onBusyChange: (busy: boolean) => void }) {
  const { lang } = useI18n(); const { message } = App.useApp();
  const [transport, setTransport] = useState<"can" | "usb">("can");
  const [busy, setBusy] = useState(false);
  useEffect(() => { onBusyChange(busy); return () => onBusyChange(false); }, [busy, onBusyChange]);
  useEffect(() => { let off: (() => void) | undefined;
    void listen("dfu-close-blocked", () => message.warning(lang === "zh" ? "升级或捕获正在进行，请先取消并等待当前命令结束。" : "Cancel and wait for the active update/capture before closing.")).then(f => { off = f; });
    return () => off?.(); }, [lang, message]);
  const change = async (next: "can" | "usb") => {
    if (busy || next === transport) return;
    try { await hpmDfuApi.leave(); await canDfuApi.leave(); await recoveryApi.leave(); setTransport(next); }
    catch (error) { message.error(String(error)); }
  };
  return <div className="dfu-panel">
    <Space className="dfu-panel__intro" style={{ width: "100%", justifyContent: "space-between" }}>
      <div><Typography.Title level={3}>{lang === "zh" ? "设备升级" : "Firmware update"}</Typography.Title>
        <Typography.Paragraph type="secondary">{lang === "zh" ? "CAN 默认监听心跳并核对身份，选择已知设备后开始升级。故障设备通过独立的恢复入口处理。" : "CAN listens for heartbeats and verifies identity. Select a known device to update; use Recovery for failed devices."}</Typography.Paragraph></div>
      <Segmented value={transport} disabled={busy} options={[{label:"CAN",value:"can"},{label:"USB",value:"usb"}]} onChange={v => void change(v as "can" | "usb")} />
    </Space>
    {transport === "can" ? <CanDfuFlow onBusyChange={setBusy} /> : <HpmDfuFlow onBusyChange={setBusy} />}
  </div>;
}
