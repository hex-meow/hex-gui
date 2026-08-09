import { useEffect, useMemo, useState } from "react";
import {
  Alert,
  App,
  Button,
  Card,
  Checkbox,
  Col,
  Descriptions,
  Empty,
  Input,
  Row,
  Space,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { nid2hex } from "../format";
import { useI18n } from "../i18n";
import type {
  AuthenticityTarget,
  CalibrationPayload,
  CalibrationRawWord,
  CalibrationSource,
  CalibrationUpdatePersistedResult,
  CalibrationUpdatePrepared,
  CalibrationUpdatePreview,
  CalibrationUpdateWriteResult,
  MotorInfo,
} from "../types";

const targetOf = (motor: MotorInfo): AuthenticityTarget => ({
  nodeId: motor.node_id,
  sessionEpoch: motor.session_epoch,
});

export function MotorCalibrationUpdatePanel({
  connected,
  devices,
  onBusyChange,
}: {
  connected: boolean;
  devices: MotorInfo[];
  onBusyChange: (busy: boolean) => void;
}) {
  const { lang } = useI18n();
  const { message, modal } = App.useApp();
  const text = (en: string, zh: string) => (lang === "zh" ? zh : en);
  const motors = useMemo(
    () =>
      devices
        .filter(
          (device) =>
            device.online &&
            device.device_type === "meow_motor" &&
            device.identity != null,
        )
        .sort((left, right) => left.node_id - right.node_id),
    [devices],
  );
  const motor = motors.length === 1 ? motors[0] : null;
  const [busy, setBusy] = useState<string | null>(null);
  const [prepared, setPrepared] = useState<CalibrationUpdatePrepared | null>(null);
  const [backupJson, setBackupJson] = useState("");
  const [torqueJson, setTorqueJson] = useState("");
  const [frictionJson, setFrictionJson] = useState("");
  const [includeFriction, setIncludeFriction] = useState(true);
  const [preview, setPreview] = useState<CalibrationUpdatePreview | null>(null);
  const [backupAcknowledged, setBackupAcknowledged] = useState(false);
  const [writeResult, setWriteResult] = useState<CalibrationUpdateWriteResult | null>(null);
  const [persisted, setPersisted] =
    useState<CalibrationUpdatePersistedResult | null>(null);

  useEffect(() => onBusyChange(busy != null), [busy, onBusyChange]);

  const run = async <T,>(label: string, action: () => Promise<T>): Promise<T | null> => {
    setBusy(label);
    try {
      return await action();
    } catch (error) {
      message.error(errMsg(error));
      return null;
    } finally {
      setBusy(null);
    }
  };

  const prepare = async () => {
    if (!motor) return;
    const result = await run("prepare", () =>
      api.calibrationUpdatePrepare(targetOf(motor)),
    );
    if (!result) return;
    const backup = {
      schema: "hex-meow/meow-motor-0x4001-backup/v1",
      captured_at: new Date().toISOString(),
      identity: result.identity,
      node_id_at_capture: result.node_id,
      heartbeat_session_epoch: result.session_epoch,
      online_authenticity_status: result.online_status,
      token_u64_decimal: result.token_decimal,
      token_u64_hex: result.token_hex,
      highest_subindex: result.highest_subindex,
      words: result.backup_words,
    };
    setPrepared(result);
    setBackupJson(JSON.stringify(backup, null, 2));
    setPreview(null);
    setBackupAcknowledged(false);
    setWriteResult(null);
    setPersisted(null);
    message.success(text("Motor is authentic and backed up", "电机验证合法，完整备份已读取"));
  };

  const copyBackup = async () => {
    try {
      await navigator.clipboard.writeText(backupJson);
      message.success(text("Raw backup JSON copied", "原始备份 JSON 已复制"));
    } catch (error) {
      message.error(`${text("Copy failed", "复制失败")}: ${errMsg(error)}`);
    }
  };

  const makePreview = async () => {
    if (!prepared) return;
    if (!torqueJson.trim()) {
      message.warning(text("Paste the torque result JSON", "请粘贴力矩标定结果 JSON"));
      return;
    }
    if (includeFriction && !frictionJson.trim()) {
      message.warning(text("Paste the friction result JSON", "请粘贴摩擦力标定结果 JSON"));
      return;
    }
    const result = await run("preview", () =>
      api.calibrationUpdatePreview({
        target: {
          nodeId: prepared.node_id,
          sessionEpoch: prepared.session_epoch,
        },
        torqueJson,
        frictionJson: includeFriction ? frictionJson : null,
      }),
    );
    if (!result) return;
    setPreview(result);
    setBackupAcknowledged(false);
    setWriteResult(null);
    setPersisted(null);
    message.success(text("Wire image and CRC calculated", "量化数据与 CRC 已计算"));
  };

  const confirmWrite = () => {
    if (!prepared || !preview || !motor || !backupAcknowledged) return;
    modal.confirm({
      title: text("Overwrite this motor's calibration?", "覆盖这台电机的校准数据？"),
      width: 720,
      content: (
        <Space direction="vertical" size="middle" style={{ width: "100%" }}>
          <Typography.Paragraph>
            {text(
              "This performs three persistent saves: invalidate the old manifest, save the new payload with the same token, then commit the new CRC. Do not remove power until the command finishes.",
              "该操作会执行三次持久化保存：先使旧 manifest 失效，再保存保留原 token 的新 payload，最后提交新 CRC。命令完成前不得断电。",
            )}
          </Typography.Paragraph>
          <Alert
            type="warning"
            showIcon
            message={text(
              "0x1010:04 saves every dirty manufacturer parameter, not only 0x4001. Use a freshly power-cycled, exclusively connected motor and make no unrelated parameter writes first.",
              "0x1010:04 会保存所有已变脏的厂家参数，不仅是 0x4001。请使用刚完成上电、独占连接的电机，并确保此前没有写入其他厂家参数。",
            )}
          />
          <IdentityDescription identity={prepared.identity} />
        </Space>
      ),
      okText: text("Write and save", "写入并保存"),
      okButtonProps: { danger: true },
      cancelText: text("Cancel", "取消"),
      onOk: async () => {
        const result = await run("write", () =>
          api.calibrationUpdateWrite({
            target: targetOf(motor),
            previewId: preview.preview_id,
            backupAcknowledged,
          }),
        );
        if (!result) throw new Error(text("Write failed", "写入失败"));
        setWriteResult(result);
        setPersisted(null);
        message.warning(
          text(
            "Saved and read back in RAM. Fully power-cycle the motor before final verification.",
            "已保存并完成 RAM 读回。最终验证前必须让电机完整断电重启。",
          ),
        );
      },
    });
  };

  const verifyPersisted = async () => {
    if (!writeResult || !motor) return;
    const result = await run("verify", () =>
      api.calibrationUpdateVerifyPersisted({
        target: targetOf(motor),
        previewId: writeResult.preview_id,
      }),
    );
    if (!result) return;
    setPersisted(result);
    message.success(
      text(
        "Persisted calibration and online authenticity verified",
        "掉电保存的校准数据与在线来源验证均已通过",
      ),
    );
  };

  return (
    <div style={{ padding: 24, maxWidth: 1280, margin: "0 auto" }}>
      <Space direction="vertical" size="large" style={{ width: "100%" }}>
        <Alert
          type="warning"
          showIcon
          message={text(
            "Developer tool · attended user recalibration",
            "开发者工具 · 人工值守的用户重标",
          )}
          description={text(
            "Exactly one known Meow Motor must be online. The app first proves its current identity + token online, backs up every reported 0x4001 word, and never obtains a token from the server. Factory repair/reissuance remains a separate private workflow.",
            "CAN 上必须只有一台已知 Meow Motor 在线。APP 会先在线验证当前 identity + token，备份 0x4001 报告的全部槽位，且绝不从服务器获取 token。工厂返修与重新签发仍是独立的私有流程。",
          )}
        />

        <Card title={text("1. Read, back up and verify", "1. 读取、备份并验证")}>
          {!connected ? (
            <Empty description={text("Connect CAN first", "请先连接 CAN")} />
          ) : motors.length === 0 ? (
            <Empty description={text("No online Meow Motor", "没有在线的 Meow Motor")} />
          ) : motors.length > 1 ? (
            <Alert
              type="error"
              showIcon
              message={text(
                `Disconnect all but one Meow Motor (${motors.length} detected)`,
                `请断开多余电机（检测到 ${motors.length} 台 Meow Motor）`,
              )}
            />
          ) : (
            <Space direction="vertical" size="middle" style={{ width: "100%" }}>
              <Typography.Text>
                {motor?.friendly_name} · {nid2hex(motor?.node_id ?? 0)} · S/N {" "}
                {motor?.identity?.serial_number.toString(16).toUpperCase()}
              </Typography.Text>
              <Button type="primary" loading={busy === "prepare"} disabled={busy != null} onClick={prepare}>
                {text("Read full backup and verify online", "读取完整备份并在线验证")}
              </Button>
            </Space>
          )}

          {prepared && (
            <Space direction="vertical" size="middle" style={{ width: "100%", marginTop: 20 }}>
              <Alert
                type="success"
                showIcon
                message={`${text("Authenticity", "来源验证")}: ${prepared.online_status}`}
              />
              <IdentityDescription identity={prepared.identity} />
              <Descriptions bordered size="small" column={{ xs: 1, md: 2 }}>
                <Descriptions.Item label="token u64 (LE)">
                  <Typography.Text copyable>{prepared.token_hex}</Typography.Text>
                  {" · "}
                  <Typography.Text copyable>{prepared.token_decimal}</Typography.Text>
                </Descriptions.Item>
                <Descriptions.Item label="0x4001:00">
                  {prepared.highest_subindex}
                </Descriptions.Item>
              </Descriptions>
              <WordList title={text("Complete raw backup", "完整原始备份")} words={prepared.backup_words} />
              <Button onClick={copyBackup}>{text("Copy backup JSON", "复制备份 JSON")}</Button>
              <PayloadDescription
                title={text("Current decoded calibration", "当前解码校准")}
                payload={prepared.current_calibration}
              />
            </Space>
          )}
        </Card>

        <Card title={text("2. Paste calibration results", "2. 粘贴校准结果")}>
          {!prepared ? (
            <Empty description={text("Verify one motor first", "请先验证一台电机")} />
          ) : (
            <Space direction="vertical" size="middle" style={{ width: "100%" }}>
              <Typography.Text strong>
                {text("Gravity torque-factor result JSON", "重力力矩系数结果 JSON")}
              </Typography.Text>
              <Input.TextArea
                rows={9}
                value={torqueJson}
                placeholder='{"schema":"hex-meow/gravity-torque-calibration-result/v3", ...}'
                onChange={(event) => {
                  setTorqueJson(event.target.value);
                  setPreview(null);
                }}
              />
              <Checkbox
                checked={includeFriction}
                onChange={(event) => {
                  setIncludeFriction(event.target.checked);
                  setPreview(null);
                }}
              >
                {text(
                  "Include friction calibration (clear this to write canonical zero to 0x4001:05..07)",
                  "包含摩擦力标定（取消勾选将向 0x4001:05..07 写入规范零值）",
                )}
              </Checkbox>
              <Input.TextArea
                rows={9}
                disabled={!includeFriction}
                value={frictionJson}
                placeholder='{"schema":"hex-meow/friction-calibration-result/v1", ...}'
                onChange={(event) => {
                  setFrictionJson(event.target.value);
                  setPreview(null);
                }}
              />
              <Button type="primary" loading={busy === "preview"} disabled={busy != null} onClick={makePreview}>
                {text("Validate, quantize and preview CRC", "校验、量化并预览 CRC")}
              </Button>
            </Space>
          )}
        </Card>

        {preview && prepared && (
          <Card title={text("3. Review exact wire image", "3. 核对精确 wire image")}>
            <Space direction="vertical" size="middle" style={{ width: "100%" }}>
              <Row gutter={[16, 16]}>
                <Col xs={24} lg={12}>
                  <SourceDescription title={text("Torque source", "力矩来源")} source={preview.torque_source} />
                </Col>
                <Col xs={24} lg={12}>
                  {preview.friction_source ? (
                    <SourceDescription title={text("Friction source", "摩擦力来源")} source={preview.friction_source} />
                  ) : (
                    <Alert type="info" showIcon message={text("Friction absent", "摩擦力标定缺省")} />
                  )}
                </Col>
              </Row>
              <Row gutter={[16, 16]}>
                <Col xs={24} lg={12}>
                  <PayloadDescription title={text("Requested values", "请求值")} payload={preview.requested} />
                </Col>
                <Col xs={24} lg={12}>
                  <PayloadDescription title={text("Decoded quantized values", "量化后解码值")} payload={preview.quantized} />
                </Col>
              </Row>
              {preview.warnings.map((warning) => (
                <Alert key={warning} type="warning" showIcon message={warning} />
              ))}
              <WordList title="0x4001:01..07" words={preview.new_words} />
              <Typography.Text type="secondary">
                preview SHA-256: <Typography.Text copyable code>{preview.preview_id}</Typography.Text>
              </Typography.Text>
              <Checkbox checked={backupAcknowledged} onChange={(event) => setBackupAcknowledged(event.target.checked)}>
                {text(
                  "I saved the complete old backup, checked the target identity and reviewed every new word.",
                  "我已保存完整旧数据、核对目标 identity，并检查了每个新 word。",
                )}
              </Checkbox>
              <Button danger type="primary" disabled={!backupAcknowledged || busy != null || !motor} onClick={confirmWrite}>
                {text("Write this one motor", "写入这一台电机")}
              </Button>
            </Space>
          </Card>
        )}

        {writeResult && (
          <Card title={text("4. Power-cycle and verify persistence", "4. 断电重启并验证持久化")}>
            <Space direction="vertical" size="middle" style={{ width: "100%" }}>
              <Alert
                type={persisted ? "success" : "warning"}
                showIcon
                message={
                  persisted
                    ? text("Update complete", "更新完成")
                    : text(
                        "RAM readback and all three 0x1010:04 saves succeeded. Fully remove motor power, restore it, and wait for a new heartbeat session.",
                        "RAM 读回与三次 0x1010:04 保存均成功。请让电机完全断电后重新上电，并等待新的心跳 session。",
                      )
                }
              />
              <Button
                type="primary"
                loading={busy === "verify"}
                disabled={busy != null || !motor || persisted != null}
                onClick={verifyPersisted}
              >
                {text("Verify after power cycle", "断电重启后验证")}
              </Button>
              {persisted && (
                <Descriptions bordered size="small" column={{ xs: 1, md: 2 }}>
                  <Descriptions.Item label={text("Online status", "在线状态")}>
                    <Tag color="success">{persisted.online_status}</Tag>
                  </Descriptions.Item>
                  <Descriptions.Item label={text("New heartbeat session", "新心跳 session")}>
                    {persisted.session_epoch}
                  </Descriptions.Item>
                </Descriptions>
              )}
            </Space>
          </Card>
        )}
      </Space>
    </div>
  );
}

function IdentityDescription({ identity }: { identity: CalibrationSource }) {
  return (
    <Descriptions bordered size="small" column={{ xs: 1, md: 2, lg: 4 }}>
      <Descriptions.Item label="Vendor">0x{identity.vendor_id.toString(16).padStart(8, "0").toUpperCase()}</Descriptions.Item>
      <Descriptions.Item label="Product">0x{identity.product_code.toString(16).padStart(8, "0").toUpperCase()}</Descriptions.Item>
      <Descriptions.Item label="Revision">0x{identity.revision_number.toString(16).padStart(8, "0").toUpperCase()}</Descriptions.Item>
      <Descriptions.Item label="Serial">0x{identity.serial_number.toString(16).padStart(8, "0").toUpperCase()}</Descriptions.Item>
    </Descriptions>
  );
}

function SourceDescription({ title, source }: { title: string; source: CalibrationSource }) {
  return (
    <Space direction="vertical" style={{ width: "100%" }}>
      <Typography.Text strong>{title}</Typography.Text>
      <IdentityDescription identity={source} />
    </Space>
  );
}

function PayloadDescription({ title, payload }: { title: string; payload: CalibrationPayload }) {
  const values = payload.friction;
  return (
    <Space direction="vertical" style={{ width: "100%" }}>
      <Typography.Text strong>{title}</Typography.Text>
      <Descriptions bordered size="small" column={1}>
        <Descriptions.Item label="torque factor">{payload.torque_factor.toPrecision(10)}</Descriptions.Item>
        <Descriptions.Item label="fit RMSE">{payload.torque_fit_rmse_nm.toPrecision(10)} Nm</Descriptions.Item>
        <Descriptions.Item label="static + / −">{values ? `${values.static_pos_raw_nm.toPrecision(8)} / ${values.static_neg_raw_nm.toPrecision(8)} Nm` : "absent"}</Descriptions.Item>
        <Descriptions.Item label="kinetic + / −">{values ? `${values.kinetic_pos_raw_nm.toPrecision(8)} / ${values.kinetic_neg_raw_nm.toPrecision(8)} Nm` : "absent"}</Descriptions.Item>
        <Descriptions.Item label="reference / temperature">{values ? `${values.reference_speed_rad_per_s.toPrecision(8)} rad/s · ${values.calibration_temperature_c.toPrecision(8)} °C` : "absent"}</Descriptions.Item>
      </Descriptions>
    </Space>
  );
}

function WordList({ title, words }: { title: string; words: CalibrationRawWord[] }) {
  return (
    <Space direction="vertical" style={{ width: "100%" }}>
      <Typography.Text strong>{title}</Typography.Text>
      <Space wrap>
        {words.map((word) => (
          <Tag key={word.subindex} color={word.subindex <= 7 ? "blue" : "default"}>
            :{word.subindex.toString(16).padStart(2, "0").toUpperCase()} = {word.value_hex}
          </Tag>
        ))}
      </Space>
    </Space>
  );
}
