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
  const [fetchingLatest, setFetchingLatest] = useState(false);
  const [prepared, setPrepared] = useState<CanDfuPrepared | null>(null);
  const [fileName, setFileName] = useState("");
  const [busy, setBusy] = useState(false);
  const [cancelRequested, setCancelRequested] = useState(false);
  const [progress, setProgress] = useState<CanDfuProgress | null>(null);
  const [outcome, setOutcome] = useState<CanDfuOutcome | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [onlineError, setOnlineError] = useState<string | null>(null);

  useEffect(() => {
    onBusyChange(busy || discovering || selecting || preparing || fetchingLatest);
    return () => onBusyChange(false);
  }, [busy, discovering, fetchingLatest, onBusyChange, preparing, selecting]);

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
    setOnlineError(null);
    if (inputRef.current) inputRef.current.value = "";
  };

  const discover = async () => {
    if (
      discovering ||
      busy ||
      selecting ||
      preparing ||
      fetchingLatest
    ) return;
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

  const loadLatest = async () => {
    setFetchingLatest(true);
    setError(null);
    setOnlineError(null);
    setOutcome(null);
    setPrepared(null);
    setFileName("");
    try {
      const staged = await canDfuApi.prepareLatest();
      setPrepared(staged);
      setSelected(staged.device);
      message.success(copy.onlineValidationPassed);
    } catch (caught) {
      // An online-discovery/network failure must not revoke the selected local
      // authorization. The manual file path remains available below.
      setOnlineError(dfuError(caught));
    } finally {
      setFetchingLatest(false);
    }
  };

  const selectDevice = async (device: CanDfuDevice) => {
    if (
      device.authorization !== "enabled" ||
      busy ||
      selecting ||
      preparing ||
      fetchingLatest
    ) return;
    setSelecting(true);
    setError(null);
    resetArtifact();
    try {
      await canDfuApi.select(device.node_id);
      setSelected(device);
      await loadLatest();
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
    if (selected.backend !== "cobs_can_iap_v1") {
      inputRef.current?.click();
      return;
    }
    modal.confirm({
      title: copy.manualImgChooserTitle,
      content: (
        <Space direction="vertical">
          <Alert
            type="error"
            showIcon
            message={copy.manualImgChooserWarning}
            description={copy.manualImgChooserDetail}
          />
          <Typography.Text>{copy.manualImgFamilyLimit}</Typography.Text>
        </Space>
      ),
      okText: copy.continueToImgChooser,
      cancelText: copy.cancelManualSelection,
      okButtonProps: { danger: true },
      autoFocusButton: "cancel",
      onOk: () => inputRef.current?.click(),
    });
  };

  const fileSelected = async (file: File | undefined) => {
    if (!file) return;
    setError(null);
    setOnlineError(null);
    setOutcome(null);
    setPrepared(null);
    setFileName(file.name);
    setPreparing(true);
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      const staged = await canDfuApi.prepare(bytes);
      setPrepared(staged);
      setSelected(staged.device);
      message.success(
        staged.manual_risk_required
          ? copy.manualImgParsed
          : copy.validationPassed,
      );
    } catch (caught) {
      setError(dfuError(caught));
    } finally {
      setPreparing(false);
      if (inputRef.current) inputRef.current.value = "";
    }
  };

  const acknowledgeManualImg = () => {
    if (
      !prepared ||
      prepared.backend !== "cobs_can_iap_v1" ||
      !prepared.manual_risk_required ||
      prepared.manual_risk_acknowledged
    ) return;
    modal.confirm({
      title: copy.manualImgAcknowledgeTitle,
      content: (
        <Space direction="vertical">
          <Alert
            type="error"
            showIcon
            message={copy.manualImgResponsibility}
            description={copy.manualImgAcknowledgeDetail}
          />
          <Descriptions size="small" column={1} bordered>
            <Descriptions.Item label={copy.profileLabel}>
              {prepared.device.display_name ?? prepared.device.profile_id}
              {prepared.device.display_name && prepared.device.profile_id
                ? ` · ${prepared.device.profile_id}`
                : null}
            </Descriptions.Item>
            <Descriptions.Item label={copy.identity}>
              {prepared.device.vendor_id_hex} / {prepared.device.product_code_hex}
            </Descriptions.Item>
            <Descriptions.Item label={copy.revisions}>
              SW {prepared.device.software_revision_hex}
            </Descriptions.Item>
            <Descriptions.Item label={copy.serial}>
              {prepared.device.serial_number_hex}
            </Descriptions.Item>
            <Descriptions.Item label={copy.fileNameLabel}>
              {fileName}
            </Descriptions.Item>
            <Descriptions.Item label={copy.fileSha}>
              <Typography.Text copyable={{ text: prepared.artifact_sha256 }}>
                {prepared.artifact_sha256}
              </Typography.Text>
            </Descriptions.Item>
            <Descriptions.Item label={copy.imgHeader}>
              {prepared.img_device_id_hex} / {prepared.firmware_id_hex} /{" "}
              {prepared.firmware_version_hex} / {prepared.img_start_address_hex}–
              {prepared.img_end_address_hex} /{" "}
              {prepared.encrypted ? copy.encrypted : copy.plaintext} /{" "}
              {formatBytes(prepared.wire_size)}
            </Descriptions.Item>
          </Descriptions>
        </Space>
      ),
      okText: copy.acknowledgeManualRisk,
      cancelText: copy.goBack,
      okButtonProps: { danger: true },
      autoFocusButton: "cancel",
      onOk: async () => {
        setPreparing(true);
        setError(null);
        try {
          const acknowledged = await canDfuApi.acknowledgeManual(prepared.token);
          setPrepared(acknowledged);
          setSelected(acknowledged.device);
          message.success(copy.manualRiskAcknowledged);
        } catch (caught) {
          setError(dfuError(caught));
          throw caught;
        } finally {
          setPreparing(false);
        }
      },
    });
  };

  const runUpgrade = async () => {
    if (
      !prepared ||
      (prepared.manual_risk_required &&
        !prepared.manual_risk_acknowledged)
    ) return;
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
      prepared.version_warning === "reinstall" ? copy.reinstallWarning : null;
    modal.confirm({
      title: copy.confirmTitle,
      content: (
        <Space direction="vertical">
          {versionText && (
            <Alert type="warning" showIcon message={versionText} />
          )}
          <Typography.Text>
            {prepared.backend === "cobs_can_iap_v1"
              ? copy.compatibleConfirmBody
              : copy.confirmBody}
          </Typography.Text>
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
  const compatibleSelected = selected?.backend === "cobs_can_iap_v1";

  return (
    <div className="dfu-panel__grid">
      <Card className="dfu-card" title={copy.deviceStep}>
        <Space direction="vertical" size="middle" className="dfu-stack">
          <Alert
            type="warning"
            showIcon
            message={copy.profileStatus}
            description={copy.profileStatusDetail}
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
              disabled={
                discovering || busy || selecting || preparing || fetchingLatest
              }
              placeholder="can0 / gs_usb0"
              onChange={(event) => setSpec(event.currentTarget.value)}
              onPressEnter={() => void discover()}
            />
            <Button
              type="primary"
              loading={discovering}
              disabled={busy || selecting || preparing || fetchingLatest}
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
          <Card
            className="dfu-card"
            title={compatibleSelected ? copy.compatibleArtifactStep : copy.artifactStep}
          >
            <Space direction="vertical" size="middle" className="dfu-stack">
              <Alert
                type="info"
                showIcon
                message={copy.onlineSource}
                description={
                  compatibleSelected
                    ? copy.compatibleOnlineSourceDetail
                    : copy.onlineSourceDetail
                }
              />
              {onlineError && (
                <Alert
                  type="warning"
                  showIcon
                  message={copy.onlineUnavailable}
                  description={`${onlineError} ${copy.localFallback}`}
                />
              )}
              <input
                ref={inputRef}
                className="dfu-panel__file-input"
                type="file"
                accept={compatibleSelected ? ".img" : ".meowpkg"}
                disabled={busy || preparing || fetchingLatest}
                onChange={(event) => {
                  void fileSelected(event.currentTarget.files?.[0]);
                }}
              />
              <Space wrap>
                <Button
                  type="primary"
                  loading={fetchingLatest}
                  disabled={busy || selecting || preparing}
                  onClick={() => void loadLatest()}
                >
                  {fetchingLatest ? copy.fetchingLatest : copy.getLatest}
                </Button>
                <Button
                  danger={compatibleSelected}
                  loading={preparing}
                  disabled={!selected || busy || selecting || fetchingLatest}
                  onClick={chooseFile}
                >
                  {compatibleSelected
                    ? copy.chooseCompatibleFileAdvanced
                    : copy.chooseFileAdvanced}
                </Button>
                {fileName && (
                  <Typography.Text className="dfu-panel__filename">
                    {fileName}
                  </Typography.Text>
                )}
              </Space>
              {prepared && <ArtifactDetails prepared={prepared} copy={copy} />}
              {prepared?.manual_risk_required &&
                !prepared.manual_risk_acknowledged && (
                  <Alert
                    type="error"
                    showIcon
                    message={copy.manualImgAwaitingAcknowledgement}
                    description={copy.manualImgAwaitingAcknowledgementDetail}
                    action={
                      <Button
                        danger
                        size="small"
                        loading={preparing}
                        onClick={acknowledgeManualImg}
                      >
                        {copy.reviewAndAcknowledge}
                      </Button>
                    }
                  />
                )}
              {prepared?.manual_risk_required &&
                prepared.manual_risk_acknowledged && (
                  <Alert
                    type="warning"
                    showIcon
                    message={copy.manualRiskAcknowledged}
                    description={copy.manualRiskAcknowledgedDetail}
                  />
                )}
              {prepared?.factory_backup_required && (
                <Alert
                  type="warning"
                  showIcon
                  message={copy.factoryBackupRequired}
                  description={copy.factoryBackupRequiredDetail}
                />
              )}
            </Space>
          </Card>

          <Card className="dfu-card dfu-card--run" title={copy.upgradeStep}>
            <Space direction="vertical" size="middle" className="dfu-stack">
              <Typography.Paragraph type="secondary">
                {prepared?.backend === "cobs_can_iap_v1"
                  ? copy.compatibleDestructiveHint
                  : copy.destructiveHint}
              </Typography.Paragraph>
              <Space wrap>
                <Button
                  danger
                  type="primary"
                  disabled={
                    !prepared ||
                    busy ||
                    (prepared.manual_risk_required &&
                      !prepared.manual_risk_acknowledged)
                  }
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
          {device.backend && (
            <Tag color={device.backend === "cobs_can_iap_v1" ? "blue" : "cyan"}>
              {device.backend === "cobs_can_iap_v1"
                ? copy.compatibleBackend
                : copy.standardBackend}
            </Tag>
          )}
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
        <Tag
          color={
            prepared.manual_risk_required &&
            !prepared.manual_risk_acknowledged
              ? "gold"
              : "green"
          }
        >
          {prepared.manual_risk_required &&
          !prepared.manual_risk_acknowledged
            ? copy.manualImgParsed
            : copy.validationPassed}
        </Tag>
        <Tag>
          {prepared.backend === "cobs_can_iap_v1"
            ? copy.compatibleImg
            : prepared.mcu}
        </Tag>
        <Tag color={prepared.encrypted ? "purple" : "default"}>
          {prepared.encrypted ? copy.encrypted : copy.plaintext}
        </Tag>
      </Space>
      <Descriptions size="small" column={1} className="dfu-descriptions">
        <Descriptions.Item label={copy.artifactSource}>
          {prepared.artifact_source === "r2"
            ? prepared.release_version
              ? `${copy.onlineArtifact} · v${prepared.release_version}`
              : copy.onlineArtifact
            : copy.localArtifact}
        </Descriptions.Item>
        <Descriptions.Item label={copy.fileSha}>
          <Typography.Text copyable={{ text: prepared.artifact_sha256 }}>
            {prepared.manual_risk_required
              ? prepared.artifact_sha256
              : shortHash(prepared.artifact_sha256)}
          </Typography.Text>
        </Descriptions.Item>
        {prepared.backend === "cobs_can_iap_v1" && (
          <Descriptions.Item label={copy.selectedDeviceIdentity}>
            {prepared.device.vendor_id_hex} / {prepared.device.product_code_hex} /{" "}
            {prepared.device.software_revision_hex} / {prepared.device.serial_number_hex}
          </Descriptions.Item>
        )}
        <Descriptions.Item label={copy.container}>
          {prepared.format_version != null && `v${prepared.format_version} · `}
          {prepared.firmware_id_hex}
        </Descriptions.Item>
        <Descriptions.Item
          label={
            prepared.backend === "cobs_can_iap_v1"
              ? copy.rawTargetVersion
              : copy.targetVersion
          }
        >
          {prepared.firmware_version_hex}
        </Descriptions.Item>
        {prepared.backend === "cobs_can_iap_v1" &&
          prepared.expected_postflash_revision_hex && (
            <Descriptions.Item label={copy.httpsTargetRevision}>
              {prepared.expected_postflash_revision_hex}
            </Descriptions.Item>
          )}
        <Descriptions.Item label={copy.sizes}>
          {formatBytes(prepared.artifact_size)}
          {prepared.plaintext_size != null
            ? ` · ${formatBytes(prepared.plaintext_size)} → `
            : " → "}
          {formatBytes(prepared.wire_size)}
        </Descriptions.Item>
        {prepared.backend === "cobs_can_iap_v1" && (
          <Descriptions.Item label={copy.imgHeader}>
            {prepared.img_device_id_hex} / {prepared.firmware_id_hex} /{" "}
            {prepared.firmware_version_hex} / {prepared.img_start_address_hex}–
            {prepared.img_end_address_hex} /{" "}
            {prepared.encrypted ? copy.encrypted : copy.plaintext} / {formatBytes(prepared.wire_size)}
          </Descriptions.Item>
        )}
      </Descriptions>
      {prepared.encrypted && prepared.backend === "stm32_canopen" && (
        <Alert
          type="info"
          showIcon
          message={copy.deviceFinalAuth}
          description={copy.deviceFinalAuthDetail}
        />
      )}
      {prepared.backend === "cobs_can_iap_v1" && (
        <Alert
          type="warning"
          showIcon
          message={copy.compatibleDeviceFinalAuth}
          description={copy.compatibleDeviceFinalAuthDetail}
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
  if (outcome.status === "verify_acked_factory_data_recovery_required") {
    return (
      <Alert
        type="error"
        showIcon
        message={copy.factoryDataRecoveryRequired}
        description={
          <Space direction="vertical" size="small">
            <Typography.Text>{copy.factoryDataRecoveryRequiredDetail}</Typography.Text>
            <FactoryBackupReference outcome={outcome} copy={copy} required />
          </Space>
        }
      />
    );
  }
  if (outcome.status === "verify_acked_factory_data_preserved") {
    return (
      <Alert
        type="success"
        showIcon
        message={copy.factoryDataPreserved}
        description={
          <Space direction="vertical" size="small">
            <Typography.Text>{copy.factoryDataPreservedDetail}</Typography.Text>
            <FactoryBackupReference outcome={outcome} copy={copy} required />
          </Space>
        }
      />
    );
  }
  if (outcome.status === "verify_acked_startup_unconfirmed") {
    return (
      <Alert
        className="dfu-panel__outcome--success"
        type="warning"
        showIcon
        message={copy.compatibleTransferComplete}
        description={
          <Space direction="vertical" size="small">
            <Typography.Text>{copy.compatibleStartupUnconfirmed}</Typography.Text>
            <FactoryBackupReference outcome={outcome} copy={copy} />
          </Space>
        }
      />
    );
  }
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

function FactoryBackupReference({
  outcome,
  copy,
  required = false,
}: {
  outcome: CanDfuOutcome;
  copy: Copy;
  required?: boolean;
}) {
  if (!required && !outcome.factory_backup_path) return null;
  return (
    <Descriptions size="small" column={1} bordered>
      <Descriptions.Item label={copy.factoryBackupPath}>
        {outcome.factory_backup_path ? (
          <Typography.Text copyable={{ text: outcome.factory_backup_path }}>
            {outcome.factory_backup_path}
          </Typography.Text>
        ) : (
          <Typography.Text type="danger">
            {copy.factoryBackupPathUnavailable}
          </Typography.Text>
        )}
      </Descriptions.Item>
      {outcome.factory_backup_sha256 && (
        <Descriptions.Item label={copy.factoryBackupSha}>
          <Typography.Text copyable={{ text: outcome.factory_backup_sha256 }}>
            {outcome.factory_backup_sha256}
          </Typography.Text>
        </Descriptions.Item>
      )}
    </Descriptions>
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
      profileStatus: "CAN 后端按完整设备身份自动选择",
      profileStatusDetail:
        "只有身份矩阵中精确匹配完整 0x1018 的设备才会启用对应后端；自有设备只接受 .meowpkg，合作厂商设备只接受 IMG，未知或不精确匹配始终锁定。",
      discover: "发现设备",
      discovering: "监听心跳…",
      noHeartbeat: "监听窗口内没有发现 CANopen 心跳节点。",
      identityRejected: "0x1018 身份读取被拒绝",
      hpmCanDisabled: "HPM CAN 仍未启用",
      hpmCanDisabledDetail:
        "HPM 当前只有 USB 升级经过真机验证；CAN 设计不会从任何已启用后端推断或复用。",
      artifactStep: "2 · 校验 .meowpkg",
      compatibleArtifactStep: "2 · 获取并核对合作厂商 IMG",
      onlineSource: "线上 stable 发布",
      onlineSourceDetail:
        "选择标准设备后会默认从固定 R2 HTTPS 地址获取最新版本。下载包与手选包经过完全相同的设备、MCU、firmware-ID、P-256、公钥指纹、key ID、security epoch 与 encrypted-v2 校验。manifest 目标 revision 低于当前 0x1018:03 时会拒绝，等版本允许重装。",
      compatibleOnlineSourceDetail:
        "选择合作厂商设备后会按已知的完整 0x1018 身份，从固定 R2 HTTPS 地址获取该身份的最新 IMG。HTTPS release 的目标 revision 低于当前 0x1018:03 时，会在下载 IMG 前拒绝；等版本允许重刷。",
      onlineUnavailable: "线上版本不可用",
      localFallback: "设备选择仍然有效。如确有需要，可使用下方高级本地文件回退。",
      getLatest: "获取线上最新版本",
      fetchingLatest: "正在获取线上版本…",
      onlineValidationPassed: "线上版本已下载并准备完成",
      selectFirst: "请先选择一个已授权的设备。",
      chooseFileAdvanced: "高级：选择本地 .meowpkg",
      chooseCompatibleFileAdvanced: "高级：手动选择 IMG",
      validationPassed: "制品已准备",
      manualImgChooserTitle: "手动选择合作厂商 IMG？",
      manualImgChooserWarning: "错误固件可能使电机无法正常启动或需要专用工具恢复",
      manualImgChooserDetail:
        "线上发布是推荐路径。仅在你能从可信来源核对目标型号、固件族与文件校验和时，才应手动选择 IMG；误升级的恢复成本和后果由操作者承担。",
      manualImgFamilyLimit:
        "重要：IMG 头不认证 CiA402 / Meow 软件族。GUI 可以严格识别当前设备并解析 IMG，但无法仅凭本地 IMG 证明它属于你想要的固件族。",
      continueToImgChooser: "理解风险，继续选 IMG",
      cancelManualSelection: "取消（推荐）",
      manualImgParsed: "IMG 已解析，等待人工核对",
      manualImgAcknowledgeTitle: "最后核对本地 IMG",
      manualImgResponsibility: "这是一次仅对当前文件生效的风险确认",
      manualImgAcknowledgeDetail:
        "请逐项核对设备身份、完整 SHA-256 与 IMG 头摘要。身份和协议字段匹配不代表 GUI 能验证 CiA402 / Meow 软件族；点击继续表示你确认文件来源和用途，并承担误升级后果。",
      acknowledgeManualRisk: "信息无误，我承担风险",
      manualRiskAcknowledged: "已确认本地 IMG 风险",
      manualRiskAcknowledgedDetail:
        "确认仅绑定当前已解析文件和一次性 token；重新选择设备或文件后必须重新核对。",
      manualImgAwaitingAcknowledgement: "尚未授权写入此本地 IMG",
      manualImgAwaitingAcknowledgementDetail:
        "先查看上方完整 SHA-256、设备身份和 IMG 头摘要，再进行一次性风险确认。确认前无法开始升级。",
      reviewAndAcknowledge: "核对并确认风险",
      factoryBackupRequired: "升级前必须备份 0x4001 出厂校准数据",
      factoryBackupRequiredDetail:
        "这是 Meow Motor 的强制前置步骤。后端会在复位或任何固件写入前读取并原子保存 0x4001 相关出厂校准数据；备份或落盘校验失败时不会修改设备。",
      imgHeader: "IMG 头（Device / Firmware / Version / 地址 / 加密 / BIN）",
      selectedDeviceIdentity:
        "当前设备 0x1018（Vendor / Product / Revision / Serial）",
      upgradeStep: "3 · 写入与启动确认",
      destructiveHint:
        "开始前会重新核对同一设备的完整身份；上位机验证产品固定的 P-256 header 签名，Bootloader 再逐条认证 AES-GCM record。任何未知或不精确匹配都无法解锁写入。",
      compatibleDestructiveHint:
        "开始前会在同一 CAN 总线上重新核对完整 0x1018；Reset 后还必须从 Enter ACK 精确读回本地 profile 与 IMG 绑定的 device/firmware ID，之后才允许发送可能擦除 Flash 的 StartDownload。",
      confirmTitle: "确认通过 CAN 升级？",
      confirmBody:
        "后端会重新监听同一节点并核对 vendor/product/serial/SW/HW，然后进入 Bootloader、写 header、清除、传输并启动。最终必须读回同一身份和目标 SW revision。",
      compatibleConfirmBody:
        "后端会重新读取同一 node/vendor/product/revision/serial，再执行 Reset → Enter 身份核对 → StartDownload → 交替分段 → Final → Verify。破坏性请求的 ACK 不明确时不会盲目重发。",
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
      standardBackend: "标准 CANopen DFU",
      compatibleBackend: "兼容电机 CAN",
      unknownName: "未命名设备",
      identity: "Vendor / Product",
      profileLabel: "身份 profile",
      serial: "Serial",
      revisions: "版本",
      notRead: "未读取（未授权产品）",
      select: "选择设备",
      selected: "已选择",
      encrypted: "加密传输",
      plaintext: "明文 / 开发",
      fileSha: "包 SHA-256",
      fileNameLabel: "文件名",
      artifactSource: "来源",
      onlineArtifact: "R2 stable",
      localArtifact: "本地文件",
      container: "Container / Firmware ID",
      targetVersion: "目标 SW revision",
      rawTargetVersion: "目标版本（协议原始值）",
      httpsTargetRevision: "HTTPS release 目标 0x1018:03 revision",
      sizes: "包 · 明文 → CAN wire",
      compatibleImg: "兼容 IMG",
      deviceFinalAuth: "逐记录认证由设备最终执行",
      deviceFinalAuthDetail:
        "GUI 不持有 AES key，无法解密固件。header 和包结构会在擦除前校验；每个 GCM record 仍由 Bootloader 验证，失败时设备留在 Bootloader 可恢复。",
      compatibleDeviceFinalAuth: "最终签名、Hash 与 CRC 仍由设备执行",
      compatibleDeviceFinalAuthDetail:
        "GUI 会检查 IMG 结构、内部 SHA、与本地 profile 绑定的协议 ID、起始地址和加密标志，但不设置型号级 BIN 上限，也无法从本地 IMG 认证 CiA402 / Meow 软件族。当前没有此格式的主机验签公钥或明文 CRC 参数；设备会在 Start/Final/Verify 阶段执行最终校验。",
      applicationVerified: "升级成功，应用身份已确认",
      applicationVerifiedDetail:
        "设备已以相同 vendor/product/serial/hardware 和目标 software revision 重新响应。",
      compatibleTransferComplete: "升级流程完成，已收到 Verify ACK",
      compatibleStartupUnconfirmed:
        "此兼容协议无法统一确认应用已正常启动。请实际检查电机功能；异常时保持供电且不要盲目重试。当前测试后端会拒绝身份全为 0xFF 的恢复状态，需使用后续经真机确认的产品恢复流程。",
      factoryDataPreserved: "升级完成，0x4001 出厂校准数据保持一致",
      factoryDataPreservedDetail:
        "固件写入与 Verify 已完成；升级后读回的 0x4001 数据与升级前备份逐字节一致。请保留以下备份用于审计或后续恢复。",
      factoryDataRecoveryRequired: "必须人工恢复 0x4001，不能视为普通升级成功",
      factoryDataRecoveryRequiredDetail:
        "固件写入和 Verify 已经完成，但升级后身份不符合 HTTPS release、0x4001 与备份不同，或无法可靠读回。电机出厂校准数据可能已丢失；停止使用该电机，不要重复升级，并使用以下备份执行受控恢复。",
      factoryBackupPath: "0x4001 备份文件",
      factoryBackupSha: "备份 SHA-256",
      factoryBackupPathUnavailable: "后端未返回备份路径；请立即停止使用并保留现场",
      cancelled: "升级已停止",
      cancelledSafe: "取消发生在第一条升级写入前，设备 Flash 未被本次升级修改。",
      cancelledRecoverable:
        "设备应保留在 Bootloader。请保持供电且不要盲目重试；若无法再读取完整设备身份，当前测试后端不会继续写入。",
      stage: {
        revalidating: "重新发现并绑定同一设备",
        backing_up_factory_data: "读取并原子保存 0x4001 出厂校准数据",
        entering_bootloader: "请求进入并认领 Bootloader",
        writing_header: "写入并验证 container header",
        clearing: "准备/擦除应用区域",
        writing: "传输固件",
        verifying_and_starting: "设备验签、提交并启动",
        confirming_application: "读取完整应用身份与目标版本",
        resetting: "复位目标设备",
        entering_compatible_bootloader: "进入兼容 Bootloader",
        validating_compatible_identity: "核对 Enter-IAP 身份",
        starting_download: "设备校验 IMG 并准备应用区域",
        finalizing: "设备校验完整 IMG Hash",
        verifying: "设备校验 CRC 并请求启动",
        checking_factory_data: "读回并逐字节核对 0x4001 出厂校准数据",
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
    profileStatus: "The complete device identity selects the CAN backend",
    profileStatusDetail:
      "Only a complete 0x1018 identity exactly present in the identity matrix enables a backend. First-party devices accept only .meowpkg; partner devices accept only IMG. Unknown and inexact identities remain locked.",
    discover: "Discover devices",
    discovering: "Listening for heartbeats…",
    noHeartbeat: "No CANopen heartbeat node appeared during the discovery window.",
    identityRejected: "0x1018 identity rejected",
    hpmCanDisabled: "HPM over CAN remains disabled",
    hpmCanDisabledDetail:
      "Only HPM USB has hardware evidence. No enabled backend infers or reuses the untested HPM CAN design.",
    artifactStep: "2 · Validate .meowpkg",
    compatibleArtifactStep: "2 · Fetch and review partner IMG",
    onlineSource: "Online stable release",
    onlineSourceDetail:
      "Selecting a standard target automatically downloads the latest release from the fixed R2 HTTPS origin. Online and local packages pass the same device, MCU, firmware-ID, P-256 key, fingerprint, key-ID, security-epoch, and encrypted-v2 checks. A manifest target revision below the current 0x1018:03 is refused; an equal revision may be reinstalled.",
    compatibleOnlineSourceDetail:
      "Selecting a partner device fetches the latest IMG for its known complete 0x1018 identity from the fixed R2 HTTPS origin. An HTTPS release target revision below the current 0x1018:03 is refused before the IMG download; an equal revision may be reflashed.",
    onlineUnavailable: "The online release is unavailable",
    localFallback:
      "The device selection remains valid. If necessary, use the advanced local-file fallback below.",
    getLatest: "Get latest online release",
    fetchingLatest: "Fetching online release…",
    onlineValidationPassed: "Online release downloaded and prepared",
    selectFirst: "Select an authorized device first.",
    chooseFileAdvanced: "Advanced: choose local .meowpkg",
    chooseCompatibleFileAdvanced: "Advanced: choose IMG manually",
    validationPassed: "Artifact prepared",
    manualImgChooserTitle: "Choose a partner IMG manually?",
    manualImgChooserWarning:
      "Incorrect firmware may prevent the motor from starting or require dedicated recovery tools",
    manualImgChooserDetail:
      "The online release is recommended. Select an IMG manually only when you can verify its target model, firmware family, trusted source, and checksum. The operator accepts the recovery cost and consequences of a wrong update.",
    manualImgFamilyLimit:
      "Important: the IMG header does not authenticate the CiA402 / Meow software family. The GUI can strictly identify the current device and parse the IMG, but the local IMG alone cannot prove its intended firmware family.",
    continueToImgChooser: "Accept risk and choose IMG",
    cancelManualSelection: "Cancel (recommended)",
    manualImgParsed: "IMG parsed; operator review required",
    manualImgAcknowledgeTitle: "Final review of local IMG",
    manualImgResponsibility:
      "This risk acknowledgement applies only to the current file",
    manualImgAcknowledgeDetail:
      "Check the device identity, full SHA-256, and IMG header summary. Matching identity and protocol fields does not mean the GUI can verify the CiA402 / Meow software family. Continuing confirms that you trust the file source and purpose and accept the consequences of a wrong update.",
    acknowledgeManualRisk: "Details match; I accept the risk",
    manualRiskAcknowledged: "Local IMG risk acknowledged",
    manualRiskAcknowledgedDetail:
      "The acknowledgement is bound to this parsed file and one-use token. Selecting another device or file requires a new review.",
    manualImgAwaitingAcknowledgement:
      "This local IMG is not yet authorized for writing",
    manualImgAwaitingAcknowledgementDetail:
      "Review the full SHA-256, device identity, and IMG header summary above, then give one-use risk consent. The update cannot start before consent.",
    reviewAndAcknowledge: "Review and acknowledge risk",
    factoryBackupRequired:
      "A 0x4001 factory-calibration backup is required before update",
    factoryBackupRequiredDetail:
      "This is a mandatory Meow Motor precondition. Before reset or any firmware write, the backend reads and atomically saves the relevant 0x4001 factory-calibration data. A backup or persistence-verification failure leaves the device untouched.",
    imgHeader: "IMG header (Device / Firmware / Version / range / encryption / BIN)",
    selectedDeviceIdentity:
      "Current device 0x1018 (Vendor / Product / Revision / Serial)",
    upgradeStep: "3 · Write and confirm startup",
    destructiveHint:
      "The backend revalidates the same full device identity before writing. The host verifies the product-pinned P-256 header signature, then the Bootloader authenticates every AES-GCM record. Unknown or inexact matches cannot unlock writes.",
    compatibleDestructiveHint:
      "The backend rereads the complete 0x1018 identity on the same bus. After Reset, the Enter ACK must return the exact profile- and IMG-bound device/firmware IDs before StartDownload may erase Flash.",
    confirmTitle: "Update over CAN?",
    confirmBody:
      "The backend will observe the same node again, bind vendor/product/serial/SW/HW, enter the Bootloader, write the header, clear, transfer and start. Success requires the same identity and target SW revision to answer afterward.",
    compatibleConfirmBody:
      "The backend rereads the same node/vendor/product/revision/serial, then runs Reset → Enter identity check → StartDownload → alternating segments → Final → Verify. It never blindly retransmits a destructive request with an ambiguous ACK.",
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
    standardBackend: "Standard CANopen DFU",
    compatibleBackend: "Compatible motor CAN",
    unknownName: "Unnamed device",
    identity: "Vendor / Product",
    profileLabel: "Identity profile",
    serial: "Serial",
    revisions: "Revisions",
    notRead: "not read (unauthorized product)",
    select: "Select device",
    selected: "Selected",
    encrypted: "Encrypted wire",
    plaintext: "Plaintext / development",
    fileSha: "Package SHA-256",
    fileNameLabel: "File name",
    artifactSource: "Source",
    onlineArtifact: "R2 stable",
    localArtifact: "Local file",
    container: "Container / Firmware ID",
    targetVersion: "Target SW revision",
    rawTargetVersion: "Target version (raw protocol value)",
    httpsTargetRevision: "HTTPS release target 0x1018:03 revision",
    sizes: "Package · plaintext → CAN wire",
    compatibleImg: "Compatible IMG",
    deviceFinalAuth: "The device performs final per-record authentication",
    deviceFinalAuthDetail:
      "The GUI has no AES key and cannot decrypt firmware. It validates the header and package before erase; the Bootloader still authenticates every GCM record and remains recoverable on failure.",
    compatibleDeviceFinalAuth: "The device performs final signature, hash, and CRC checks",
    compatibleDeviceFinalAuthDetail:
      "The GUI checks IMG structure, internal SHA, profile-bound protocol IDs, start address, and encryption flag, but sets no model-specific BIN ceiling and cannot authenticate a local IMG's CiA402 / Meow software family. This format has no host verification key or plaintext CRC parameters; the device performs the final checks during Start, Final, and Verify.",
    applicationVerified: "Update succeeded; application identity verified",
    applicationVerifiedDetail:
      "The device answered with the same vendor/product/serial/hardware and the target software revision.",
    compatibleTransferComplete: "Update flow complete; Verify ACK received",
    compatibleStartupUnconfirmed:
      "This compatible protocol cannot confirm application health generically. Check actual motor operation. If it is abnormal, keep power applied and do not retry blindly. This test backend rejects an all-0xFF recovery identity until a product recovery path is hardware-qualified.",
    factoryDataPreserved:
      "Update complete; 0x4001 factory calibration is preserved",
    factoryDataPreservedDetail:
      "Firmware writing and Verify completed, and the post-update 0x4001 data matches the pre-update backup byte for byte. Retain this backup for audit or later recovery.",
    factoryDataRecoveryRequired:
      "Manual 0x4001 recovery required; this is not a normal update success",
    factoryDataRecoveryRequiredDetail:
      "Firmware writing and Verify completed, but the post-update identity does not match the HTTPS release, or 0x4001 differs from the backup or could not be read reliably. Factory calibration may be lost. Stop using this motor, do not repeat the update, and use the backup below for controlled recovery.",
    factoryBackupPath: "0x4001 backup file",
    factoryBackupSha: "Backup SHA-256",
    factoryBackupPathUnavailable:
      "No backup path was returned; stop using the motor and preserve its current state",
    cancelled: "Upgrade stopped",
    cancelledSafe:
      "Cancellation happened before the first update write; this run did not alter device Flash.",
    cancelledRecoverable:
      "The device should remain in Bootloader. Keep power applied and do not retry blindly; this test backend will not write if the complete device identity can no longer be read.",
    stage: {
      revalidating: "Observe and bind the same device again",
      backing_up_factory_data:
        "Read and atomically save 0x4001 factory calibration",
      entering_bootloader: "Request and claim Bootloader",
      writing_header: "Write and validate container header",
      clearing: "Prepare / erase application region",
      writing: "Transfer firmware",
      verifying_and_starting: "Device verify, commit and start",
      confirming_application: "Read full application identity and target version",
      resetting: "Reset target device",
      entering_compatible_bootloader: "Enter compatible Bootloader",
      validating_compatible_identity: "Validate Enter-IAP identity",
      starting_download: "Device validates IMG and prepares the application region",
      finalizing: "Device validates the complete IMG hash",
      verifying: "Device validates CRC and requests startup",
      checking_factory_data:
        "Read back and compare 0x4001 factory calibration byte for byte",
    } satisfies Record<CanDfuStage, string>,
  };
}
