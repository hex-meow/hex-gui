import { useEffect, useMemo, useRef, useState } from "react";
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  Descriptions,
  Divider,
  Input,
  Checkbox,
  Progress,
  Space,
  Tag,
  Typography,
} from "antd";
import {
  dfuError,
  hpmDfuApi,
  type HpmDfuDevice,
  type HpmDfuOutcome,
  type HpmDfuPrepared,
  type HpmDfuProgress,
  type HpmDfuStage,
} from "../dfuApi";
import { useI18n } from "../i18n";
import { hpmCanCaptureOptions } from "../hpmV3";
import "./DfuPanel.css";


const MAX_FRONTEND_FILE_SIZE = 2 * 1024 * 1024;

export function HpmDfuFlow({ onBusyChange, mode = "usb", recoveryToken, recoverySpec, selectedNode }: {
  onBusyChange: (busy: boolean) => void;
  mode?: "usb" | "selected-can" | "recovery-can";
  recoveryToken?: string; recoverySpec?: string; selectedNode?: number;
}) {
  const { lang } = useI18n();
  const { message, modal } = AntdApp.useApp();
  const copy = useMemo(() => textFor(lang), [lang]);
  const inputRef = useRef<HTMLInputElement>(null);
  const transport = mode === "usb" ? "usb" : "hpm-can";
  const [canFields, setCanFields] = useState({
    spec: recoverySpec ?? (navigator.userAgent.includes("Linux") ? "can0" : "gs_usb0"),
    unconfigured: false, node: "31", assignedNode: "31",
    vendor: "0x6865786D", product: "0x45564B33", uid: "",
  });
  const [validating, setValidating] = useState(false);
  const [probing, setProbing] = useState(false);
  const [fetchingLatest, setFetchingLatest] = useState(false);
  const [device, setDevice] = useState<HpmDfuDevice | null>(null);
  const [prepared, setPrepared] = useState<HpmDfuPrepared | null>(null);
  const [fileName, setFileName] = useState("");
  const [busy, setBusy] = useState(false);
  const [cancelRequested, setCancelRequested] = useState(false);
  const [progress, setProgress] = useState<HpmDfuProgress | null>(null);
  const [outcome, setOutcome] = useState<HpmDfuOutcome | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [onlineError, setOnlineError] = useState<string | null>(null);

  useEffect(() => {
    onBusyChange(busy || probing || fetchingLatest || validating);
    return () => onBusyChange(false);
  }, [busy, fetchingLatest, onBusyChange, probing, validating]);

  useEffect(() => () => { void hpmDfuApi.leave().catch(() => {}); }, []);

  const resetArtifact = () => {
    setPrepared(null);
    setFileName("");
    setProgress(null);
    setOutcome(null);
    setOnlineError(null);
    if (inputRef.current) inputRef.current.value = "";
  };

  const prepareLatest = async () => {
    setFetchingLatest(true);
    setError(null);
    setOnlineError(null);
    setOutcome(null);
    setProgress(null);
    setPrepared(null);
    setFileName("");
    if (inputRef.current) inputRef.current.value = "";
    try {
      const staged = await hpmDfuApi.prepareLatest();
      setPrepared(staged);
      setDevice(staged.device);
      message.success(copy.latestReady);
    } catch (caught) {
      setOnlineError(dfuError(caught));
    } finally {
      setFetchingLatest(false);
    }
  };

  const probe = async () => {
    setProbing(true);
    setError(null);
    setDevice(null);
    resetArtifact();
    try {
      setCancelRequested(false);
      const found = mode === "selected-can" ? await hpmDfuApi.captureSelectedCan()
        : mode === "recovery-can" ? await hpmDfuApi.captureCan({ ...hpmCanCaptureOptions(canFields), recoveryToken })
        : await hpmDfuApi.probe();
      setDevice(found);
      message.success(copy.deviceRecognized);
      if (found.security_mode === "production_confidential") {
        await prepareLatest();
      } else {
        setOnlineError(found.protocol_version === 3 ? copy.v3LocalOnly : copy.developmentLocalOnly);
      }
    } catch (caught) {
      setError(dfuError(caught));
    } finally {
      setProbing(false);
    }
  };

  const chooseFile = () => {
    if (!device) {
      message.warning(copy.probeFirst);
      return;
    }
    inputRef.current?.click();
  };

  const fileSelected = async (file: File | undefined) => {
    if (!file) return;
    setError(null);
    setOnlineError(null);
    setOutcome(null);
    setPrepared(null);
    setFileName(file.name);
    if (file.size > MAX_FRONTEND_FILE_SIZE) {
      setError(copy.fileTooLarge);
      return;
    }
    setValidating(true);
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      const staged = await hpmDfuApi.prepare(bytes);
      setPrepared(staged);
      setDevice(staged.device);
      message.success(copy.validationPassed);
    } catch (caught) {
      setError(dfuError(caught));
    } finally {
      setValidating(false);
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
      const result = await hpmDfuApi.start(prepared.token, setProgress);
      setProgress(null);
      setOutcome(result);
    } catch (caught) {
      setError(dfuError(caught));
    } finally {
      setBusy(false);
      setCancelRequested(false);
      setPrepared(null);
      setDevice(null);
    }
  };

  const confirmUpgrade = () => {
    if (!prepared) return;
    modal.confirm({
      title: copy.confirmTitle,
      content: prepared.device.protocol_version === 3 ? copy.v3ConfirmBody : copy.confirmBody,
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
      const accepted = await hpmDfuApi.cancel();
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

  return (<div className="dfu-panel">
        <div className="dfu-panel__grid">
          <Card className="dfu-card" title={copy.deviceStep}>
            <Space direction="vertical" size="middle" className="dfu-stack">
              <Alert
                type="info"
                showIcon
                message={transport === "hpm-can" ? copy.canCaptureHint : copy.bootloaderHint}
              />
              {mode === "selected-can" && <Typography.Text>Node-ID {selectedNode} · {lang === "zh" ? "将重新核对已选设备的完整 0x1018；如已离线，请在开始等待后重新上电。" : "Revalidate the selected full 0x1018; power-cycle after waiting if offline."}</Typography.Text>}
              {mode === "recovery-can" && (
                <Space direction="vertical" className="dfu-stack">
                  <label>{copy.canInterface}<Input aria-label={copy.canInterface} value={canFields.spec} disabled={!!device || probing || busy}
                    onChange={e => setCanFields({ ...canFields, spec: e.target.value })} /></label>
                  <Checkbox checked={canFields.unconfigured} disabled={!!device || probing || busy}
                    onChange={e => setCanFields({ ...canFields, unconfigured: e.target.checked })}>{copy.lssAssign}</Checkbox>
                  <label>{canFields.unconfigured ? copy.freeNode : "Node-ID"}<Input value={canFields.unconfigured ? canFields.assignedNode : canFields.node} disabled={!!device || probing || busy}
                    onChange={e => setCanFields({ ...canFields, [canFields.unconfigured ? "assignedNode" : "node"]: e.target.value })} /></label>
                  {canFields.unconfigured && <>
                    <label>LSS Vendor-ID<Input value={canFields.vendor} disabled={!!device || probing || busy} onChange={e => setCanFields({ ...canFields, vendor: e.target.value })} /></label>
                    <label>LSS Product-Code (EVK3: 0x45564B33)<Input value={canFields.product} disabled={!!device || probing || busy} onChange={e => setCanFields({ ...canFields, product: e.target.value })} /></label>
                  </>}
                  <label>{copy.optionalUid}<Input value={canFields.uid} disabled={!!device || probing || busy} onChange={e => setCanFields({ ...canFields, uid: e.target.value })} /></label>
                </Space>
              )}
              <Button
                type="primary"
                loading={probing || fetchingLatest}
                disabled={busy || fetchingLatest || validating}
                onClick={() => void probe()}
              >
                {fetchingLatest
                  ? copy.downloadingLatest
                  : probing
                    ? copy.probing
                    : transport === "hpm-can" ? copy.captureCan : copy.probe}
              </Button>
              {device && <>
                {device.protocol_version === 3 && <Alert type="success" showIcon message={copy.waitLatched} />}
                <DeviceDetails device={device} copy={copy} />
                <Button disabled={busy || probing} onClick={() => void hpmDfuApi.leave().then(() => { setDevice(null); resetArtifact(); }).catch(e => setError(dfuError(e)))}>{copy.releaseCapture}</Button>
              </>}
            </Space>
          </Card>

          <Card className="dfu-card" title={copy.artifactStep}>
            <Space direction="vertical" size="middle" className="dfu-stack">
              {fetchingLatest ? (
                <Alert
                  type="info"
                  showIcon
                  message={copy.downloadingLatest}
                  description={copy.onlineValidation}
                />
              ) : prepared?.artifact_source === "r2" ? (
                <Alert
                  type="success"
                  showIcon
                  message={copy.latestReady}
                  description={copy.onlineValidation}
                />
              ) : prepared?.artifact_source === "local" ? (
                <Alert
                  type="success"
                  showIcon
                  message={copy.validationPassed}
                  description={copy.localStillValidated}
                />
              ) : onlineError ? (
                <Alert
                  type="warning"
                  showIcon
                  message={copy.onlineUnavailable}
                  description={
                    <Space direction="vertical" size="small">
                      <Typography.Text>{onlineError}</Typography.Text>
                      <Typography.Text type="secondary">
                        {copy.localStillValidated}
                      </Typography.Text>
                    </Space>
                  }
                />
              ) : (
                <Alert
                  type="info"
                  showIcon
                  message={copy.scanForLatest}
                  description={copy.localStillValidated}
                />
              )}
              <input
                ref={inputRef}
                className="dfu-panel__file-input"
                type="file"
                accept={device?.protocol_version === 3 ? ".hpmota" : ".hpmota,.bin"}
                disabled={busy || probing || fetchingLatest || validating}
                onChange={(event) => {
                  void fileSelected(event.currentTarget.files?.[0]);
                }}
              />
              <Space wrap>
                {device?.security_mode === "production_confidential" && (
                  <Button
                    disabled={busy || probing || fetchingLatest || validating}
                    loading={fetchingLatest}
                    onClick={() => void prepareLatest()}
                  >
                    {copy.retryLatest}
                  </Button>
                )}
                <Button
                  disabled={!device || busy || probing || fetchingLatest || validating}
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
                  disabled={!prepared || busy || probing || fetchingLatest || validating}
                  onClick={confirmUpgrade}
                >
                  {copy.startUpgrade}
                </Button>
                <Button
                  disabled={
                    (!busy && !probing) ||
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
                      : copy.stage.connecting}
                  </Typography.Text>
                  {busy && progress?.cancellable === false && (
                    <Typography.Text type="secondary">
                      {copy.waitForCommand}
                    </Typography.Text>
                  )}
                </div>
              )}
              {outcome && <OutcomeAlert outcome={outcome} copy={copy} />}
              {error && (
                <Alert
                  type="error"
                  showIcon
                  message={copy.upgradeError}
                  description={error}
                />
              )}
            </Space>
          </Card>
        </div>

  </div>);
}

function DeviceDetails({
  device,
  copy,
}: {
  device: HpmDfuDevice;
  copy: Copy;
}) {
  const v3 = device.protocol_version === 3;
  const protectedDevice = device.security_mode === "production_confidential" || device.security_mode === "v3_encrypted_unsupported";
  return (
    <>
      <Divider />
      <Space wrap>
        <Tag color="green">{v3 ? `HPM V3 · ${device.transport.toUpperCase()}${device.node_id ? ` #${device.node_id}` : ""}` : copy.knownProduct}</Tag>
        <Tag color={protectedDevice ? "red" : "gold"}>
          {protectedDevice ? copy.protectedMode : v3 ? copy.signedPlaintext : copy.developmentMode}
        </Tag>
      </Space>
      <Descriptions size="small" column={1} className="dfu-descriptions">
        <Descriptions.Item label={copy.product}>
          {v3 ? "HPM" : "gs_can"} · {device.product_code_hex} ({device.product_code_ascii})
        </Descriptions.Item>
        <Descriptions.Item label={copy.uid}>
          <Typography.Text copyable>{device.uid}</Typography.Text>
        </Descriptions.Item>
        <Descriptions.Item label={copy.bootloader}>
          {device.bootloader_version} ({v3 ? "V3" : "exact legacy profile"})
        </Descriptions.Item>
        <Descriptions.Item label={copy.chipFamily}>
          {device.chip_family_id_hex}
        </Descriptions.Item>
        <Descriptions.Item label={copy.hardware}>
          {device.hardware_version_valid
            ? device.hardware_version_hex
            : copy.hardwareUnprovisioned}
        </Descriptions.Item>
        <Descriptions.Item label={copy.appRegion}>
          {device.app0_address_hex} · {formatBytes(device.app0_max_size)}
        </Descriptions.Item>
        <Descriptions.Item label={copy.keyFingerprint}>
          {device.key_fingerprint_hex}
        </Descriptions.Item>
        <Descriptions.Item label={copy.pubkeyFingerprint}>
          {device.pubkey_fingerprint_hex}
        </Descriptions.Item>
        <Descriptions.Item label={copy.arvFloor}>
          {device.otp_app_arv_floor_state.startsWith("corrupt")
            ? v3 ? copy.v3ArvCorrupt : copy.arvCorrupt
            : `${device.otp_app_arv_floor} · ${v3 ? copy.enforced : copy.notEnforced}`}
        </Descriptions.Item>
      </Descriptions>
    </>
  );
}

function ArtifactDetails({
  prepared,
  copy,
}: {
  prepared: HpmDfuPrepared;
  copy: Copy;
}) {
  return (
    <>
      <Divider />
      <Space wrap>
        <Tag color="green">{copy.validationPassed}</Tag>
        <Tag>
          {prepared.artifact_kind === "legacy_hpmota_v2"
            ? ".hpmota v2"
            : prepared.artifact_kind === "signed_hpmota_v3" ? ".hpmota v3 · P-256" : copy.rawDevelopment}
        </Tag>
      </Space>
      <Descriptions size="small" column={1} className="dfu-descriptions">
        <Descriptions.Item label={copy.artifactSource}>
          {prepared.artifact_source === "r2"
            ? copy.onlineSource
            : copy.localSource}
        </Descriptions.Item>
        {prepared.release_version && (
          <Descriptions.Item label={copy.releaseLabel}>
            {prepared.release_version} · {copy.notAntiRollback}
          </Descriptions.Item>
        )}
        <Descriptions.Item label={copy.fileSha}>
          <Typography.Text copyable={{ text: prepared.source_sha256 }}>
            {shortHash(prepared.source_sha256)}
          </Typography.Text>
        </Descriptions.Item>
        <Descriptions.Item label={copy.imageSha}>
          <Typography.Text copyable={{ text: prepared.wire_image_sha256 }}>
            {shortHash(prepared.wire_image_sha256)}
          </Typography.Text>
        </Descriptions.Item>
        <Descriptions.Item label={copy.sizes}>
          {formatBytes(prepared.source_size)} →{" "}
          {formatBytes(prepared.erase_size)}
        </Descriptions.Item>
        {prepared.pack_tool_version && (
          <Descriptions.Item label={copy.packer}>
            {prepared.pack_tool_version}
          </Descriptions.Item>
        )}
        {prepared.app_arv != null && (
          <Descriptions.Item label="app_arv">
            {prepared.app_arv} · {prepared.app_arv_state === "signature_verified_enforced" ? copy.enforced : copy.metadataOnly}
          </Descriptions.Item>
        )}
      </Descriptions>
      {prepared.artifact_kind === "legacy_hpmota_v2" && (
        <Alert
          type="warning"
          showIcon
          message={copy.legacyEncodingUnproven}
          description={copy.legacyEncodingDetail}
        />
      )}
    </>
  );
}

function OutcomeAlert({
  outcome,
  copy,
}: {
  outcome: HpmDfuOutcome;
  copy: Copy;
}) {
  if (outcome.status === "jump_acked_startup_unconfirmed") {
    return (
      <Alert
        className="dfu-panel__outcome--success"
        type="warning"
        showIcon
        message={copy.transferComplete}
        description={copy.startupUnconfirmed}
      />
    );
  }
  if (outcome.status === "jump_outcome_unknown") {
    return (
      <Alert
        type="warning"
        showIcon
        message={copy.jumpUnknown}
        description={copy.startupUnconfirmed}
      />
    );
  }
  return (
    <Alert
      type="warning"
      showIcon
      message={copy.cancelled}
      description={
        outcome.status === "cancelled_before_erase"
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

function textFor(lang: "en" | "zh") {
  if (lang === "zh") {
    return {
      eyebrow: "固件维护",
      title: "设备升级",
      lead:
        "HPM 支持 USB 与 Classic CAN V3 签名升级，并保留旧 gs_can USB 流程。其他 CAN 产品继续按完整 0x1018 身份选择升级后端。",
      canTransportLabel: "其他 CAN",
      canInterface: "CAN 接口（1 Mbit/s）",
      lssAssign: "未配置节点：通过 LSS 分配地址",
      freeNode: "确认空闲的 Node-ID（会保存到 NVS）",
      optionalUid: "目标完整 UID（可选，32 位十六进制字符）",
      captureCan: "等待并捕获 CAN Bootloader",
      canCaptureHint: "先开始等待，再给目标上电。收到心跳后才发送 LSS／SDO；仅用 11 位 Classic CAN 帧。当前一次捕获一个目标。",
      waitLatched: "已捕获：保持等待，无需保活；断线或释放后也不会自行启动 APP。",
      releaseCapture: "释放连接",
      signedPlaintext: "明文 · P-256 签名",
      enforced: "签名校验并强制执行 floor",
      v3ArvCorrupt: "OTP floor 异常，拒绝写入",
      v3LocalOnly: "V3 请选用签名 .hpmota 文件；当前没有对应的在线发布配置。加密 V3 尚未实现。",
      v3ConfirmBody: "将重新核对 UID、产品和安全状态，然后执行 PREPARE → WRITE → COMMIT → JUMP。捕获状态无需保活；提交和跳转期间请等待应答。",
      closeBlocked: "升级命令正在执行。请先等待当前命令结束并取消升级，再关闭窗口。",
      deviceStep: "1 · 识别设备",
      bootloaderHint: "先点击等待，再插入或给设备上电；USB 捕获最长等待 30 秒。",
      probe: "等待并捕获 USB Bootloader",
      probing: "扫描中…",
      downloadingLatest: "正在下载并校验最新固件…",
      deviceRecognized: "已识别 Bootloader",
      probeFirst: "请先扫描并识别设备",
      artifactStep: "2 · 获取并校验升级文件",
      latestReady: "最新 R2 固件已下载并通过前置校验",
      onlineValidation:
        "固定 HTTPS 路径提供 release 与整包 SHA 绑定；下载内容仍执行和手选文件完全相同的设备、格式、长度与密钥指纹校验。",
      onlineUnavailable: "在线固件暂不可用，可重试或手动选择文件",
      scanForLatest: "识别受保护设备后会自动获取最新固件",
      localStillValidated:
        "手动选择只改变文件来源，不会绕过身份、格式、SHA、长度或密钥指纹检查。",
      developmentLocalOnly:
        "开发模式设备不请求在线 secure 制品，请手动选择 plaintext .bin。",
      retryLatest: "重新获取最新固件",
      chooseFile: "手动选择 .hpmota / .bin",
      fileTooLarge: "文件超过 2 MiB 的硬上限。",
      validationPassed: "全部前置校验已通过",
      upgradeStep: "3 · 写入与校验",
      destructiveHint:
        "开始后会擦除 APP0。升级中可在协议命令之间取消；正在执行的 ERASE/WRITE 必须先等待设备回复。",
      confirmTitle: "确认擦除并升级？",
      confirmBody:
        "即将重新读取同一 UID 和 Bootloader 身份，然后执行 ERASE → WRITE → CRC32 → KN_DATA（安全设备）→ JUMP。",
      startUpgrade: "开始升级",
      goBack: "返回检查",
      cancel: "取消升级",
      cancelQueued: "已请求取消；将在当前协议命令回复后停止。",
      cancelQueuedShort: "等待安全停止…",
      waitForCommand: "当前命令不可中断，正在等待设备 ACK。",
      upgradeError: "升级未完成",
      knownProduct: "已知产品 · gcan",
      protectedMode: "受保护设备",
      developmentMode: "开发设备",
      product: "产品",
      uid: "UID",
      bootloader: "Bootloader",
      chipFamily: "芯片族",
      hardware: "硬件版本",
      hardwareUnprovisioned: "OTP 未配置（当前 legacy profile 允许）",
      appRegion: "APP0 区域",
      keyFingerprint: "主密钥指纹（防呆）",
      pubkeyFingerprint: "验签公钥指纹（防呆）",
      arvFloor: "防回滚 floor",
      arvCorrupt: "OTP bit pattern 异常；当前仍仅诊断",
      notEnforced: "仅报告，未启用",
      rawDevelopment: "裸 bin · 开发模式",
      artifactSource: "制品来源",
      onlineSource: "R2 HTTPS 发布",
      localSource: "本地手选文件",
      releaseLabel: "R2 发布标签",
      notAntiRollback: "不是防回滚版本证明",
      fileSha: "原文件 SHA-256",
      imageSha: "写入镜像 SHA-256",
      sizes: "文件 → 擦除长度",
      packer: "打包工具版本",
      metadataOnly: "公开 metadata，仅显示；未启用防回滚",
      legacyEncodingUnproven: "当前 v2 格式无法证明打包时启用了 EXIP 加密",
      legacyEncodingDetail:
        "R2 HTTPS 与 whole-file SHA 绑定只负责选择完整制品，无法让 v2 在 host 侧证明 EXIP 打包模式。相同的严格本地校验仍会执行，设备在启动前继续承担最终 APP0 验签。",
      transferComplete: "传输完成，已收到 JUMP ACK",
      startupUnconfirmed:
        "当前升级应答无法确认 APP 是否正常启动。请实际检查设备功能；异常时可重新进入 Bootloader 完整升级。",
      jumpUnknown: "JUMP 结果未知",
      cancelled: "升级已停止",
      cancelledSafe: "取消发生在 ERASE 前，APP0 未被本次升级改动。",
      cancelledRecoverable:
        "设备可能保留在 Bootloader。请保持供电并重新执行一次完整升级。",
      stage: {
        capturing: "等待捕获",
        preparing: "验证签名与升级条件",
        committing: "完整验签并提交试运行镜像",
        connecting: "连接 Bootloader",
        revalidating: "重新核对 UID、产品 profile 和安全状态",
        erasing: "擦除 APP0",
        writing: "写入 APP0",
        verifying_crc32: "设备端 CRC32 校验",
        writing_kn_data: "写入 KN_DATA",
        requesting_jump: "请求跳转 APP0",
      } satisfies Record<HpmDfuStage, string>,
    };
  }
  return {
    eyebrow: "Firmware maintenance",
    title: "Device Firmware Update",
    lead:
      "HPM supports signed V3 updates over USB and Classic CAN, plus legacy gs_can USB. Other CAN devices keep their exact 0x1018-based update backend.",
    canTransportLabel: "Other CAN",
    canInterface: "CAN interface (1 Mbit/s)",
    lssAssign: "Unconfigured node: assign an address using LSS",
    freeNode: "Confirmed free Node-ID (saved to NVS)",
    optionalUid: "Full target UID (optional, 32 hex characters)",
    captureCan: "Wait for CAN Bootloader",
    canCaptureHint: "Start waiting before powering on the target. Send LSS/SDO only after its heartbeat; 11-bit Classic CAN only. Captures one target at a time.",
    waitLatched: "Captured: waiting without keepalives. Disconnecting or releasing does not start the APP.",
    releaseCapture: "Release connection",
    signedPlaintext: "Plaintext · P-256 signed",
    enforced: "signature verified; floor enforced",
    v3ArvCorrupt: "Corrupt OTP floor; writing is blocked",
    v3LocalOnly: "Select a signed V3 .hpmota file; no matching online release profile is configured. Encrypted V3 is not implemented.",
    v3ConfirmBody: "Revalidate UID, product and security state, then PREPARE → WRITE → COMMIT → JUMP. Captured devices need no keepalives. Wait for commit and jump acknowledgements.",
    closeBlocked:
      "A firmware command is in progress. Wait for it, cancel safely, then close the window.",
    deviceStep: "1 · Identify device",
    bootloaderHint: "Start waiting, then connect or power on the device; USB capture waits up to 30 seconds.",
    probe: "Wait for USB Bootloader",
    probing: "Scanning…",
    downloadingLatest: "Downloading and validating the latest firmware…",
    deviceRecognized: "Bootloader recognized",
    probeFirst: "Probe and recognize the device first",
    artifactStep: "2 · Fetch and validate firmware",
    latestReady: "The latest R2 firmware passed preflight validation",
    onlineValidation:
      "The fixed HTTPS path binds the release and whole-file SHA. Downloaded bytes still pass the same device, format, length and key-fingerprint checks as a local file.",
    onlineUnavailable:
      "Online firmware is unavailable; retry or select a local file",
    scanForLatest:
      "Recognizing a protected device automatically fetches its latest firmware",
    localStillValidated:
      "Manual selection changes only the source; it never bypasses identity, format, SHA, size or key-fingerprint checks.",
    developmentLocalOnly:
      "Development devices do not request an online secure artifact; select a plaintext .bin locally.",
    retryLatest: "Fetch latest again",
    chooseFile: "Choose local .hpmota / .bin",
    fileTooLarge: "The file exceeds the 2 MiB hard limit.",
    validationPassed: "All preflight checks passed",
    upgradeStep: "3 · Write and verify",
    destructiveHint:
      "Starting erases APP0. Cancellation is honored between protocol commands; an in-flight ERASE/WRITE must first return.",
    confirmTitle: "Erase APP0 and upgrade?",
    confirmBody:
      "The backend will re-read the same UID and Bootloader identity, then run ERASE → WRITE → CRC32 → KN_DATA (protected device) → JUMP.",
    startUpgrade: "Start upgrade",
    goBack: "Review",
    cancel: "Cancel upgrade",
    cancelQueued: "Cancellation requested; stopping after the current command returns.",
    cancelQueuedShort: "Waiting to stop safely…",
    waitForCommand: "The current command cannot be interrupted; waiting for its ACK.",
    upgradeError: "Upgrade did not complete",
    knownProduct: "Known product · gcan",
    protectedMode: "Protected device",
    developmentMode: "Development device",
    product: "Product",
    uid: "UID",
    bootloader: "Bootloader",
    chipFamily: "Chip family",
    hardware: "Hardware version",
    hardwareUnprovisioned: "OTP not provisioned (allowed by this legacy profile)",
    appRegion: "APP0 region",
    keyFingerprint: "Master-key fingerprint (mistake prevention)",
    pubkeyFingerprint: "Verify-key fingerprint (mistake prevention)",
    arvFloor: "Anti-rollback floor",
    arvCorrupt: "OTP bit pattern is abnormal; currently diagnostic only",
    notEnforced: "reported only, not enforced",
    rawDevelopment: "raw bin · development",
    artifactSource: "Artifact source",
    onlineSource: "R2 HTTPS release",
    localSource: "Locally selected file",
    releaseLabel: "R2 release label",
    notAntiRollback: "not anti-rollback evidence",
    fileSha: "Source file SHA-256",
    imageSha: "Wire image SHA-256",
    sizes: "File → erase length",
    packer: "Packer version",
    metadataOnly: "public metadata only; anti-rollback is not enabled",
    legacyEncodingUnproven:
      "The current v2 format cannot prove that EXIP encryption was enabled while packing",
    legacyEncodingDetail:
      "R2 HTTPS and whole-file SHA binding select one complete artifact, but cannot make v2 prove its EXIP packing mode to the host. The same strict local checks still run, and the device remains the final APP0 signature verifier before boot.",
    transferComplete: "Transfer complete; JUMP ACK received",
    startupUnconfirmed:
      "The upgrade ACK cannot confirm that the application became healthy. Check the device's actual function; on failure, re-enter Bootloader and run a complete upgrade.",
    jumpUnknown: "JUMP outcome is unknown",
    cancelled: "Upgrade stopped",
    cancelledSafe: "Cancellation happened before ERASE; this run did not alter APP0.",
    cancelledRecoverable:
      "The device may remain in Bootloader. Keep power applied and run one complete upgrade again.",
    stage: {
      capturing: "Waiting for capture",
      preparing: "Validate signature and update policy",
      committing: "Verify image and commit trial",
      connecting: "Connect Bootloader",
      revalidating: "Revalidate UID, product profile and security state",
      erasing: "Erase APP0",
      writing: "Write APP0",
      verifying_crc32: "Device CRC32 verification",
      writing_kn_data: "Write KN_DATA",
      requesting_jump: "Request APP0 jump",
    } satisfies Record<HpmDfuStage, string>,
  };
}
