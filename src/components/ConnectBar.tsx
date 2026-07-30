import { useEffect, useMemo, useState } from "react";
import { Alert, App, Button, Input, Select, Space, Tag, Typography } from "antd";
import { api, errMsg } from "../api";
import {
  formatBitrate,
  formatSamplePoint,
  isGsUsbSpec,
  validateDeviceProfiles,
  validateHostProfile,
  type DeviceProfileIssue,
  type HostProfileIssue,
} from "../canProfile";
import { parseNid } from "../format";
import { useI18n } from "../i18n";
import type { ConnectionInfo, MotorInfo } from "../types";

// Linux uses SocketCAN (`can0`); macOS/Windows have no SocketCAN, so they
// default to the cross-platform gs_usb / candleLight adapter on channel 0.
function defaultIface(): string {
  return /linux/i.test(navigator.userAgent) ? "can0" : "gs_usb0";
}

// Global CANopen allocation: the direct-CAN host is node 10 (`0x0A`).
const DEFAULT_HOST_NID = "0x0A";

export function ConnectBar({
  connected,
  onChange,
  broadcastHeartbeat,
  devices,
}: {
  connected: boolean;
  onChange: (connected: boolean) => void;
  broadcastHeartbeat: boolean;
  devices: MotorInfo[];
}) {
  const { message } = App.useApp();
  const { t } = useI18n();
  const [iface, setIface] = useState(defaultIface);
  const [ourNid, setOurNid] = useState(DEFAULT_HOST_NID);
  const [dataBitrate, setDataBitrate] = useState(5_000_000);
  const [connectionInfo, setConnectionInfo] = useState<ConnectionInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const gsUsb = isGsUsbSpec(iface);

  useEffect(() => {
    if (!connected) setConnectionInfo(null);
  }, [connected]);

  const hostIssues = useMemo(
    () => (connectionInfo ? validateHostProfile(connectionInfo) : []),
    [connectionInfo],
  );
  const deviceIssues = useMemo(
    () =>
      connectionInfo
        ? validateDeviceProfiles(connectionInfo, devices)
        : [],
    [connectionInfo, devices],
  );

  const connect = async () => {
    setBusy(true);
    try {
      const nid = parseNid(ourNid);
      const info = await api.connect(
        iface.trim(),
        dataBitrate,
        nid,
        broadcastHeartbeat,
      );
      setConnectionInfo(info);
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
      setConnectionInfo(null);
      onChange(false);
      message.info(t("disconnectedMsg"));
    } catch (e) {
      message.error(`${t("disconnectFailed")}: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{ width: "100%" }}>
      <Space className="connect-bar" wrap size={[8, 8]}>
        <span className="connect-bar__label">{t("bus")}</span>
        <Input
          style={{ width: 140 }}
          value={iface}
          disabled={connected}
          onChange={(e) => setIface(e.target.value)}
          placeholder={defaultIface()}
        />
        {gsUsb && (
          <>
            <span className="connect-bar__label">{t("dataRate")}</span>
            <Select
              style={{ width: 96 }}
              value={dataBitrate}
              disabled={connected}
              onChange={setDataBitrate}
              options={[
                { value: 1_000_000, label: "1 Mbit/s" },
                { value: 2_000_000, label: "2 Mbit/s" },
                { value: 4_000_000, label: "4 Mbit/s" },
                { value: 5_000_000, label: "5 Mbit/s" },
              ]}
            />
          </>
        )}
        <span className="connect-bar__label">{t("ourNid")}</span>
        <Input
          style={{ width: 80 }}
          value={ourNid}
          disabled={connected}
          onChange={(e) => setOurNid(e.target.value)}
          placeholder={DEFAULT_HOST_NID}
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
        {connectionInfo && <LinkProfileTags info={connectionInfo} />}
      </Space>

      {hostIssues.length > 0 && (
        <Alert
          style={{ marginTop: 8 }}
          type="warning"
          showIcon
          message={
            connectionInfo?.backend === "socketcan"
              ? t("socketCanProfileWarning")
              : t("hostCanProfileWarning")
          }
          description={
            <IssueList
              items={hostIssues.map((issue) => hostIssueText(issue, t))}
            />
          }
        />
      )}
      {deviceIssues.length > 0 && (
        <Alert
          style={{ marginTop: 8 }}
          type="error"
          showIcon
          message={t("deviceCanProfileError")}
          description={
            <IssueList
              items={deviceIssues.map((issue) => deviceIssueText(issue, t))}
            />
          }
        />
      )}
    </div>
  );
}

function LinkProfileTags({ info }: { info: ConnectionInfo }) {
  const fd =
    info.fd_enabled == null ? "FD ?" : info.fd_enabled ? "FD on" : "FD off";
  return (
    <Space size={4} wrap>
      <Tag>{info.backend}</Tag>
      <Tag color={info.fd_enabled ? "blue" : "default"}>{fd}</Tag>
      <Tag>
        N {formatBitrate(info.nominal?.bitrate)} ·{" "}
        {formatSamplePoint(info.nominal?.sample_point_per_mille)}
      </Tag>
      <Tag>
        D {formatBitrate(info.data?.bitrate)} ·{" "}
        {formatSamplePoint(info.data?.sample_point_per_mille)}
      </Tag>
    </Space>
  );
}

function IssueList({ items }: { items: string[] }) {
  return (
    <ul style={{ margin: "4px 0 0", paddingLeft: 20 }}>
      {items.map((item, index) => (
        <li key={`${index}:${item}`}>
          <Typography.Text>{item}</Typography.Text>
        </li>
      ))}
    </ul>
  );
}

type Translate = (key: Parameters<ReturnType<typeof useI18n>["t"]>[0]) => string;

function hostIssueText(issue: HostProfileIssue, t: Translate): string {
  switch (issue.code) {
    case "inspection_failed":
      return `${t("canInspectionFailed")}: ${issue.actual}`;
    case "fd_unknown":
      return t("canFdUnknown");
    case "fd_disabled":
      return t("canFdDisabled");
    case "nominal_unknown":
      return t("canNominalUnknown");
    case "data_unknown":
      return t("canDataUnknown");
    case "nominal_bitrate":
      return `${t("canNominalRate")}: ${formatIssueRate(issue.actual)} (${t("canExpected")} ${formatIssueRate(issue.expected)})`;
    case "data_bitrate":
      return `${t("canDataRate")}: ${formatIssueRate(issue.actual)} (${t("canExpected")} ${formatIssueRate(issue.expected)})`;
    case "nominal_sample_point":
      return `${t("canNominalSamplePoint")}: ${formatIssueSamplePoint(issue.actual)} (${t("canExpected")} ${formatIssueSamplePoint(issue.expected)})`;
    case "data_sample_point":
      return `${t("canDataSamplePoint")}: ${formatIssueSamplePoint(issue.actual)} (${t("canExpected")} ${formatIssueSamplePoint(issue.expected)})`;
  }
}

function deviceIssueText(issue: DeviceProfileIssue, t: Translate): string {
  const prefix = `0x${issue.nodeId.toString(16).toUpperCase().padStart(2, "0")} ${issue.name}`;
  switch (issue.code) {
    case "unsupported":
      return `${prefix}: ${t("canConfigUnsupported")}`;
    case "read_failed":
      return `${prefix}: ${t("canConfigReadFailed")}: ${issue.reason}`;
    case "nominal_bitrate":
      return `${prefix}: ${t("canNominalRate")} ${formatIssueRate(issue.actual)} (${t("canExpected")} ${formatIssueRate(issue.expected)})`;
    case "data_bitrate":
      return `${prefix}: ${t("canDataRate")} ${formatIssueRate(issue.actual)} (${t("canExpected")} ${formatIssueRate(issue.expected)})`;
    case "classic_on_fd":
      return `${prefix}: ${t("canClassicOnly")} (${t("canExpected")} ${formatIssueRate(issue.expected)})`;
  }
}

function formatIssueRate(value: number | string | undefined): string {
  return typeof value === "number" ? formatBitrate(value) : String(value ?? "?");
}

function formatIssueSamplePoint(value: number | string | undefined): string {
  return typeof value === "number"
    ? (value / 1000).toFixed(3)
    : String(value ?? "?");
}
