import { useMemo, useState } from "react";
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  Checkbox,
  Input,
  InputNumber,
  Space,
  Tag,
  Typography,
} from "antd";
import { api, errMsg } from "../api";
import type { LiftState } from "../types";

const STATE_DISARMED = 0;
const STATE_ARMED = 1;
const STATE_SEEKING_LOWER = 2;
const STATE_LOWER_FOUND = 3;
const STATE_SEEKING_UPPER = 4;
const STATE_COMPLETE = 5;
const STATE_FAULT = 0x80;

const FLAG_LOWER_VALID = 1 << 1;
const FLAG_UPPER_VALID = 1 << 2;
const FLAG_OUTPUT_ACTIVE = 1 << 3;

const SENSOR_REQUIRED = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3);
const SENSOR_ALERT = 1 << 4;

function stateName(value: number): string {
  switch (value) {
    case STATE_DISARMED:
      return "Disarmed";
    case STATE_ARMED:
      return "Armed";
    case STATE_SEEKING_LOWER:
      return "正在寻找下端";
    case STATE_LOWER_FOUND:
      return "下端已找到";
    case STATE_SEEKING_UPPER:
      return "正在寻找上端";
    case STATE_COMPLETE:
      return "两端完成";
    case STATE_FAULT:
      return "Fault";
    default:
      return `Unknown (${value})`;
  }
}

function today(): string {
  const now = new Date();
  const year = now.getFullYear();
  const month = String(now.getMonth() + 1).padStart(2, "0");
  const day = String(now.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function hex(value: number, width = 8): string {
  return (
    "0x" +
    (Math.trunc(value) >>> 0).toString(16).toUpperCase().padStart(width, "0")
  );
}

export function LiftFactoryCalibrationCard({
  state,
  connected,
  attached,
}: {
  state: LiftState;
  connected: boolean;
  attached: boolean;
}) {
  const { message } = AntdApp.useApp();
  const factory = state.factory_calibration;
  const [busy, setBusy] = useState<string | null>(null);
  const [motionAcknowledged, setMotionAcknowledged] = useState(false);
  const [commitAcknowledged, setCommitAcknowledged] = useState(false);
  const [lowerReadingM, setLowerReadingM] = useState<number | null>(null);
  const [upperReadingM, setUpperReadingM] = useState<number | null>(null);
  const [manufactureDate, setManufactureDate] = useState(today());
  const [calibrationDate, setCalibrationDate] = useState(today());
  const [stationId, setStationId] = useState(1);

  const sensorsHealthy =
    (state.sensor_status & SENSOR_REQUIRED) === SENSOR_REQUIRED &&
    (state.sensor_status & SENSOR_ALERT) === 0;
  const commonReady =
    connected &&
    attached &&
    state.online &&
    state.tpdo1_fresh &&
    state.tpdo2_fresh &&
    state.nmt_state === 0x05 &&
    sensorsHealthy &&
    state.detailed_fault === 0;
  const outputActive =
    (factory.flags & FLAG_OUTPUT_ACTIVE) !== 0 ||
    state.duty_command_permille !== 0;
  const endpointsValid =
    (factory.flags & (FLAG_LOWER_VALID | FLAG_UPPER_VALID)) ===
    (FLAG_LOWER_VALID | FLAG_UPPER_VALID);

  const preview = useMemo(() => {
    if (
      lowerReadingM == null ||
      upperReadingM == null ||
      !Number.isFinite(lowerReadingM) ||
      !Number.isFinite(upperReadingM)
    ) {
      return null;
    }
    const travelM = Math.abs(upperReadingM - lowerReadingM);
    const countDelta = Math.abs(factory.upper_count - factory.lower_count);
    if (travelM <= 0 || countDelta <= 0) return null;
    const countsPerMeter = countDelta / travelM;
    return {
      travelM,
      countDelta,
      countsPerMeter,
      correction: countsPerMeter / (10000 / 0.7),
    };
  }, [
    factory.lower_count,
    factory.upper_count,
    lowerReadingM,
    upperReadingM,
  ]);

  const run = async (name: string, action: () => Promise<unknown>) => {
    setBusy(name);
    try {
      await action();
      message.success(`${name} 命令已确认`);
    } catch (error) {
      message.error(`${name} 失败：${errMsg(error)}`, 0);
    } finally {
      setBusy(null);
    }
  };

  const commit = async () => {
    if (lowerReadingM == null || upperReadingM == null) return;
    setBusy("写入 NVS");
    try {
      const result = await api.liftFactoryCalibrationCommit(
        lowerReadingM,
        upperReadingM,
        manufactureDate,
        calibrationDate,
        stationId
      );
      message.success(
        `标定已持久化并重启回读：${result.counts_per_meter.toFixed(
          3
        )} count/m，修正 ${result.transmission_correction.toFixed(
          6
        )}，CRC ${hex(result.crc32)}`,
        0
      );
      setCommitAcknowledged(false);
    } catch (error) {
      message.error(`写入/复位回读失败：${errMsg(error)}`, 0);
    } finally {
      setBusy(null);
    }
  };

  const canArm =
    commonReady &&
    !outputActive &&
    motionAcknowledged &&
    (factory.state === STATE_DISARMED || factory.state === STATE_COMPLETE);

  return (
    <Card
      title="lift_a70 厂家两端标定"
      extra={
        <Space>
          <Tag color="purple">ABI {factory.abi}</Tag>
          <Tag color={factory.state === STATE_FAULT ? "red" : "blue"}>
            {stateName(factory.state)}
          </Tag>
        </Space>
      }
    >
      <Space direction="vertical" size="middle" style={{ width: "100%" }}>
        <Alert
          type="warning"
          showIcon
          message="这是自主硬限位运动，不是安全等级功能"
          description="保持电源限流和物理断电手段。Seek 开始后固件会运行到检测到硬限位或 120 秒超时；GUI/电脑掉电不会替代物理急停。"
        />

        <Checkbox
          checked={motionAcknowledged}
          onChange={(event) => setMotionAcknowledged(event.target.checked)}
        >
          区域已清空、电源已限流、物理断电触手可及
        </Checkbox>

        <Space wrap>
          <Button
            disabled={!canArm || busy != null}
            loading={busy === "Arm"}
            onClick={() =>
              void run("Arm", () => api.liftFactoryCalibrationArm())
            }
          >
            1 · Arm
          </Button>
          <Button
            type="primary"
            disabled={
              !commonReady ||
              factory.state !== STATE_ARMED ||
              busy != null
            }
            loading={busy === "寻找下端"}
            onClick={() =>
              void run("寻找下端", () =>
                api.liftFactoryCalibrationSeekLower()
              )
            }
          >
            2 · 寻找下端
          </Button>
          <Button
            type="primary"
            disabled={
              !commonReady ||
              factory.state !== STATE_LOWER_FOUND ||
              busy != null
            }
            loading={busy === "寻找上端"}
            onClick={() =>
              void run("寻找上端", () =>
                api.liftFactoryCalibrationSeekUpper()
              )
            }
          >
            3 · 寻找上端
          </Button>
          <Button
            danger
            disabled={
              busy != null ||
              factory.state === STATE_DISARMED ||
              factory.state === STATE_COMPLETE
            }
            loading={busy === "Abort"}
            onClick={() =>
              void run("Abort", () => api.liftFactoryCalibrationAbort())
            }
          >
            Abort / Coast
          </Button>
          <Button
            danger
            disabled={factory.state !== STATE_FAULT || busy != null}
            loading={busy === "Clear fault"}
            onClick={() =>
              void run("Clear fault", () =>
                api.liftFactoryCalibrationClearFault()
              )
            }
          >
            Clear fault
          </Button>
        </Space>

        <Typography.Text>
          下端 count：{factory.lower_count.toLocaleString()}　上端 count：
          {factory.upper_count.toLocaleString()}　当前 duty：
          {state.duty_command_permille}‰
        </Typography.Text>

        <Space wrap align="end">
          <label>
            <Typography.Text>下端米尺读数（m）</Typography.Text>
            <br />
            <InputNumber
              value={lowerReadingM}
              min={0}
              max={10}
              step={0.001}
              precision={3}
              onChange={setLowerReadingM}
            />
          </label>
          <label>
            <Typography.Text>上端米尺读数（m）</Typography.Text>
            <br />
            <InputNumber
              value={upperReadingM}
              min={0}
              max={10}
              step={0.001}
              precision={3}
              onChange={setUpperReadingM}
            />
          </label>
          <label>
            <Typography.Text>组装日期</Typography.Text>
            <br />
            <Input
              type="date"
              value={manufactureDate}
              onChange={(event) => setManufactureDate(event.target.value)}
            />
          </label>
          <label>
            <Typography.Text>标定日期</Typography.Text>
            <br />
            <Input
              type="date"
              value={calibrationDate}
              onChange={(event) => setCalibrationDate(event.target.value)}
            />
          </label>
          <label>
            <Typography.Text>工位 ID</Typography.Text>
            <br />
            <InputNumber
              value={stationId}
              min={0}
              max={0xffffffff}
              precision={0}
              onChange={(value) => setStationId(value ?? 0)}
            />
          </label>
        </Space>

        {preview && (
          <Alert
            type={
              preview.travelM > 0 &&
              preview.travelM <= 0.7 &&
              preview.correction >= 0.5 &&
              preview.correction <= 1.5
                ? "info"
                : "error"
            }
            showIcon
            message={`预览：行程 ${preview.travelM.toFixed(
              4
            )} m，Δcount ${preview.countDelta.toLocaleString()}`}
            description={`counts/m ${preview.countsPerMeter.toFixed(
              3
            )}，transmission correction ${preview.correction.toFixed(6)}`}
          />
        )}

        <Checkbox
          checked={commitAcknowledged}
          onChange={(event) => setCommitAcknowledged(event.target.checked)}
        >
          已人工复核两次米尺读数和两个编码器快照；允许一次性写入 NVS、复位并回读
        </Checkbox>
        <Button
          type="primary"
          disabled={
            busy != null ||
            factory.state !== STATE_COMPLETE ||
            !endpointsValid ||
            outputActive ||
            preview == null ||
            preview.travelM > 0.7 ||
            preview.correction < 0.5 ||
            preview.correction > 1.5 ||
            !commitAcknowledged
          }
          loading={busy === "写入 NVS"}
          onClick={() => void commit()}
        >
          4 · 一键计算、写入 NVS、复位回读
        </Button>

        <Typography.Text type="secondary">
          成功后仍需手工 OTA 回标准 fw-id=0 固件；GUI OTA/R2 catalog 暂未实现。
        </Typography.Text>
      </Space>
    </Card>
  );
}
