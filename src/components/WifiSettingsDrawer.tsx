import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Alert,
  App as AntdApp,
  Button,
  Descriptions,
  Divider,
  Drawer,
  Empty,
  Input,
  List,
  Popconfirm,
  Select,
  Space,
  Switch,
  Tag,
  Typography,
} from "antd";
import { api } from "../api";
import { useI18n } from "../i18n";
import type {
  WifiController,
  WifiJob,
  WifiSavedNetwork,
  WifiScanEntry,
  WifiStatus,
} from "../types";

const { Text } = Typography;

interface Props {
  open: boolean;
  connected: boolean;
  fallbackCids: string[];
  onClose: () => void;
}

function statusColor(state: string) {
  if (state === "connected") return "green";
  if (state === "associating") return "processing";
  if (state === "unavailable") return "red";
  return "default";
}

function jobColor(state: string) {
  if (state === "succeeded") return "green";
  if (state === "failed") return "red";
  return "processing";
}

export function WifiSettingsDrawer({ open, connected, fallbackCids, onClose }: Props) {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();
  const [controllers, setControllers] = useState<WifiController[]>([]);
  const [cid, setCid] = useState<string>();
  const [status, setStatus] = useState<WifiStatus>();
  const [saved, setSaved] = useState<WifiSavedNetwork[]>([]);
  const [scan, setScan] = useState<WifiScanEntry[]>([]);
  const [ssid, setSsid] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [country, setCountry] = useState("JP");
  const [hidden, setHidden] = useState(false);
  const [discovering, setDiscovering] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [applying, setApplying] = useState(false);
  const [job, setJob] = useState<WifiJob>();

  const uniqueFallbackCids = useMemo(
    () => [...new Set(fallbackCids)].sort(),
    [fallbackCids],
  );

  const refreshDetails = useCallback(async (controller: string) => {
    const [nextStatus, nextSaved] = await Promise.all([
      api.wifiStatus(controller),
      api.wifiNetworks(controller),
    ]);
    setStatus(nextStatus);
    setSaved(nextSaved);
  }, []);

  const discover = useCallback(async () => {
    if (!connected) return;
    setDiscovering(true);
    try {
      let found = await api.wifiDiscover();
      if (found.length === 0 && uniqueFallbackCids.length > 0) {
        const fallback = await Promise.allSettled(
          uniqueFallbackCids.map(async (fallbackCid) => ({
            cid: fallbackCid,
            status: await api.wifiStatus(fallbackCid),
          })),
        );
        found = fallback
          .filter((result): result is PromiseFulfilledResult<WifiController> => result.status === "fulfilled")
          .map((result) => result.value);
      }
      setControllers(found);
      const nextCid = found.some((controller) => controller.cid === cid)
        ? cid
        : found[0]?.cid;
      setCid(nextCid);
      if (nextCid) await refreshDetails(nextCid);
    } catch (error) {
      message.error(String(error));
    } finally {
      setDiscovering(false);
    }
  }, [cid, connected, message, refreshDetails, uniqueFallbackCids]);

  useEffect(() => {
    if (open) void discover();
  }, [open]); // Opening the otherwise hidden panel is the explicit discovery trigger.

  useEffect(() => {
    if (!open || !cid || !job || !["queued", "running"].includes(job.state)) return;
    let cancelled = false;
    let timer: number | undefined;
    let consecutiveErrors = 0;
    const poll = async () => {
      try {
        const next = await api.wifiJob(cid, job.job_id);
        if (cancelled) return;
        consecutiveErrors = 0;
        setJob(next);
        if (next.state === "succeeded") {
          message.success(t("wifiConfigured"));
          await refreshDetails(cid);
          return;
        } else if (next.state === "failed") {
          message.error(next.error_message || t("wifiJobFailed"));
          await refreshDetails(cid);
          return;
        }
      } catch (error) {
        consecutiveErrors += 1;
        if (consecutiveErrors >= 3) {
          if (!cancelled) message.error(String(error));
          return;
        }
      }
      if (!cancelled) timer = window.setTimeout(poll, 800);
    };
    timer = window.setTimeout(poll, 800);
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [cid, job?.job_id, job?.state, message, open, refreshDetails, t]);

  const selectController = async (nextCid: string) => {
    setCid(nextCid);
    setScan([]);
    setJob(undefined);
    try {
      await refreshDetails(nextCid);
    } catch (error) {
      message.error(String(error));
    }
  };

  const scanNetworks = async () => {
    if (!cid) return;
    setScanning(true);
    try {
      setScan(await api.wifiScan(cid));
    } catch (error) {
      message.error(String(error));
    } finally {
      setScanning(false);
    }
  };

  const apply = async () => {
    if (!cid) return;
    setApplying(true);
    const normalizedCountry = country.trim().toUpperCase() || null;
    try {
      await api.wifiValidate(cid, ssid, passphrase, hidden, normalizedCountry);
      const accepted = await api.wifiSet(
        cid,
        ssid,
        passphrase,
        hidden,
        normalizedCountry,
        status?.revision ?? null,
      );
      setPassphrase("");
      setJob(accepted);
    } catch (error) {
      message.error(String(error));
    } finally {
      setApplying(false);
    }
  };

  const forget = async (network: WifiSavedNetwork) => {
    if (!cid) return;
    try {
      setJob(await api.wifiForget(cid, network.ssid.hex, status?.revision ?? null));
    } catch (error) {
      message.error(String(error));
    }
  };

  const forgetAll = async () => {
    if (!cid) return;
    try {
      setJob(await api.wifiForgetAll(cid, status?.revision ?? null));
    } catch (error) {
      message.error(String(error));
    }
  };

  const close = () => {
    setPassphrase("");
    onClose();
  };

  const jobPending = job?.state === "queued" || job?.state === "running";
  const ssidBytes = new TextEncoder().encode(ssid).length;
  const passphraseBytes = new TextEncoder().encode(passphrase).length;
  const countryValid = country.trim() === "" || /^[A-Za-z]{2}$/.test(country.trim());
  const canApply =
    ssidBytes >= 1 &&
    ssidBytes <= 32 &&
    passphraseBytes >= 8 &&
    passphraseBytes <= 63 &&
    countryValid &&
    !jobPending;

  return (
    <Drawer title={t("wifiSettings")} width={520} open={open} onClose={close}>
      {!connected && <Alert type="warning" showIcon message={t("wifiNeedConnection")} />}
      {connected && (
        <Space direction="vertical" size="middle" style={{ width: "100%" }}>
          <Alert type="info" showIcon message={t("wifiWiredOnly")} />
          <Space.Compact style={{ width: "100%" }}>
            <Select
              style={{ flex: 1 }}
              placeholder={t("wifiController")}
              value={cid}
              options={controllers.map((controller) => ({
                value: controller.cid,
                label: controller.cid.replace(/^hexmeow\//, ""),
              }))}
              notFoundContent={t("wifiNoController")}
              onChange={(value) => void selectController(value)}
            />
            <Button loading={discovering} onClick={() => void discover()}>{t("wifiRefresh")}</Button>
          </Space.Compact>

          {cid && status && (
            <Descriptions size="small" bordered column={1}>
              <Descriptions.Item label={t("wifiStatus")}>
                <Tag color={statusColor(status.state)}>{status.state}</Tag>
              </Descriptions.Item>
              <Descriptions.Item label={t("wifiConnectedSsid")}>
                {status.connected?.display || "—"}
              </Descriptions.Item>
              <Descriptions.Item label={t("wifiRevision")}>{status.revision}</Descriptions.Item>
            </Descriptions>
          )}

          {cid && (
            <>
              <Button block loading={scanning} onClick={() => void scanNetworks()}>
                {scanning ? t("wifiScanning") : t("wifiScan")}
              </Button>
              {scan.length > 0 && (
                <List
                  size="small"
                  bordered
                  dataSource={scan}
                  renderItem={(network) => (
                    <List.Item
                      actions={[
                        <Button
                          key="use"
                          size="small"
                          disabled={network.security === "open"}
                          onClick={() => {
                            setSsid(network.ssid.display);
                            setHidden(false);
                          }}
                        >
                          {t("wifiSelectNetwork")}
                        </Button>,
                      ]}
                    >
                      <List.Item.Meta
                        title={network.ssid.display || `<${network.ssid.hex}>`}
                        description={
                          <Space wrap>
                            <span>{t("wifiSignal")}: {network.signal_dbm} dBm</span>
                            <Tag>{network.security}</Tag>
                            {network.security === "open" && <Text type="secondary">{t("wifiOpenUnsupported")}</Text>}
                          </Space>
                        }
                      />
                    </List.Item>
                  )}
                />
              )}

              <Divider orientation="left" plain>{t("wifiSaved")}</Divider>
              {saved.length === 0 ? (
                <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={t("wifiNoSaved")} />
              ) : (
                <List
                  size="small"
                  bordered
                  dataSource={saved}
                  renderItem={(network) => (
                    <List.Item
                      actions={[
                        <Popconfirm
                          key="forget"
                          title={t("wifiForget")}
                          onConfirm={() => void forget(network)}
                        >
                          <Button size="small" danger disabled={jobPending}>{t("wifiForget")}</Button>
                        </Popconfirm>,
                      ]}
                    >
                      <Space>
                        <span>{network.ssid.display || `<${network.ssid.hex}>`}</span>
                        {network.connected && <Tag color="green">{t("wifiConnected")}</Tag>}
                        {!network.enabled && <Tag>{t("wifiDisabled")}</Tag>}
                      </Space>
                    </List.Item>
                  )}
                />
              )}

              <Divider orientation="left" plain>{t("wifiSettings")}</Divider>
              <Space direction="vertical" style={{ width: "100%" }}>
                <label>
                  <Text>{t("wifiSsid")}</Text>
                  <Input value={ssid} maxLength={32} onChange={(event) => setSsid(event.target.value)} />
                </label>
                <label>
                  <Text>{t("wifiPassword")}</Text>
                  <Input.Password
                    value={passphrase}
                    maxLength={63}
                    placeholder={t("wifiPasswordHint")}
                    autoComplete="new-password"
                    onChange={(event) => setPassphrase(event.target.value)}
                  />
                </label>
                <Space wrap>
                  <label>
                  <Text>{t("wifiCountry")}</Text>
                    <Input
                      value={country}
                      style={{ width: 72, marginLeft: 8 }}
                      maxLength={2}
                      allowClear
                      onChange={(event) => setCountry(event.target.value.toUpperCase())}
                    />
                  </label>
                  <Space>
                    <Text>{t("wifiHidden")}</Text>
                    <Switch checked={hidden} onChange={setHidden} />
                  </Space>
                </Space>
                <Button
                  block
                  type="primary"
                  loading={applying || jobPending}
                  disabled={!canApply}
                  onClick={() => void apply()}
                >
                  {applying ? t("wifiApplying") : t("wifiApply")}
                </Button>
                <Popconfirm title={t("wifiForgetAll")} onConfirm={() => void forgetAll()}>
                  <Button block danger disabled={saved.length === 0 || jobPending}>{t("wifiForgetAll")}</Button>
                </Popconfirm>
              </Space>

              {job && (
                <Alert
                  type={job.state === "failed" ? "error" : job.state === "succeeded" ? "success" : "info"}
                  showIcon
                  message={
                    <Space>
                      <span>{t("wifiJob")}</span>
                      <Tag color={jobColor(job.state)}>{job.state}</Tag>
                      <Text code>{job.job_id}</Text>
                    </Space>
                  }
                  description={job.error_message || (job.revision != null ? `revision ${job.revision}` : undefined)}
                />
              )}
            </>
          )}
        </Space>
      )}
    </Drawer>
  );
}
