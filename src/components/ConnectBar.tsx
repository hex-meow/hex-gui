import { useState } from "react";
import { App, Button, Input, Space, Tag } from "antd";
import { api, errMsg } from "../api";
import { parseNid } from "../format";
import { useI18n } from "../i18n";

// Linux uses SocketCAN (`can0`); macOS/Windows have no SocketCAN, so they
// default to the cross-platform gs_usb / candleLight adapter on channel 0.
function defaultIface(): string {
  return /linux/i.test(navigator.userAgent) ? "can0" : "gs_usb0";
}

export function ConnectBar({
  connected,
  onChange,
  broadcastHeartbeat,
}: {
  connected: boolean;
  onChange: (connected: boolean) => void;
  broadcastHeartbeat: boolean;
}) {
  const { message } = App.useApp();
  const { t } = useI18n();
  const [iface, setIface] = useState(defaultIface);
  const [ourNid, setOurNid] = useState("0x10");
  const [busy, setBusy] = useState(false);

  const connect = async () => {
    setBusy(true);
    try {
      const nid = parseNid(ourNid);
      await api.connect(iface.trim(), nid, broadcastHeartbeat);
      onChange(true);
      message.success(`${t("connectedTo")} ${iface}`);
    } catch (e) {
      message.error(`${t("connectFailed")}: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const disconnect = async () => {
    setBusy(true);
    try {
      await api.disconnect();
      onChange(false);
      message.info(t("disconnectedMsg"));
    } catch (e) {
      message.error(`${t("disconnectFailed")}: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Space className="connect-bar" wrap size={[8, 8]}>
      <span className="connect-bar__label">{t("bus")}</span>
      <Input
        style={{ width: 110 }}
        value={iface}
        disabled={connected}
        onChange={(e) => setIface(e.target.value)}
        placeholder={defaultIface()}
      />
      <span className="connect-bar__label">{t("ourNid")}</span>
      <Input
        style={{ width: 80 }}
        value={ourNid}
        disabled={connected}
        onChange={(e) => setOurNid(e.target.value)}
        placeholder="0x10"
      />
      {connected ? (
        <Button danger loading={busy} onClick={disconnect}>
          {t("disconnect")}
        </Button>
      ) : (
        <Button type="primary" loading={busy} onClick={connect}>
          {t("connect")}
        </Button>
      )}
      <Tag color={connected ? "green" : "default"}>
        {connected ? t("zConnected") : t("zDisconnected")}
      </Tag>
    </Space>
  );
}
