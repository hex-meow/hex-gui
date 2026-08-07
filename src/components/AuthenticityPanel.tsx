import { useEffect, useMemo, useState } from "react";
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  Checkbox,
  Descriptions,
  Empty,
  Space,
  Spin,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import type {
  AuthenticityDeviceView,
  AuthenticityOnlineStatus,
  AuthenticityTarget,
  MotorInfo,
} from "../types";
import "./AuthenticityPanel.css";

type ScanRecord = {
  inspection: AuthenticityDeviceView | null;
  online: AuthenticityOnlineStatus["status"] | "unavailable" | null;
  error: string | null;
};

type Props = {
  connected: boolean;
  devices: MotorInfo[];
};

const NETWORK_COOLDOWN_MS = 3_200;
const keyOf = (nodeId: number, epoch: number) => `${nodeId}:${epoch}`;
const targetOf = (device: MotorInfo): AuthenticityTarget => ({
  nodeId: device.node_id,
  sessionEpoch: device.session_epoch,
});

export function AuthenticityPanel({ connected, devices }: Props) {
  const { lang } = useI18n();
  const { message, modal } = AntdApp.useApp();
  const text = (en: string, zh: string) => (lang === "zh" ? zh : en);
  const candidates = useMemo(
    () =>
      devices
        .filter(
          (device) =>
            device.online &&
            device.identity != null &&
            (device.device_type === "meow_motor" || device.device_type === "lift"),
        )
        .sort((left, right) => left.node_id - right.node_id),
    [devices],
  );
  const ignoredCount = devices.filter(
    (device) =>
      device.online &&
      device.identity != null &&
      device.device_type !== "meow_motor" &&
      device.device_type !== "lift",
  ).length;
  const candidateSignature = candidates
    .map((device) => keyOf(device.node_id, device.session_epoch))
    .join(",");
  const [records, setRecords] = useState<Record<string, ScanRecord>>({});
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState<"scan" | "register" | null>(null);
  const [cooldownUntil, setCooldownUntil] = useState(0);
  const [clock, setClock] = useState(() => Date.now());
  const cooldownSeconds = Math.max(0, Math.ceil((cooldownUntil - clock) / 1_000));

  useEffect(() => {
    if (cooldownUntil <= Date.now()) return;
    setClock(Date.now());
    const timer = window.setInterval(() => {
      const now = Date.now();
      setClock(now);
      if (now >= cooldownUntil) window.clearInterval(timer);
    }, 200);
    return () => window.clearInterval(timer);
  }, [cooldownUntil]);

  const scan = async (snapshot: MotorInfo[]) => {
    if (snapshot.length === 0) {
      setRecords({});
      setSelected(new Set());
      return;
    }
    setBusy("scan");
    const next: Record<string, ScanRecord> = {};
    const eligible: AuthenticityTarget[] = [];
    try {
      for (const device of snapshot) {
        const key = keyOf(device.node_id, device.session_epoch);
        try {
          const inspection = await api.authenticityInspect(targetOf(device));
          next[key] = { inspection, online: null, error: null };
          if (inspection.registration_eligible) eligible.push(targetOf(device));
        } catch (error) {
          next[key] = { inspection: null, online: null, error: errMsg(error) };
        }
      }
      if (eligible.length > 0) {
        try {
          setCooldownUntil(Date.now() + NETWORK_COOLDOWN_MS);
          const statuses = await api.authenticityVerifyOnline(eligible);
          for (const status of statuses) {
            const key = keyOf(status.node_id, status.session_epoch);
            if (next[key]) next[key].online = status.status;
          }
        } catch (error) {
          const detail = errMsg(error);
          for (const target of eligible) {
            const key = keyOf(target.nodeId, target.sessionEpoch);
            if (next[key]) {
              next[key].online = "unavailable";
              next[key].error = detail;
            }
          }
        }
      }
      setRecords(next);
      setSelected(
        new Set(
          Object.entries(next)
            .filter(([, record]) => record.online === "issued_unregistered")
            .map(([key]) => key),
        ),
      );
    } finally {
      setBusy(null);
    }
  };

  useEffect(() => {
    if (!connected) {
      setRecords({});
      setSelected(new Set());
      return;
    }
    const snapshot = candidates;
    void scan(snapshot);
    // The stable signature intentionally represents physical heartbeat sessions.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connected, candidateSignature]);

  const selectedTargets = candidates
    .filter((device) => selected.has(keyOf(device.node_id, device.session_epoch)))
    .map(targetOf);

  const register = () => {
    if (selectedTargets.length === 0 || cooldownSeconds > 0) return;
    modal.confirm({
      title: text("First-register the selected device set?", "首次注册所选设备集合？"),
      content: text(
        "Registration is atomic and normally irreversible. It records only that these identities have been activated; no customer or order information is returned publicly.",
        "注册将作为一个原子事务提交，通常不可撤销。公开状态只记录这些身份已经首次启用，不会返回客户或订单信息。",
      ),
      okText: text("Register", "确认注册"),
      okButtonProps: { danger: false },
      cancelText: text("Cancel", "取消"),
      onOk: async () => {
        setBusy("register");
        try {
          setCooldownUntil(Date.now() + NETWORK_COOLDOWN_MS);
          const result = await api.authenticityRegister(selectedTargets);
          setRecords((previous) => {
            const next = { ...previous };
            for (const target of selectedTargets) {
              const key = keyOf(target.nodeId, target.sessionEpoch);
              if (next[key]) next[key] = { ...next[key], online: "registered", error: null };
            }
            return next;
          });
          setSelected(new Set());
          message.success(
            text(
              `${result.device_count} device(s) registered atomically`,
              `${result.device_count} 台设备已原子注册`,
            ),
          );
        } catch (error) {
          message.error(errMsg(error));
          throw error;
        } finally {
          setBusy(null);
        }
      },
    });
  };

  return (
    <div className="authenticity-panel">
      <section className="authenticity-hero">
        <div>
          <Typography.Text className="authenticity-kicker">
            {text("SOURCE PROOF · FIRST REGISTRATION", "来源证明 · 首次注册")}
          </Typography.Text>
          <Typography.Title level={3}>
            {text("Verify before commissioning", "投入使用前先验证")}
          </Typography.Title>
          <Typography.Paragraph>
            {text(
              "Known devices are discovered from their CANopen heartbeat. The app then re-reads the complete 0x1018 identity before touching any product-specific proof object.",
              "APP 根据 CANopen 心跳发现已知设备，并在读取任何产品专用证明对象前重新读取完整 0x1018 身份。",
            )}
          </Typography.Paragraph>
        </div>
        <Space>
          <Button
            onClick={() => void scan(candidates)}
            loading={busy === "scan"}
            disabled={!connected || busy === "register" || cooldownSeconds > 0}
          >
            {text("Re-read all", "重新读取全部")}
          </Button>
          <Button
            type="primary"
            onClick={register}
            loading={busy === "register"}
            disabled={
              !connected || selectedTargets.length === 0 || busy === "scan" || cooldownSeconds > 0
            }
          >
            {cooldownSeconds > 0
              ? text(`Network cooldown ${cooldownSeconds}s`, `网络冷却 ${cooldownSeconds} 秒`)
              : text(
                  `Register selected (${selectedTargets.length})`,
                  `注册所选设备（${selectedTargets.length}）`,
                )}
          </Button>
        </Space>
      </section>

      <div className="authenticity-boundaries">
        <Alert
          type="info"
          showIcon
          message={text("Two independent results", "两个相互独立的结果")}
          description={text(
            "P-256 devices can prove their source offline. Registration and revocation are online state. Meow Motor source verification requires the online issuance ledger.",
            "P-256 设备可以离线证明来源；注册与撤销属于在线状态。Meow Motor 的来源验证需要在线签发账本。",
          )}
        />
        <Alert
          type="warning"
          showIcon
          message={text("A valid proof is not an unclonable identity", "有效证明不是不可克隆身份")}
          description={text(
            "A complete UID and proof can still be copied. First-registration and the private shipment/customer ledger are the evidence used to detect reuse and decide support eligibility.",
            "完整 UID 与证明仍可能被复制。首次注册状态以及私有的发货/客户账本用于发现重复使用并判断售后资格。",
          )}
        />
      </div>

      {!connected ? (
        <Empty description={text("Connect a CAN bus to begin heartbeat discovery", "连接 CAN 总线后开始心跳发现")} />
      ) : candidates.length === 0 ? (
        <Empty description={text("Waiting for a known Meow Motor or signed hexmeow device heartbeat", "等待已知 Meow Motor 或带签名的 hexmeow 设备心跳")} />
      ) : (
        <Spin spinning={busy === "scan"} tip={text("Strictly reading proofs…", "正在严格读取证明…")}>
          <div className="authenticity-grid">
            {candidates.map((device) => {
              const key = keyOf(device.node_id, device.session_epoch);
              const record = records[key];
              return (
                <DeviceCard
                  key={key}
                  device={device}
                  record={record}
                  checked={selected.has(key)}
                  onChecked={(checked) =>
                    setSelected((previous) => {
                      const next = new Set(previous);
                      if (checked) next.add(key);
                      else next.delete(key);
                      return next;
                    })
                  }
                  text={text}
                />
              );
            })}
          </div>
        </Spin>
      )}

      {ignoredCount > 0 && (
        <Typography.Text type="secondary">
          {text(
            `${ignoredCount} other heartbeat-discovered node(s) were intentionally not probed for proprietary proof objects.`,
            `另有 ${ignoredCount} 个心跳节点未被尝试读取专用证明对象。`,
          )}
        </Typography.Text>
      )}
    </div>
  );
}

function DeviceCard({
  device,
  record,
  checked,
  onChecked,
  text,
}: {
  device: MotorInfo;
  record: ScanRecord | undefined;
  checked: boolean;
  onChecked: (checked: boolean) => void;
  text: (en: string, zh: string) => string;
}) {
  const inspection = record?.inspection;
  const canSelect = record?.online === "issued_unregistered";
  return (
    <Card
      className="authenticity-device-card"
      title={
        <div className="authenticity-device-title">
          <span>{inspection?.device_name ?? device.friendly_name}</span>
          <code>0x{device.node_id.toString(16).toUpperCase().padStart(2, "0")}</code>
        </div>
      }
      extra={
        <Checkbox checked={checked} disabled={!canSelect} onChange={(event) => onChecked(event.target.checked)}>
          {text("Register", "注册")}
        </Checkbox>
      }
    >
      <Space wrap className="authenticity-status-row">
        <StatusTag kind="local" status={inspection?.local_status ?? (record?.error ? "read_error" : "reading")} text={text} />
        <StatusTag kind="online" status={record?.online ?? "pending"} text={text} />
      </Space>
      {inspection && (
        <Descriptions size="small" column={1} colon={false}>
          <Descriptions.Item label="0x1018">
            <code>
              {hex32(inspection.identity.vendor_id)} · {hex32(inspection.identity.product_code)} · rev {hex32(inspection.identity.revision_number)} · UID {hex32(inspection.identity.serial_number)}
            </code>
          </Descriptions.Item>
          <Descriptions.Item label={text("Proof", "证明")}>{inspection.detail}</Descriptions.Item>
          {inspection.signing_key_id != null && (
            <Descriptions.Item label={text("Signing key", "签名密钥")}>key-id {inspection.signing_key_id}</Descriptions.Item>
          )}
          {inspection.digest_hex && (
            <Descriptions.Item label="SHA-256">
              <Typography.Text copyable={{ text: inspection.digest_hex }} className="authenticity-digest">
                {inspection.digest_hex}
              </Typography.Text>
            </Descriptions.Item>
          )}
        </Descriptions>
      )}
      {record?.error && <Alert className="authenticity-error" type="warning" showIcon message={record.error} />}
    </Card>
  );
}

function StatusTag({
  kind,
  status,
  text,
}: {
  kind: "local" | "online";
  status: string;
  text: (en: string, zh: string) => string;
}) {
  const labels: Record<string, [string, string, string]> = {
    valid: ["success", "Offline source proof valid", "离线来源证明有效"],
    envelope_valid: ["processing", "Local envelope valid", "本地 envelope 有效"],
    unsupported: ["default", "Proof unsupported", "不支持来源证明"],
    unprovisioned: ["warning", "Proof not provisioned", "尚未签发证明"],
    invalid: ["error", "Source proof invalid", "来源证明无效"],
    reading: ["processing", "Reading source proof", "正在读取来源证明"],
    read_error: ["warning", "Read unavailable", "读取暂不可用"],
    issued_unregistered: ["success", "Authentic · not registered", "正品 · 尚未注册"],
    registered: ["warning", "Already registered", "已经注册"],
    revoked: ["error", "Revoked", "已撤销"],
    unknown: ["error", "Unknown issuance", "未知签发记录"],
    unavailable: ["default", "Online status unavailable", "在线状态暂不可用"],
    pending: ["default", "Online status pending", "等待在线状态"],
  };
  const [color, en, zh] = labels[status] ?? ["default", status, status];
  return <Tag color={color}>{kind === "local" ? text(en, zh) : text(en, zh)}</Tag>;
}

function hex32(value: number) {
  return `0x${(value >>> 0).toString(16).toUpperCase().padStart(8, "0")}`;
}
