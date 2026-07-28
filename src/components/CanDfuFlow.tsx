import { useEffect, useMemo, useRef, useState } from "react";
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  Descriptions,
  Divider,
  Input,
  Progress,
  Space,
  Tag,
  Typography,
} from "antd";
import {
  canDfuApi,
  dfuError,
  type CanDfuDevice,
  type CanDfuDiscoveryIssue,
  type CanDfuOutcome,
  type CanDfuPrepared,
  type CanDfuProgress,
  type CanDfuStage,
} from "../dfuApi";
import { useI18n, type Lang } from "../i18n";

const MAX_FRONTEND_FILE_SIZE = 2 * 1024 * 1024;
const DEFAULT_IFACE = navigator.userAgent.includes("Linux")
  ? "can0"
  : "gs_usb0";

export function CanDfuFlow({
  onBusyChange,
}: {
  onBusyChange: (busy: boolean) => void;
}) {
  const { lang } = useI18n();
  const { message, modal } = AntdApp.useApp();
  const copy = useMemo(() => textFor(lang), [lang]);
  const inputRef = useRef<HTMLInputElement>(null);
  const [spec, setSpec] = useState(DEFAULT_IFACE);
  const [discovering, setDiscovering] = useState(false);
  const [discovered, setDiscovered] = useState(false);
  const [devices, setDevices] = useState<CanDfuDevice[]>([]);
  const [issues, setIssues] = useState<CanDfuDiscoveryIssue[]>([]);
  const [selected, setSelected] = useState<CanDfuDevice | null>(null);
  const [selecting, setSelecting] = useState(false);
  const [preparing, setPreparing] = useState(false);
  const [prepared, setPrepared] = useState<CanDfuPrepared | null>(null);
  const [fileName, setFileName] = useState("");
  const [busy, setBusy] = useState(false);
  const [cancelRequested, setCancelRequested] = useState(false);
  const [progress, setProgress] = useState<CanDfuProgress | null>(null);
  const [outcome, setOutcome] = useState<CanDfuOutcome | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    onBusyChange(busy || discovering || selecting || preparing);
    return () => onBusyChange(false);
  }, [busy, discovering, onBusyChange, preparing, selecting]);

  useEffect(() => {
    return () => {
      void canDfuApi.leave().catch(() => {});
    };
  }, []);

  const resetArtifact = () => {
    setPrepared(null);
    setFileName("");
    setProgress(null);
    setOutcome(null);
    if (inputRef.current) inputRef.current.value = "";
  };

  const discover = async () => {
    const trimmed = spec.trim();
    if (!trimmed) {
      setError(copy.interfaceRequired);
      return;
    }
    setDiscovering(true);
    setDiscovered(false);
    setDevices([]);
    setIssues([]);
    setSelected(null);
    setError(null);
    resetArtifact();
    try {
      const found = await canDfuApi.discover(trimmed);
      setDevices(found.devices);
      setIssues(found.issues);
      setDiscovered(true);
      if (found.devices.length === 0 && found.issues.length === 0) {
        message.info(copy.noHeartbeat);
      }
    } catch (caught) {
      setError(dfuError(caught));
    } finally {
      setDiscovering(false);
    }
  };

  const selectDevice = async (device: CanDfuDevice) => {
    if (device.authorization !== "enabled" || busy || selecting || preparing) return;
    setSelecting(true);
    setError(null);
    resetArtifact();
    try {
      await canDfuApi.select(device.node_id);
      setSelected(device);
    } catch (caught) {
      setSelected(null);
      setError(dfuError(caught));
    } finally {
      setSelecting(false);
    }
  };

  const chooseFile = () => {
    if (!selected) {
      message.warning(copy.selectFirst);
      return;
    }
    inputRef.current?.click();
  };

  const fileSelected = async (file: File | undefined) => {
    if (!file) return;
    setError(null);
    setOutcome(null);
    setPrepared(null);
    setFileName(file.name);
    if (file.size > MAX_FRONTEND_FILE_SIZE) {
      setError(copy.fileTooLarge);
      return;
    }
    setPreparing(true);
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      const staged = await canDfuApi.prepare(bytes);
      setPrepared(staged);
      setSelected(staged.device);
      message.success(copy.validationPassed);
    } catch (caught) {
      setError(dfuError(caught));
    } finally {
      setPreparing(false);
      if (inputRef.current) inputRef.current.value = "";
    }
  };

  const runUpgrade = async () => {
    if (!prepared) return;
    setBusy(true);
    setCancelRequested(false);
    setProgress(null);
    setOutcome(null);
    setError(null);
    try {
      const result = await canDfuApi.start(prepared.token, setProgress);
      setProgress(null);
      setOutcome(result);
    } catch (caught) {
      setError(dfuError(caught));
    } finally {
      setBusy(false);
      setCancelRequested(false);
    }
  };

  const confirmUpgrade = () => {
    if (!prepared) return;
    const versionText =
      prepared.version_warning === "downgrade"
        ? copy.downgradeWarning
        : prepared.version_warning === "reinstall"
          ? copy.reinstallWarning
          : null;
    modal.confirm({
      title: copy.confirmTitle,
      content: (
        <Space direction="vertical">
          {versionText && (
            <Alert type="warning" showIcon message={versionText} />
          )}
          <Typography.Text>{copy.confirmBody}</Typography.Text>
        </Space>
      ),
      okText: copy.startUpgrade,
      cancelText: copy.goBack,
      okButtonProps: { danger: true },
      onOk: () => {
        void runUpgrade();
      },
    });
  };

  const requestCancel = async () => {
    try {
      const accepted = await canDfuApi.cancel();
      if (accepted) {
        setCancelRequested(true);
        message.info(copy.cancelQueued);
      }
    } catch (caught) {
      setError(dfuError(caught));
    }
  };

  const percent =
    progress && progress.total > 0
      ? Math.min(100, Math.round((progress.completed / progress.total) * 100))
      : 0;
  const hasEnabledTarget = devices.some(
    (device) => device.authorization === "enabled"
  );

  return (
    <div className="dfu-panel__grid">
      <Card className="dfu-card" title={copy.deviceStep}>
        <Space direction="vertical" size="middle" className="dfu-stack">
          <Alert
            type="warning"
            showIcon
            message={copy.writeLocked}
            description={copy.writeLockedDetail}
          />
          <Alert
            type="info"
            showIcon
            message={copy.classicCan}
            description={copy.discoverySafety}
          />
          <Space.Compact block>
            <Input
              value={spec}
              disabled={discovering || busy}
              placeholder="can0 / gs_usb0"
              onChange={(event) => setSpec(event.currentTarget.value)}
              onPressEnter={() => void discover()}
            />
            <Button
              type="primary"
              loading={discovering}
              disabled={busy}
              onClick={() => void discover()}
            >
              {discovering ? copy.discovering : copy.discover}
            </Button>
          </Space.Compact>
          {discovered && devices.length === 0 && issues.length === 0 && (
            <Alert type="warning" showIcon message={copy.noHeartbeat} />
          )}
          {issues.map((issue) => (
            <Alert
              key={issue.node_id}
              type="error"
              showIcon
              message={`${issue.node_id_hex} · ${copy.identityRejected}`}
              description={issue.reason}
            />
          ))}
          {devices.map((device) => (
            <DeviceCard
              key={device.node_id}
              device={device}
              selected={selected?.node_id === device.node_id}
              copy={copy}
              onSelect={() => void selectDevice(device)}
            />
          ))}
          <Alert
            type="warning"
            showIcon
            message={copy.hpmCanDisabled}
            description={copy.hpmCanDisabledDetail}
          />
        </Space>
      </Card>

      {hasEnabledTarget && (
        <>
          <Card className="dfu-card" title={copy.artifactStep}>
            <Space direction="vertical" size="middle" className="dfu-stack">
              <Alert
                type="warning"
                showIcon
                message={copy.remoteUnavailable}
                description={copy.localStillValidated}
              />
              <input
                ref={inputRef}
                className="dfu-panel__file-input"
                type="file"
                accept=".meowpkg"
                disabled={busy || preparing}
                onChange={(event) => {
                  void fileSelected(event.currentTarget.files?.[0]);
                }}
              />
              <Space wrap>
                <Button
                  loading={preparing}
                  disabled={!selected || busy || selecting}
                  onClick={chooseFile}
                >
                  {copy.chooseFile}
                </Button>
                {fileName && (
                  <Typography.Text className="dfu-panel__filename">
                    {fileName}
                  </Typography.Text>
                )}
              </Space>
              {prepared && <ArtifactDetails prepared={prepared} copy={copy} />}
            </Space>
          </Card>

          <Card className="dfu-card dfu-card--run" title={copy.upgradeStep}>
            <Space direction="vertical" size="middle" className="dfu-stack">
              <Typography.Paragraph type="secondary">
                {copy.destructiveHint}
              </Typography.Paragraph>
              <Space wrap>
                <Button
                  danger
                  type="primary"
                  disabled={!prepared || busy}
                  onClick={confirmUpgrade}
                >
                  {copy.startUpgrade}
                </Button>
                <Button
                  disabled={
                    !busy ||
                    cancelRequested ||
                    progress?.cancellable === false
                  }
                  onClick={() => void requestCancel()}
                >
                  {cancelRequested ? copy.cancelQueuedShort : copy.cancel}
                </Button>
              </Space>
              {(busy || progress) && (
                <div className="dfu-panel__progress">
                  <Progress
                    percent={percent}
                    status={cancelRequested ? "exception" : "active"}
                  />
                  <Typography.Text>
                    {progress
                      ? copy.stage[progress.stage]
                      : copy.stage.revalidating}
                  </Typography.Text>
                  {busy && progress?.cancellable === false && (
                    <Typography.Text type="secondary">
                      {copy.waitForCommand}
                    </Typography.Text>
                  )}
                </div>
              )}
              {outcome && <OutcomeAlert outcome={outcome} copy={copy} />}
            </Space>
          </Card>
        </>
      )}
      {error && (
        <Alert
          type="error"
          showIcon
          message={copy.upgradeError}
          description={error}
        />
      )}
    </div>
  );
}

function DeviceCard({
  device,
  selected,
  copy,
  onSelect,
}: {
  device: CanDfuDevice;
  selected: boolean;
  copy: Copy;
  onSelect: () => void;
}) {
  const enabled = device.authorization === "enabled";
  const color =
    device.authorization === "enabled"
      ? "green"
      : device.authorization === "known_disabled"
        ? "gold"
        : "default";
  return (
    <Card
      size="small"
      className={selected ? "dfu-target-card dfu-target-card--selected" : "dfu-target-card"}
    >
      <Space direction="vertical" className="dfu-stack">
        <Space wrap>
          <Tag color={color}>
            {device.authorization === "enabled"
              ? copy.authorized
              : device.authorization === "known_disabled"
                ? copy.knownDisabled
                : copy.unsupported}
          </Tag>
          <Typography.Text strong>
            {device.node_id_hex} · {device.display_name ?? device.device_name ?? copy.unknownName}
          </Typography.Text>
        </Space>
        <Descriptions size="small" column={1} className="dfu-descriptions">
          <Descriptions.Item label={copy.identity}>
            {device.vendor_id_hex} / {device.product_code_hex}
          </Descriptions.Item>
          <Descriptions.Item label={copy.serial}>
            {device.serial_number_hex}
          </Descriptions.Item>
          <Descriptions.Item label={copy.revisions}>
            SW {device.software_revision_hex} · HW{" "}
            {device.hardware_version_hex ?? copy.notRead}
          </Descriptions.Item>
        </Descriptions>
        <Typography.Text type="secondary">{device.reason}</Typography.Text>
        <Button
          type={selected ? "default" : "primary"}
          disabled={!enabled || selected}
          onClick={onSelect}
        >
          {selected ? copy.selected : copy.select}
        </Button>
      </Space>
    </Card>
  );
}

function ArtifactDetails({
  prepared,
  copy,
}: {
  prepared: CanDfuPrepared;
  copy: Copy;
}) {
  return (
    <>
      <Divider />
      <Space wrap>
        <Tag color="green">{copy.validationPassed}</Tag>
        <Tag>{prepared.mcu}</Tag>
        <Tag color={prepared.encrypted ? "purple" : "default"}>
          {prepared.encrypted ? copy.encrypted : copy.plaintext}
        </Tag>
      </Space>
      <Descriptions size="small" column={1} className="dfu-descriptions">
        <Descriptions.Item label={copy.fileSha}>
          <Typography.Text copyable={{ text: prepared.artifact_sha256 }}>
            {shortHash(prepared.artifact_sha256)}
          </Typography.Text>
        </Descriptions.Item>
        <Descriptions.Item label={copy.container}>
          v{prepared.format_version} · {prepared.firmware_id_hex}
        </Descriptions.Item>
        <Descriptions.Item label={copy.targetVersion}>
          {prepared.firmware_version_hex}
        </Descriptions.Item>
        <Descriptions.Item label={copy.sizes}>
          {formatBytes(prepared.artifact_size)} ·{" "}
          {formatBytes(prepared.plaintext_size)} →{" "}
          {formatBytes(prepared.wire_size)}
        </Descriptions.Item>
      </Descriptions>
      {prepared.encrypted && (
        <Alert
          type="info"
          showIcon
          message={copy.deviceFinalAuth}
          description={copy.deviceFinalAuthDetail}
        />
      )}
    </>
  );
}

function OutcomeAlert({
  outcome,
  copy,
}: {
  outcome: CanDfuOutcome;
  copy: Copy;
}) {
  if (outcome.status === "application_verified") {
    return (
      <Alert
        className="dfu-panel__outcome--success"
        type="warning"
        showIcon
        message={copy.applicationVerified}
        description={copy.applicationVerifiedDetail}
      />
    );
  }
  return (
    <Alert
      type="warning"
      showIcon
      message={copy.cancelled}
      description={
        outcome.status === "cancelled_before_write"
          ? copy.cancelledSafe
          : copy.cancelledRecoverable
      }
    />
  );
}

function shortHash(hash: string): string {
  return `${hash.slice(0, 12)}…${hash.slice(-12)}`;
}

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(2)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

type Copy = ReturnType<typeof textFor>;

function textFor(lang: Lang) {
  if (lang === "zh") {
    return {
      interfaceRequired: "请输入 CAN 接口。",
      deviceStep: "1 · 发现与授权设备",
      classicCan:
        "gs_usb 会配置为 Classic CAN 1 Mbit/s；SocketCAN 接口需由系统预先配置为 1 Mbit/s",
      discoverySafety:
        "只被动收集心跳节点，再严格读取完整 0x1018。未知产品不会读取升级私有对象，也不会收到任何 SDO 写入。",
      writeLocked: "此版本尚未启用 STM32 CAN 写入",
      writeLockedDetail:
        "当前只提供被动发现、严格身份校验和本地 profile 分类。三个已知产品均保持锁定，因此不会读取 0x2102，也不会发送升级写命令。",
      discover: "发现设备",
      discovering: "监听心跳…",
      noHeartbeat: "监听窗口内没有发现 CANopen 心跳节点。",
      identityRejected: "0x1018 身份读取被拒绝",
      hpmCanDisabled: "HPM CAN 仍未启用",
      hpmCanDisabledDetail:
        "HPM 当前只有 USB 升级经过真机验证；CAN 设计不会被此 STM32 后端推断或复用。",
      artifactStep: "2 · 校验 .meowpkg",
      remoteUnavailable: "制品校验等待首个合格产品 profile",
      localStillValidated:
        "产品的硬件版本、MCU 和 firmware ID 映射冻结后才会开放本地 .meowpkg；手动选择也不会跳过任何合法性检查。",
      selectFirst: "请先选择一个已授权的设备。",
      chooseFile: "选择 .meowpkg",
      fileTooLarge: "文件超过 2 MiB 的硬上限。",
      validationPassed: "全部前置校验已通过",
      upgradeStep: "3 · 写入与启动确认",
      destructiveHint:
        "公共写入引擎已接入安全门，但此构建没有任何合格产品 profile，因此不会发送 0x1F51。只有精确身份重新核对且制品通过校验后，状态机才可能解锁。",
      confirmTitle: "确认通过 CAN 升级？",
      confirmBody:
        "后端会重新监听同一节点并核对 vendor/product/serial/SW/HW，然后进入 Bootloader、写 header、清除、传输并启动。最终必须读回同一身份和目标 SW revision。",
      downgradeWarning: "目标版本低于当前版本；防回滚尚未启用，这是一次显式降级。",
      reinstallWarning: "目标版本与当前版本相同；这将重装同一版本。",
      startUpgrade: "开始升级",
      goBack: "返回检查",
      cancel: "取消升级",
      cancelQueued: "已请求取消；将在当前协议命令结束后停止。",
      cancelQueuedShort: "等待安全停止…",
      waitForCommand: "当前命令不可中断，正在等待设备 ACK。",
      upgradeError: "升级未完成",
      authorized: "已授权",
      knownDisabled: "已知但未启用",
      unsupported: "不支持",
      unknownName: "未命名设备",
      identity: "Vendor / Product",
      serial: "Serial",
      revisions: "版本",
      notRead: "未读取（未授权产品）",
      select: "选择设备",
      selected: "已选择",
      encrypted: "加密传输",
      plaintext: "明文 / 开发",
      fileSha: "包 SHA-256",
      container: "Container / Firmware ID",
      targetVersion: "目标 SW revision",
      sizes: "包 · 明文 → CAN wire",
      deviceFinalAuth: "逐记录认证由设备最终执行",
      deviceFinalAuthDetail:
        "GUI 不持有 AES key，无法解密固件。header 和包结构会在擦除前校验；每个 GCM record 仍由 Bootloader 验证，失败时设备留在 Bootloader 可恢复。",
      applicationVerified: "升级成功，应用身份已确认",
      applicationVerifiedDetail:
        "设备已以相同 vendor/product/serial/hardware 和目标 software revision 重新响应。",
      cancelled: "升级已停止",
      cancelledSafe: "取消发生在第一条升级写入前，设备 Flash 未被本次升级修改。",
      cancelledRecoverable:
        "设备应保留在 Bootloader。请保持供电并重新执行一次完整升级。",
      stage: {
        revalidating: "重新发现并绑定同一设备",
        entering_bootloader: "请求进入并认领 Bootloader",
        writing_header: "写入并验证 container header",
        clearing: "准备/擦除应用区域",
        writing: "传输固件",
        verifying_and_starting: "设备验签、提交并启动",
        confirming_application: "读取完整应用身份与目标版本",
      } satisfies Record<CanDfuStage, string>,
    };
  }
  return {
    interfaceRequired: "Enter a CAN interface.",
    deviceStep: "1 · Discover and authorize device",
    classicCan:
      "gs_usb is configured for Classic CAN at 1 Mbit/s; SocketCAN must already be configured by the system at 1 Mbit/s",
    discoverySafety:
      "The updater passively collects heartbeat nodes, then reads the complete 0x1018 identity strictly. An unknown product receives no proprietary-update reads and no SDO write.",
    writeLocked: "STM32 CAN writes are not enabled in this build",
    writeLockedDetail:
      "This milestone provides passive discovery, strict identity checks and local profile classification only. All three known products remain locked, so 0x2102 is not read and no update write is sent.",
    discover: "Discover devices",
    discovering: "Listening for heartbeats…",
    noHeartbeat: "No CANopen heartbeat node appeared during the discovery window.",
    identityRejected: "0x1018 identity rejected",
    hpmCanDisabled: "HPM over CAN remains disabled",
    hpmCanDisabledDetail:
      "Only HPM USB has hardware evidence. This STM32 backend never infers or reuses the untested HPM CAN design.",
    artifactStep: "2 · Validate .meowpkg",
    remoteUnavailable: "Artifact validation awaits the first qualified product profile",
    localStillValidated:
      "Local .meowpkg selection unlocks only after the product's hardware, MCU and firmware-ID mapping is frozen. Manual selection will not bypass validation.",
    selectFirst: "Select an authorized device first.",
    chooseFile: "Choose .meowpkg",
    fileTooLarge: "The file exceeds the 2 MiB hard limit.",
    validationPassed: "All preflight checks passed",
    upgradeStep: "3 · Write and confirm startup",
    destructiveHint:
      "The common write engine is wired behind the safety gate, but this build has no qualified product profile and therefore never sends 0x1F51. Fresh exact identity and artifact authorization are still mandatory before it can unlock.",
    confirmTitle: "Update over CAN?",
    confirmBody:
      "The backend will observe the same node again, bind vendor/product/serial/SW/HW, enter the Bootloader, write the header, clear, transfer and start. Success requires the same identity and target SW revision to answer afterward.",
    downgradeWarning:
      "The target is older than the installed version. Anti-rollback is not enabled; this is an explicit downgrade.",
    reinstallWarning:
      "The target equals the installed version. This will reinstall the same version.",
    startUpgrade: "Start upgrade",
    goBack: "Review",
    cancel: "Cancel upgrade",
    cancelQueued: "Cancellation requested; stopping after the current command returns.",
    cancelQueuedShort: "Waiting to stop safely…",
    waitForCommand: "The current command cannot be interrupted; waiting for its ACK.",
    upgradeError: "Upgrade did not complete",
    authorized: "Authorized",
    knownDisabled: "Known, not enabled",
    unsupported: "Unsupported",
    unknownName: "Unnamed device",
    identity: "Vendor / Product",
    serial: "Serial",
    revisions: "Revisions",
    notRead: "not read (unauthorized product)",
    select: "Select device",
    selected: "Selected",
    encrypted: "Encrypted wire",
    plaintext: "Plaintext / development",
    fileSha: "Package SHA-256",
    container: "Container / Firmware ID",
    targetVersion: "Target SW revision",
    sizes: "Package · plaintext → CAN wire",
    deviceFinalAuth: "The device performs final per-record authentication",
    deviceFinalAuthDetail:
      "The GUI has no AES key and cannot decrypt firmware. It validates the header and package before erase; the Bootloader still authenticates every GCM record and remains recoverable on failure.",
    applicationVerified: "Update succeeded; application identity verified",
    applicationVerifiedDetail:
      "The device answered with the same vendor/product/serial/hardware and the target software revision.",
    cancelled: "Upgrade stopped",
    cancelledSafe:
      "Cancellation happened before the first update write; this run did not alter device Flash.",
    cancelledRecoverable:
      "The device should remain in Bootloader. Keep power applied and run one complete upgrade again.",
    stage: {
      revalidating: "Observe and bind the same device again",
      entering_bootloader: "Request and claim Bootloader",
      writing_header: "Write and validate container header",
      clearing: "Prepare / erase application region",
      writing: "Transfer firmware",
      verifying_and_starting: "Device verify, commit and start",
      confirming_application: "Read full application identity and target version",
    } satisfies Record<CanDfuStage, string>,
  };
}
