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
  Table,
  Tag,
  Typography,
} from "antd";
import ReactECharts from "echarts-for-react";
import { api, errMsg } from "../api";
import { nid2hex } from "../format";
import { useI18n } from "../i18n";
import type {
  MotorInfo,
  TorqueCalibrationRequest,
  TorqueCalibrationResult,
  TorqueCalibrationView,
  TorqueFitPoint,
  TorquePassSummary,
} from "../types";

const DEFAULT_MASS_KG = 1.0;
const DEFAULT_CENTER_DISTANCE_M = 0.27;
const DEFAULT_SWEEP_SPEED_RAD_PER_S = 0.2;
const DEFAULT_SWEEP_ACCELERATION_RAD_PER_S2 = 0.4;
const DEFAULT_SWEEP_CYCLES = 5;
const DEFAULT_CONTROLLER_KP_NM_PER_RAD = 12.0;
const DEFAULT_CONTROLLER_KD_NM_S_PER_RAD = 2.0;
const DEFAULT_MAX_PERMILLE = 400;
const STANDARD_GRAVITY = 9.80665;

const EMPTY_VIEW: TorqueCalibrationView = {
  running: false,
  acceptance_active: false,
  traffic_active: false,
  phase: "idle",
  progress_percent: 0,
  node_id: null,
  current_command_permille: 0,
  current_command_nm: 0,
  angle_deg: null,
  target_angle_deg: null,
  trajectory_angle_deg: null,
  trajectory_velocity_rad_per_s: null,
  tracking_error_deg: null,
  velocity_rad_per_s: null,
  acceleration_rad_per_s2: null,
  actual_torque_permille: null,
  actual_torque_nm: null,
  motor_temperature_c: null,
  current_pass: 0,
  total_passes: 0,
  accepted_samples: 0,
  rejected_samples: 0,
  sample_valid: false,
  sample_rejection_reason: null,
  result: null,
  error: null,
  cleanup_warning: null,
};

export function MotorTorqueCalibrationPanel({
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
  const [massKg, setMassKg] = useState(DEFAULT_MASS_KG);
  const [centerDistanceM, setCenterDistanceM] = useState(
    DEFAULT_CENTER_DISTANCE_M,
  );
  const [sweepSpeed, setSweepSpeed] = useState(
    DEFAULT_SWEEP_SPEED_RAD_PER_S,
  );
  const [sweepAcceleration, setSweepAcceleration] = useState(
    DEFAULT_SWEEP_ACCELERATION_RAD_PER_S2,
  );
  const [sweepCycles, setSweepCycles] = useState(DEFAULT_SWEEP_CYCLES);
  const [controllerKp, setControllerKp] = useState(
    DEFAULT_CONTROLLER_KP_NM_PER_RAD,
  );
  const [controllerKd, setControllerKd] = useState(
    DEFAULT_CONTROLLER_KD_NM_S_PER_RAD,
  );
  const [maxPermille, setMaxPermille] = useState(DEFAULT_MAX_PERMILLE);
  const [acknowledged, setAcknowledged] = useState(false);
  const [view, setView] = useState<TorqueCalibrationView>(EMPTY_VIEW);
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
    onRunningChange(view.running || view.acceptance_active);
  }, [onRunningChange, view.acceptance_active, view.running]);

  useEffect(() => {
    if (!connected) return;
    let alive = true;
    const poll = async () => {
      try {
        const next = await api.torqueCalibrationGet();
        if (alive) setView(next);
      } catch {
        // Tool changes and disconnects may race the final passive poll.
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
  const maximumGravityNm = massKg * STANDARD_GRAVITY * centerDistanceM;
  const fitEdgeGravityNm = maximumGravityNm * Math.sin(Math.PI / 3);
  const nominalPermille =
    selected?.peak_torque_nm && selected.peak_torque_nm > 0
      ? (maximumGravityNm / selected.peak_torque_nm) * 1000
      : null;
  const rampDistanceDeg =
    ((sweepSpeed * sweepSpeed) / (2 * sweepAcceleration)) * (180 / Math.PI);
  const profileValid = rampDistanceDeg <= 4;
  const canStart =
    connected &&
    selected?.identity != null &&
    acknowledged &&
    profileValid &&
    !view.running &&
    !commandBusy;

  const startMeasurement = async () => {
    if (!selected?.identity) return;
    const identity = selected.identity;
    const request: TorqueCalibrationRequest = {
      node_id: selected.node_id,
      expected_vendor_id: identity.vendor_id,
      expected_product_code: identity.product_code,
      expected_revision_number: identity.revision_number,
      expected_serial_number: identity.serial_number,
      mass_kg: massKg,
      center_distance_m: centerDistanceM,
      sweep_speed_rad_per_s: sweepSpeed,
      sweep_acceleration_rad_per_s2: sweepAcceleration,
      sweep_cycles: sweepCycles,
      controller_kp_nm_per_rad: controllerKp,
      controller_kd_nm_s_per_rad: controllerKd,
      max_torque_permille: maxPermille,
    };
    setCommandBusy(true);
    try {
      setView(await api.torqueCalibrationStart(request));
      message.warning(
        text(
          "1000 Hz MIT trajectory started — keep the physical power cut ready",
          "1000 Hz MIT 双向轨迹已开始——请保持物理断电随时可用",
        ),
      );
    } catch (error) {
      message.error(`${text("Start failed", "启动失败")}: ${errMsg(error)}`);
    } finally {
      setCommandBusy(false);
    }
  };

  const startAcceptance = async () => {
    setCommandBusy(true);
    try {
      setView(await api.torqueCalibrationAcceptanceStart());
      message.warning(
        text(
          "Gravity-compensation acceptance is active",
          "重力补偿验收模式已启用",
        ),
      );
    } catch (error) {
      message.error(
        `${text("Acceptance failed", "验收模式启动失败")}: ${errMsg(error)}`,
      );
    } finally {
      setCommandBusy(false);
    }
  };

  const stop = async () => {
    setCommandBusy(true);
    try {
      setView(await api.torqueCalibrationStop());
      message.warning(
        text(
          "Output zeroed, motor Disabled, periodic host CAN transmission stopped",
          "输出已归零、电机已失能，主机周期 CAN 发送已停止",
        ),
      );
    } catch (error) {
      message.error(`${text("Stop failed", "停止失败")}: ${errMsg(error)}`);
    } finally {
      setCommandBusy(false);
    }
  };

  const copyResult = async (result: TorqueCalibrationResult) => {
    const record = {
      schema: "hex-meow/gravity-torque-calibration-result/v3",
      equation: "raw_command_nm = desired_physical_torque_nm * torque_factor",
      measurement_source: "raw 0x4577; 0x4001 is never read or written",
      method: "paired constant-speed 1000 Hz host-trajectory compressed-MIT traversals",
      control_law:
        "Tff = nominal_gravity + host_Kp*(trajectory_position-actual_position); motor MIT Kd closes trajectory velocity against firmware velocity",
      safety_velocity_estimator: "5 ms Q8.24 position window; 8 rad/s one-frame anomaly trip",
      fixture_rod_mass_ignored: true,
      measurement_overshoot_guard_deg: 72,
      acceptance_angle_deg: 75,
      hard_angle_deg: 80,
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
    <div style={{ padding: 24, maxWidth: 1380, margin: "0 auto" }}>
      <Space direction="vertical" size="large" style={{ width: "100%" }}>
        <Alert
          type="warning"
          showIcon
          message={text(
            "Loaded developer calibration — operator execution only",
            "带负载开发者标定——仅允许人工执行",
          )}
          description={text(
            "Rust generates the bounded trajectory and streams compressed MIT at 1000 Hz. Tff contains gravity plus host position correction; velocity damping uses the motor's internal MIT Kd and firmware velocity. Sampling and the 1 rad/s early trip use a 5 ms position window, while an independent 8 rad/s one-frame anomaly trip remains. Only constant-speed samples inside ±60° are fitted.",
            "Rust 生成有限速/限加速度轨迹并以 1000 Hz 发送 compressed MIT。Tff 包含重力前馈与主机位置修正，速度阻尼改用电机内部 MIT Kd 和固件速度。采样及 1 rad/s 提前保护采用 5 ms 位置窗口，同时保留独立的 8 rad/s 单帧异常保护；只拟合 ±60° 内的恒速样本。",
          )}
        />

        <Card title={text("Fixture and host MIT trajectory", "工装与主机 MIT 轨迹参数")}>
          {!connected ? (
            <Empty description={text("Connect CAN first", "请先连接 CAN")} />
          ) : motors.length === 0 ? (
            <Empty
              description={text(
                "No online new-protocol motor",
                "没有在线的新协议电机",
              )}
            />
          ) : (
            <Space direction="vertical" size="middle" style={{ width: "100%" }}>
              <Select
                value={selectedNid}
                disabled={view.running}
                style={{ width: "100%", maxWidth: 620 }}
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
                <Parameter label={text("Hanging mass (kg)", "悬挂质量（kg）")} value={massKg} min={0.05} max={20} step={0.01} disabled={view.running} onChange={setMassKg} />
                <Parameter label={text("Center distance (m)", "中心距（m）")} value={centerDistanceM} min={0.01} max={2} step={0.001} disabled={view.running} onChange={setCenterDistanceM} />
                <Parameter label={text("Sweep speed (rad/s)", "扫掠速度（rad/s）")} value={sweepSpeed} min={0.05} max={0.5} step={0.01} disabled={view.running} onChange={setSweepSpeed} />
                <Parameter label={text("Acceleration (rad/s²)", "加速度（rad/s²）")} value={sweepAcceleration} min={0.1} max={2} step={0.05} disabled={view.running} onChange={setSweepAcceleration} />
                <Parameter label={text("Round trips", "往返周期数")} value={sweepCycles} min={1} max={5} step={1} disabled={view.running} onChange={setSweepCycles} />
                <Parameter label={text("Host Kp (Nm/rad)", "主机 Kp（Nm/rad）")} value={controllerKp} min={1} max={20} step={0.5} disabled={view.running} onChange={setControllerKp} />
                <Parameter label={text("Motor MIT Kd (Nm·s/rad)", "电机 MIT Kd（Nm·s/rad）")} value={controllerKd} min={0.2} max={5} step={0.1} disabled={view.running} onChange={setControllerKd} />
                <Parameter label={text("Torque ceiling (‰)", "力矩硬上限（‰）")} value={maxPermille} min={100} max={500} step={5} disabled={view.running} onChange={setMaxPermille} />
              </Row>

              <Descriptions bordered size="small" column={{ xs: 1, sm: 2, lg: 4 }}>
                <Descriptions.Item label={text("Maximum gravity", "90° 最大重力矩")}>{maximumGravityNm.toFixed(6)} Nm</Descriptions.Item>
                <Descriptions.Item label={text("Gravity at fit edge", "60° 拟合边缘重力矩")}>{fitEdgeGravityNm.toFixed(6)} Nm</Descriptions.Item>
                <Descriptions.Item label={text("Nominal motor command", "90° 名义电机命令")}>{nominalPermille == null ? "—" : `${nominalPermille.toFixed(1)}‰`}</Descriptions.Item>
                <Descriptions.Item label={text("Acceleration distance", "单侧加速距离")}><Tag color={profileValid ? "success" : "error"}>{rampDistanceDeg.toFixed(2)}° / 4.00°</Tag></Descriptions.Item>
                <Descriptions.Item label={text("Measurement geometry", "测量角度")}>{text("endpoints ±65° / fit ±60°", "端点 ±65° / 拟合 ±60°")}</Descriptions.Item>
                <Descriptions.Item label={text("Protection", "保护角度")}>{text("overshoot 72° / hard 80°", "越界 72° / 硬保护 80°")}</Descriptions.Item>
                <Descriptions.Item label={text("Raw source", "原始数据源")}>0x4577 i16 ‰ × 0x4576 Nm</Descriptions.Item>
                <Descriptions.Item label={text("Expected passes", "总测量趟数")}>{sweepCycles * 2}</Descriptions.Item>
                <Descriptions.Item label={text("Controller", "控制器")}>1000 Hz · Tff = gravity + host Kp·e<sub>q</sub> · motor Kd·e<sub>v</sub></Descriptions.Item>
                <Descriptions.Item label={text("Fast trips", "快速保护")}>{text("5 ms early/hard 1/2 rad/s · one-frame anomaly 8 rad/s · TPDO 30 ms", "5 ms 提前/硬保护 1/2 rad/s · 单帧异常 8 rad/s · TPDO 30 ms")}</Descriptions.Item>
              </Descriptions>

              {!profileValid && (
                <Alert
                  type="error"
                  showIcon
                  message={text(
                    "The acceleration ramp exceeds the 4° budget between the fit edge and endpoint. Increase acceleration or reduce speed.",
                    "加速距离超过拟合边缘到端点之间预留的 4°；请提高加速度或降低速度。",
                  )}
                />
              )}

              <Checkbox checked={acknowledged} disabled={view.running} onChange={(event) => setAcknowledged(event.target.checked)}>
                {text(
                  "The lever can swing freely through both ±65° endpoints, nobody is in the sweep plane, the load data above are correct, and I am ready to cut motor power immediately.",
                  "杠杆可自由通过两侧 ±65° 端点；摆动平面内无人和杂物；上方负载参数正确；我已准备随时立即切断电机电源。",
                )}
              </Checkbox>

              <Space wrap>
                <Button type="primary" danger disabled={!canStart} loading={commandBusy} onClick={startMeasurement}>
                  {text("Start paired MIT sweep", "开始 MIT 双向配对扫掠")}
                </Button>
                <Button danger disabled={!view.running || commandBusy} onClick={stop}>
                  {text("Emergency software stop", "软件紧急停止")}
                </Button>
              </Space>
            </Space>
          )}
        </Card>

        <Card
          title={text("Live operation", "实时运行状态")}
          extra={
            <Space>
              <Tag color={view.traffic_active ? "processing" : "default"}>
                {view.traffic_active ? text("Host TX active", "主机发送中") : text("Host TX silent", "主机已静默")}
              </Tag>
              <Tag color={view.phase === "failed" ? "error" : view.running ? "processing" : "default"}>{phaseName(view.phase, lang)}</Tag>
            </Space>
          }
        >
          <Progress percent={view.progress_percent} status={view.phase === "failed" ? "exception" : undefined} />
          <Row gutter={[16, 16]} style={{ marginTop: 16 }}>
            <LiveValue title={text("Angle", "相对最低点角度")} value={view.angle_deg} suffix="°" precision={2} />
            <LiveValue title={text("Endpoint target", "端点目标")} value={view.target_angle_deg} suffix="°" precision={1} />
            <LiveValue title={text("Trajectory position", "轨迹位置")} value={view.trajectory_angle_deg} suffix="°" precision={2} />
            <LiveValue title={text("Tracking error", "跟踪误差")} value={view.tracking_error_deg} suffix="°" precision={2} />
            <LiveValue title={text("5 ms actual velocity", "5 ms 实际速度")} value={view.velocity_rad_per_s} suffix=" rad/s" precision={3} />
            <LiveValue title={text("Trajectory velocity", "轨迹速度")} value={view.trajectory_velocity_rad_per_s} suffix=" rad/s" precision={3} />
            <LiveValue title={text("Filtered acceleration", "滤波加速度")} value={view.acceleration_rad_per_s2} suffix=" rad/s²" precision={3} />
            <LiveValue title={text("Raw 0x4577", "原始 0x4577")} value={view.actual_torque_permille} suffix="‰" />
            <LiveValue title={text("Raw torque", "原始力矩")} value={view.actual_torque_nm} suffix=" Nm" precision={4} />
            <LiveValue title={text("MIT Tff command", "MIT Tff 命令")} value={view.current_command_nm} suffix=" Nm" precision={4} />
            <LiveValue title={text("Temperature", "电机温度")} value={view.motor_temperature_c} suffix=" °C" precision={1} />
            <LiveValue title={text("Current pass", "当前趟")} value={view.current_pass || null} suffix={` / ${view.total_passes}`} />
            <LiveValue title={text("Accepted samples", "已接受样本")} value={view.accepted_samples} suffix="" />
            <LiveValue title={text("Rejected samples", "已拒绝样本")} value={view.rejected_samples} suffix="" />
          </Row>
          {view.running && view.current_pass > 0 && (
            <Alert
              style={{ marginTop: 16 }}
              type={view.sample_valid ? "success" : "info"}
              showIcon
              message={
                view.sample_valid
                  ? text("Current 0x4577 sample accepted", "当前 0x4577 样本已接受")
                  : text("Current sample not used", "当前样本未用于拟合")
              }
              description={view.sample_rejection_reason ?? text("Inside constant-speed fit window", "位于恒速拟合窗口内")}
            />
          )}
          {view.error && <Alert style={{ marginTop: 16 }} type="error" showIcon message={view.error} />}
          {view.cleanup_warning && <Alert style={{ marginTop: 16 }} type="error" showIcon message={view.cleanup_warning} />}
        </Card>

        {view.result && (
          <ResultCard
            result={view.result}
            lang={lang}
            acceptanceActive={view.acceptance_active}
            connected={connected}
            busy={commandBusy}
            onAcceptance={startAcceptance}
            onStop={stop}
            onCopy={() => copyResult(view.result!)}
          />
        )}
      </Space>
    </div>
  );
}

function Parameter({ label, value, min, max, step, disabled, onChange }: { label: string; value: number; min: number; max: number; step: number; disabled: boolean; onChange: (value: number) => void }) {
  return (
    <Col xs={24} sm={12} lg={8} xl={4}>
      <Typography.Text type="secondary">{label}</Typography.Text>
      <InputNumber value={value} min={min} max={max} step={step} disabled={disabled} style={{ width: "100%", display: "block", marginTop: 4 }} onChange={(next) => onChange(Number(next ?? value))} />
    </Col>
  );
}

function LiveValue({ title, value, suffix, precision = 0 }: { title: string; value: number | null; suffix: string; precision?: number }) {
  return (
    <Col xs={12} md={8} lg={6} xl={4}>
      <Statistic title={title} value={value ?? "—"} precision={value == null ? undefined : precision} suffix={value == null ? undefined : suffix} />
    </Col>
  );
}

function ResultCard({ result, lang, acceptanceActive, connected, busy, onAcceptance, onStop, onCopy }: { result: TorqueCalibrationResult; lang: "en" | "zh"; acceptanceActive: boolean; connected: boolean; busy: boolean; onAcceptance: () => void; onStop: () => void; onCopy: () => void }) {
  const text = (en: string, zh: string) => (lang === "zh" ? zh : en);
  const torqueChart = {
    tooltip: { trigger: "axis" },
    legend: { data: [text("Forward raw", "正向原始"), text("Reverse raw", "反向原始"), text("Paired midpoint", "配对中线"), text("Fit", "拟合")] },
    grid: { left: 64, right: 24, top: 64, bottom: 52 },
    xAxis: { type: "value", name: text("Angle (deg)", "角度（deg）"), min: -62, max: 62 },
    yAxis: { type: "value", name: text("Raw torque (Nm)", "原始力矩（Nm）") },
    series: [
      chartSeries(text("Forward raw", "正向原始"), result.fit_points, "forward_raw_nm", "#d46b08"),
      chartSeries(text("Reverse raw", "反向原始"), result.fit_points, "reverse_raw_nm", "#1677ff"),
      chartSeries(text("Paired midpoint", "配对中线"), result.fit_points, "midpoint_raw_nm", "#722ed1"),
      chartSeries(text("Fit", "拟合"), result.fit_points, "fitted_raw_nm", "#389e0d"),
    ],
  };
  const diagnosticChart = {
    tooltip: { trigger: "axis" },
    legend: { data: [text("Friction half-difference (raw)", "摩擦半差（原始）"), text("Corrected residual", "修正后残差")] },
    grid: { left: 64, right: 24, top: 64, bottom: 52 },
    xAxis: { type: "value", name: text("Angle (deg)", "角度（deg）"), min: -62, max: 62 },
    yAxis: { type: "value", name: "Nm" },
    series: [
      chartSeries(text("Friction half-difference (raw)", "摩擦半差（原始）"), result.fit_points, "friction_half_difference_raw_nm", "#fa8c16"),
      chartSeries(text("Corrected residual", "修正后残差"), result.fit_points, "corrected_residual_nm", "#eb2f96"),
    ],
  };
  const fitColumns = [
    { title: text("Angle", "角度"), dataIndex: "angle_deg", render: (value: number) => `${value.toFixed(0)}°` },
    { title: text("Gravity", "理论重力矩"), dataIndex: "gravity_torque_nm", render: fixed(4) },
    { title: text("Forward raw", "正向原始"), dataIndex: "forward_raw_nm", render: fixed(4) },
    { title: text("Reverse raw", "反向原始"), dataIndex: "reverse_raw_nm", render: fixed(4) },
    { title: text("Midpoint", "配对中线"), dataIndex: "midpoint_raw_nm", render: fixed(4) },
    { title: text("Fit", "拟合值"), dataIndex: "fitted_raw_nm", render: fixed(4) },
    { title: text("Friction ½Δ", "摩擦半差"), dataIndex: "friction_half_difference_raw_nm", render: fixed(4) },
    { title: text("Corrected residual", "修正后残差"), dataIndex: "corrected_residual_nm", render: fixed(5) },
    { title: text("Fwd σ / n", "正向 σ / n"), render: (_: unknown, point: TorqueFitPoint) => `${point.forward_stddev_raw_nm.toFixed(4)} / ${point.forward_samples}` },
    { title: text("Rev σ / n", "反向 σ / n"), render: (_: unknown, point: TorqueFitPoint) => `${point.reverse_stddev_raw_nm.toFixed(4)} / ${point.reverse_samples}` },
  ];
  const passColumns = [
    { title: text("Cycle", "周期"), dataIndex: "cycle" },
    { title: text("Direction", "方向"), dataIndex: "direction", render: (value: TorquePassSummary["direction"]) => value === "forward" ? text("Forward", "正向") : text("Reverse", "反向") },
    { title: text("Accepted", "接受"), dataIndex: "accepted_samples" },
    { title: text("Rejected", "拒绝"), dataIndex: "rejected_samples" },
    { title: text("Mean velocity", "平均速度"), dataIndex: "mean_velocity_rad_per_s", render: fixed(4) },
    { title: text("Velocity σ", "速度 σ"), dataIndex: "velocity_stddev_rad_per_s", render: fixed(4) },
    { title: text("Peak |velocity|", "峰值 |速度|"), dataIndex: "peak_absolute_velocity_rad_per_s", render: fixed(4) },
    { title: text("Peak tracking error", "峰值跟踪误差"), dataIndex: "peak_tracking_error_deg", render: (value: number) => `${value.toFixed(2)}°` },
    { title: text("Raw torque range", "原始力矩范围"), render: (_: unknown, pass: TorquePassSummary) => `${pass.minimum_raw_torque_nm.toFixed(4)} … ${pass.maximum_raw_torque_nm.toFixed(4)} Nm` },
  ];

  return (
    <Card title={text("Torque-factor result and diagnostics", "力矩修正系数与诊断结果")} extra={<Button onClick={onCopy}>{text("Copy JSON", "复制 JSON")}</Button>}>
      <Alert
        type="success"
        showIcon
        message={`raw_command_nm = desired_physical_torque_nm × ${result.torque_factor.toFixed(8)}`}
        description={text(
          "The coefficient is fitted from the midpoint of raw 0x4577 values at the same angle in opposite travel directions. Acceptance applies it to gravity only and adds no friction compensation.",
          "系数来自相同角度、相反运动方向的原始 0x4577 配对中线。验收仅对重力矩应用该系数，不叠加摩擦力补偿。",
        )}
        style={{ marginBottom: 16 }}
      />
      <Descriptions bordered size="small" column={{ xs: 1, sm: 2, lg: 4 }}>
        <Descriptions.Item label="torque_factor">{result.torque_factor.toFixed(8)}</Descriptions.Item>
        <Descriptions.Item label={text("Positive / negative", "正角 / 负角系数")}>{result.positive_torque_factor.toFixed(6)} / {result.negative_torque_factor.toFixed(6)}</Descriptions.Item>
        <Descriptions.Item label={text("Directional asymmetry", "正负角差异")}>{result.directional_asymmetry_percent.toFixed(3)}%</Descriptions.Item>
        <Descriptions.Item label={text("Physical fit RMSE", "物理量拟合 RMSE")}>{result.torque_fit_rmse_nm.toFixed(6)} Nm</Descriptions.Item>
        <Descriptions.Item label={text("Mean friction half-width", "平均摩擦半宽")}>{result.mean_hysteresis_half_width_raw_nm.toFixed(6)} Nm</Descriptions.Item>
        <Descriptions.Item label={text("Forward / reverse offset", "正向 / 反向摩擦偏置")}>{result.forward_friction_offset_raw_nm.toFixed(5)} / {result.reverse_friction_offset_raw_nm.toFixed(5)} Nm</Descriptions.Item>
        <Descriptions.Item label={text("Accepted / rejected", "接受 / 拒绝样本")}>{result.accepted_sample_count} / {result.rejected_sample_count}</Descriptions.Item>
        <Descriptions.Item label={text("Temperature", "标定温度")}>{result.calibration_temperature_c.toFixed(1)} °C</Descriptions.Item>
        <Descriptions.Item label={text("Host trajectory", "主机轨迹")}>{result.control_rate_hz} Hz · {result.sweep_speed_rad_per_s.toFixed(3)} rad/s · {result.sweep_acceleration_rad_per_s2.toFixed(3)} rad/s²</Descriptions.Item>
        <Descriptions.Item label={text("Position / damping", "位置 / 阻尼")}>Host Kp {result.controller_kp_nm_per_rad.toFixed(2)} Nm/rad · motor MIT Kd {result.controller_kd_nm_s_per_rad.toFixed(2)} Nm·s/rad</Descriptions.Item>
        <Descriptions.Item label={text("Passes", "测量趟数")}>{result.sweep_cycles} × 2 = {result.sweep_cycles * 2}</Descriptions.Item>
        <Descriptions.Item label={text("Measurement window", "测量窗口")}>±{result.fit_angle_limit_deg.toFixed(0)}° / ±{result.sweep_endpoint_deg.toFixed(0)}°</Descriptions.Item>
        <Descriptions.Item label={text("Peak / gravity torque", "峰值 / 最大重力矩")}>{result.peak_torque_nm.toFixed(3)} / {result.maximum_gravity_torque_nm.toFixed(4)} Nm</Descriptions.Item>
      </Descriptions>

      <Row gutter={[16, 16]} style={{ marginTop: 16 }}>
        <Col xs={24} xl={12}><Card size="small" title={text("Raw 0x4577 pairing and fit", "原始 0x4577 配对与拟合")}><ReactECharts option={torqueChart} notMerge style={{ height: 360 }} /></Card></Col>
        <Col xs={24} xl={12}><Card size="small" title={text("Friction and fit residual", "摩擦与拟合残差")}><ReactECharts option={diagnosticChart} notMerge style={{ height: 360 }} /></Card></Col>
      </Row>

      <Typography.Title level={5} style={{ marginTop: 20 }}>{text("Per-pass quality", "每趟数据质量")}</Typography.Title>
      <Table<TorquePassSummary> size="small" pagination={false} rowKey={(pass) => `${pass.cycle}-${pass.direction}`} dataSource={result.pass_summaries} columns={passColumns} scroll={{ x: 1120 }} />

      <Space wrap style={{ margin: "18px 0" }}>
        {acceptanceActive ? (
          <Button type="primary" danger disabled={busy || !connected} onClick={onStop}>{text("End acceptance and stop host TX", "结束验收并停止主机发送")}</Button>
        ) : (
          <Button type="primary" danger disabled={busy || !connected} onClick={onAcceptance}>{text("Enter gravity-compensation acceptance", "进入重力补偿验收模式")}</Button>
        )}
        <Typography.Text type="secondary">{text("Acceptance streams 100 Hz compressed-MIT PDO; kp=kd=0; ±80° remains a hard trip.", "验收以 100 Hz 发送 compressed-MIT PDO；kp=kd=0；±80° 仍为硬保护。")}</Typography.Text>
      </Space>

      <Typography.Title level={5}>{text("Angle-bin details", "角度分箱明细")}</Typography.Title>
      <Table<TorqueFitPoint> size="small" pagination={false} rowKey={(point) => point.angle_deg} dataSource={result.fit_points} columns={fitColumns} scroll={{ x: 1280 }} />
    </Card>
  );
}

function chartSeries(name: string, points: TorqueFitPoint[], key: keyof TorqueFitPoint, color: string) {
  return {
    name,
    type: "line",
    showSymbol: true,
    symbolSize: 6,
    itemStyle: { color },
    lineStyle: { color, width: 2 },
    data: points.map((point) => [point.angle_deg, Number(point[key])]),
  };
}

function fixed(digits: number) {
  return (value: number) => value.toFixed(digits);
}

function phaseName(phase: string, lang: "en" | "zh"): string {
  const labels: Record<string, [string, string]> = {
    idle: ["Idle", "空闲"],
    preparing: ["Preparing motor", "准备电机"],
    capturing_bottom: ["Capturing gravity low point", "采集重力最低点"],
    positioning_start: ["MIT trajectory to -65°", "MIT 轨迹定位到 -65°"],
    sweep_forward: ["MIT sweep -65° → +65°", "MIT 扫掠 -65° → +65°"],
    sweep_reverse: ["MIT sweep +65° → -65°", "MIT 扫掠 +65° → -65°"],
    returning_bottom: ["Returning to gravity low point", "返回重力最低点"],
    fitting: ["Pairing and fitting raw 0x4577", "配对并拟合原始 0x4577"],
    acceptance: ["Gravity-compensation acceptance", "重力补偿验收"],
    cleanup: ["Zero, Disabled and silence", "归零、失能并静默"],
    measured: ["Measured", "测量完成"],
    cancelled: ["Cancelled", "已取消"],
    failed: ["Failed", "失败"],
  };
  const value = labels[phase];
  return value ? value[lang === "zh" ? 1 : 0] : phase;
}
