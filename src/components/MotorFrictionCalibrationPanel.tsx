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
  InputNumber,
  Progress,
  Row,
  Select,
  Space,
  Statistic,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import { nid2hex } from "../format";
import { useI18n } from "../i18n";
import type {
  FrictionCalibrationRequest,
  FrictionCalibrationResult,
  FrictionCalibrationView,
  MotorInfo,
} from "../types";

const DEFAULT_STEP_PERMILLE = 1;
const DEFAULT_MAX_PERMILLE = 100;
const DEFAULT_DWELL_MS = 1_000;
const DEFAULT_POSITION_THRESHOLD_RAD = 0.01;
const DEFAULT_VELOCITY_THRESHOLD_RAD_PER_S = 0.05;
const DEFAULT_KINETIC_SAMPLE_MS = 3_000;

const EMPTY_VIEW: FrictionCalibrationView = {
  running: false,
  phase: "idle",
  progress_percent: 0,
  node_id: null,
  current_command_permille: 0,
  position_rad: null,
  velocity_rad_per_s: null,
  actual_torque_permille: null,
  motor_temperature_c: null,
  result: null,
  error: null,
  cleanup_warning: null,
};

export function MotorFrictionCalibrationPanel({
  connected,
  devices,
  onRunningChange,
}: {
  connected: boolean;
  devices: MotorInfo[];
  onRunningChange: (running: boolean) => void;
}) {
  const { message } = App.useApp();
  const { lang } = useI18n();
  const text = (en: string, zh: string) => (lang === "zh" ? zh : en);
  const motors = useMemo(
    () =>
      devices.filter(
        (device) =>
          device.device_type === "meow_motor" &&
          device.online &&
          device.identity != null,
      ),
    [devices],
  );
  const [selectedNid, setSelectedNid] = useState<number | null>(null);
  const [acknowledged, setAcknowledged] = useState(false);
  const [stepPermille, setStepPermille] = useState(DEFAULT_STEP_PERMILLE);
  const [maxPermille, setMaxPermille] = useState(DEFAULT_MAX_PERMILLE);
  const [dwellMs, setDwellMs] = useState(DEFAULT_DWELL_MS);
  const [positionThresholdRad, setPositionThresholdRad] = useState(
    DEFAULT_POSITION_THRESHOLD_RAD,
  );
  const [velocityThresholdRadPerS, setVelocityThresholdRadPerS] = useState(
    DEFAULT_VELOCITY_THRESHOLD_RAD_PER_S,
  );
  const [kineticSampleMs, setKineticSampleMs] = useState(
    DEFAULT_KINETIC_SAMPLE_MS,
  );
  const [view, setView] = useState<FrictionCalibrationView>(EMPTY_VIEW);
  const [commandBusy, setCommandBusy] = useState(false);

  useEffect(() => {
    if (!connected) {
      setSelectedNid(null);
      setAcknowledged(false);
      return;
    }
    if (!motors.some((motor) => motor.node_id === selectedNid)) {
      setSelectedNid(motors[0]?.node_id ?? null);
      setAcknowledged(false);
    }
  }, [connected, motors, selectedNid]);

  useEffect(() => {
    onRunningChange(view.running);
  }, [onRunningChange, view.running]);

  useEffect(() => {
    if (!connected) {
      setView((current) => (current.running ? current : EMPTY_VIEW));
      return;
    }
    let alive = true;
    const poll = async () => {
      try {
        const next = await api.frictionCalibrationGet();
        if (alive) setView(next);
      } catch {
        // A tool switch/disconnect can race the final poll.
      }
    };
    void poll();
    const timer = window.setInterval(poll, view.running ? 80 : 500);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [connected, view.running]);

  const selected = motors.find((motor) => motor.node_id === selectedNid) ?? null;
  const phaseLabel = phaseName(view.phase, lang);
  const canStart =
    connected &&
    selected?.identity != null &&
    acknowledged &&
    !view.running &&
    !commandBusy;

  const start = async () => {
    if (!selected?.identity) return;
    const identity = selected.identity;
    const request: FrictionCalibrationRequest = {
      node_id: selected.node_id,
      expected_vendor_id: identity.vendor_id,
      expected_product_code: identity.product_code,
      expected_revision_number: identity.revision_number,
      expected_serial_number: identity.serial_number,
      torque_step_permille: stepPermille,
      max_torque_permille: maxPermille,
      step_dwell_ms: dwellMs,
      movement_position_threshold_rad: positionThresholdRad,
      movement_velocity_threshold_rad_per_s: velocityThresholdRadPerS,
      kinetic_sample_ms: kineticSampleMs,
    };
    setCommandBusy(true);
    try {
      setView(await api.frictionCalibrationStart(request));
      message.info(text("Calibration started", "摩擦力标定已启动"));
    } catch (error) {
      message.error(`${text("Start failed", "启动失败")}: ${errMsg(error)}`);
    } finally {
      setCommandBusy(false);
    }
  };

  const stop = async () => {
    setCommandBusy(true);
    try {
      setView(await api.frictionCalibrationStop());
      message.warning(text("Calibration cancelled and output disabled", "已取消标定并执行失能"));
    } catch (error) {
      message.error(`${text("Stop failed", "停止失败")}: ${errMsg(error)}`);
    } finally {
      setCommandBusy(false);
    }
  };

  const copyResult = async (result: FrictionCalibrationResult) => {
    const record = {
      schema: "hex-meow/friction-calibration-result/v1",
      semantics: "raw_command_domain_before_torque_factor",
      equation: "raw_command_nm = desired_torque_nm * torque_factor + friction_raw_nm",
      ...result,
    };
    try {
      await navigator.clipboard.writeText(JSON.stringify(record, null, 2));
      message.success(text("Result JSON copied", "结果 JSON 已复制"));
    } catch (error) {
      message.error(`${text("Copy failed", "复制失败")}: ${errMsg(error)}`);
    }
  };

  return (
    <div style={{ padding: 24, maxWidth: 1180, margin: "0 auto" }}>
      <Space direction="vertical" size="large" style={{ width: "100%" }}>
        <Alert
          type="warning"
          showIcon
          message={text("Developer calibration tool", "开发者标定工具")}
          description={text(
            "New-protocol motors only. This build measures friction but does not write 0x4001: a complete torque factor, CRC and HMAC must be issued together later.",
            "仅支持新协议电机。本版本只测量摩擦力，不写入 0x4001；后续必须把力矩系数、CRC 与 HMAC 作为完整记录统一签发。",
          )}
        />

        <Card title={text("Test setup", "测试设置")}>
          {!connected ? (
            <Empty description={text("Connect CAN first", "请先连接 CAN")} />
          ) : motors.length === 0 ? (
            <Empty description={text("No online new-protocol motor", "没有在线的新协议电机")} />
          ) : (
            <Space direction="vertical" size="middle" style={{ width: "100%" }}>
              <Select
                value={selectedNid}
                disabled={view.running}
                style={{ width: "100%", maxWidth: 520 }}
                options={motors.map((motor) => ({
                  value: motor.node_id,
                  label: `${motor.friendly_name} · ${nid2hex(motor.node_id)} · S/N ${motor.identity?.serial_number.toString(16).toUpperCase()}`,
                }))}
                onChange={(nodeId) => {
                  setSelectedNid(nodeId);
                  setAcknowledged(false);
                }}
              />

              <Row gutter={[16, 16]}>
                <Parameter
                  label={text("Torque step (‰)", "力矩步长（‰）")}
                  value={stepPermille}
                  min={1}
                  max={20}
                  step={1}
                  disabled={view.running}
                  onChange={setStepPermille}
                />
                <Parameter
                  label={text("Safety ceiling (‰)", "安全上限（‰）")}
                  value={maxPermille}
                  min={stepPermille}
                  max={200}
                  step={1}
                  disabled={view.running}
                  onChange={setMaxPermille}
                />
                <Parameter
                  label={text("Step dwell (ms)", "每步等待（ms）")}
                  value={dwellMs}
                  min={100}
                  max={1000}
                  step={50}
                  disabled={view.running}
                  onChange={setDwellMs}
                />
                <Parameter
                  label={text("Motion distance (rad)", "有效位移（rad）")}
                  value={positionThresholdRad}
                  min={0.001}
                  max={0.1}
                  step={0.001}
                  disabled={view.running}
                  onChange={setPositionThresholdRad}
                />
                <Parameter
                  label={text("Motion speed (rad/s)", "有效速度（rad/s）")}
                  value={velocityThresholdRadPerS}
                  min={0.01}
                  max={0.5}
                  step={0.01}
                  disabled={view.running}
                  onChange={setVelocityThresholdRadPerS}
                />
                <Parameter
                  label={text("Kinetic sample (ms)", "动摩擦采样（ms）")}
                  value={kineticSampleMs}
                  min={500}
                  max={5000}
                  step={100}
                  disabled={view.running}
                  onChange={setKineticSampleMs}
                />
              </Row>

              <Alert
                type="info"
                showIcon
                message={text(
                  "Kinetic passes are fixed at ±1 rad/s; no torque-factor or friction compensation is applied.",
                  "动摩擦固定以 ±1 rad/s 测试；测试过程不应用力矩系数，也不补偿摩擦力。",
                )}
              />
              <Checkbox
                checked={acknowledged}
                disabled={view.running}
                onChange={(event) => setAcknowledged(event.target.checked)}
              >
                {text(
                  "The shaft is unloaded and clear, the motor case is stable on a flat surface, and physical power removal is ready.",
                  "输出轴无负载且旋转范围内无人和杂物；电机外壳已平稳放在水平桌面上，并已准备好物理断电。",
                )}
              </Checkbox>
              <Space>
                <Button type="primary" danger disabled={!canStart} loading={commandBusy} onClick={start}>
                  {text("Start full friction test", "开始完整摩擦力测试")}
                </Button>
                <Button danger disabled={!view.running || commandBusy} onClick={stop}>
                  {text("Abort and disable", "中止并失能")}
                </Button>
              </Space>
            </Space>
          )}
        </Card>

        <Card
          title={text("Live calibration state", "标定实时状态")}
          extra={<Tag color={view.running ? "processing" : view.phase === "completed" ? "success" : "default"}>{phaseLabel}</Tag>}
        >
          <Progress percent={view.progress_percent} status={view.phase === "failed" ? "exception" : undefined} />
          <Row gutter={[16, 16]} style={{ marginTop: 16 }}>
            <LiveValue title={text("Command", "当前命令")} value={view.current_command_permille} suffix="‰" />
            <LiveValue title={text("Position", "累计位置")} value={view.position_rad} suffix=" rad" precision={3} />
            <LiveValue title={text("Velocity", "速度")} value={view.velocity_rad_per_s} suffix=" rad/s" precision={3} />
            <LiveValue title={text("Actual torque", "实际力矩")} value={view.actual_torque_permille} suffix="‰" />
            <LiveValue title={text("Motor temperature", "电机温度")} value={view.motor_temperature_c} suffix=" °C" precision={1} />
          </Row>
          {view.error && <Alert style={{ marginTop: 16 }} type={view.phase === "cancelled" ? "warning" : "error"} showIcon message={view.error} />}
          {view.cleanup_warning && <Alert style={{ marginTop: 16 }} type="error" showIcon message={view.cleanup_warning} />}
        </Card>

        {view.result && (
          <ResultCard result={view.result} lang={lang} onCopy={() => copyResult(view.result!)} />
        )}
      </Space>
    </div>
  );
}

function Parameter({
  label,
  value,
  min,
  max,
  step,
  disabled,
  onChange,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  disabled: boolean;
  onChange: (value: number) => void;
}) {
  return (
    <Col xs={24} sm={12} lg={8}>
      <Typography.Text type="secondary">{label}</Typography.Text>
      <InputNumber
        value={value}
        min={min}
        max={max}
        step={step}
        disabled={disabled}
        style={{ width: "100%", display: "block", marginTop: 4 }}
        onChange={(next) => onChange(Number(next ?? value))}
      />
    </Col>
  );
}

function LiveValue({
  title,
  value,
  suffix,
  precision = 0,
}: {
  title: string;
  value: number | null;
  suffix: string;
  precision?: number;
}) {
  return (
    <Col xs={12} md={8} lg={4}>
      <Statistic title={title} value={value ?? "—"} precision={value == null ? undefined : precision} suffix={value == null ? undefined : suffix} />
    </Col>
  );
}

function ResultCard({
  result,
  lang,
  onCopy,
}: {
  result: FrictionCalibrationResult;
  lang: "en" | "zh";
  onCopy: () => void;
}) {
  const text = (en: string, zh: string) => (lang === "zh" ? zh : en);
  return (
    <Card
      title={text("Measured raw command-domain friction", "测得的修正前原始命令域摩擦力")}
      extra={<Button onClick={onCopy}>{text("Copy JSON", "复制 JSON")}</Button>}
    >
      <Alert
        type="success"
        showIcon
        message="raw_command_nm = desired_torque_nm × torque_factor + friction_raw_nm"
        description={text(
          "Do not multiply or divide these four values by torque_factor before storing them in 0x4001:05/:06.",
          "写入 0x4001:05/:06 前，不得再用 torque_factor 乘或除这四个值。",
        )}
        style={{ marginBottom: 16 }}
      />
      <Descriptions bordered size="small" column={{ xs: 1, sm: 2 }}>
        <Descriptions.Item label={text("Static +", "正向静摩擦")}>{result.static_pos_raw_nm.toFixed(6)} Nm ({result.static_pos_permille}‰)</Descriptions.Item>
        <Descriptions.Item label={text("Static −", "反向静摩擦")}>{result.static_neg_raw_nm.toFixed(6)} Nm ({result.static_neg_permille}‰)</Descriptions.Item>
        <Descriptions.Item label={text("Kinetic +", "正向动摩擦")}>{result.kinetic_pos_raw_nm.toFixed(6)} Nm ({result.kinetic_pos_mean_permille.toFixed(3)}‰)</Descriptions.Item>
        <Descriptions.Item label={text("Kinetic −", "反向动摩擦")}>{result.kinetic_neg_raw_nm.toFixed(6)} Nm ({result.kinetic_neg_mean_permille.toFixed(3)}‰)</Descriptions.Item>
        <Descriptions.Item label={text("Mean speed +/−", "正/反向平均速度")}>{result.kinetic_pos_mean_speed_rad_per_s.toFixed(4)} / {result.kinetic_neg_mean_speed_rad_per_s.toFixed(4)} rad/s</Descriptions.Item>
        <Descriptions.Item label={text("Temperature", "标定温度")}>{result.calibration_temperature_c.toFixed(1)} °C</Descriptions.Item>
        <Descriptions.Item label={text("Peak torque", "峰值力矩")}>{result.peak_torque_nm.toFixed(6)} Nm</Descriptions.Item>
        <Descriptions.Item label={text("Runtime settings", "运行时设置")}>{text("Restored after cleanup", "清理后已恢复")}</Descriptions.Item>
      </Descriptions>
    </Card>
  );
}

function phaseName(phase: string, lang: "en" | "zh"): string {
  const labels: Record<string, [string, string]> = {
    idle: ["Idle", "空闲"],
    preparing: ["Preparing motor", "准备电机"],
    settling_initial: ["Waiting for stillness", "等待静止"],
    static_positive: ["Static friction +", "正向静摩擦"],
    settling_after_static_positive: ["Settling after static +", "正向静摩擦后静止"],
    static_negative: ["Static friction −", "反向静摩擦"],
    settling_after_static_negative: ["Settling after static −", "反向静摩擦后静止"],
    kinetic_positive: ["Kinetic friction + at 1 rad/s", "正向 1 rad/s 动摩擦"],
    settling_between_kinetic_passes: ["Settling between passes", "等待反向测试"],
    kinetic_negative: ["Kinetic friction − at 1 rad/s", "反向 1 rad/s 动摩擦"],
    settling_final: ["Final settling", "最终静止"],
    cleanup: ["Zero and disable", "归零并失能"],
    completed: ["Completed", "完成"],
    cancelled: ["Cancelled", "已取消"],
    failed: ["Failed", "失败"],
  };
  const value = labels[phase];
  return value ? value[lang === "zh" ? 1 : 0] : phase;
}
