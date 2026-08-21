import { useMemo, type MutableRefObject } from "react";
import ReactECharts from "echarts-for-react";
import type { RollerCanControlSample } from "../useRollerCanControlTelemetry";

const SIGNALS = [
  { key: "position" as const, en: "Position (deg)", zh: "位置 (°)", color: "#4f8cff" },
  { key: "speed" as const, en: "Speed (rpm)", zh: "速度 (rpm)", color: "#2ecc71" },
  { key: "current" as const, en: "Current (mA)", zh: "电流 (mA)", color: "#f39c12" },
];

export function RollerCanControlChart({
  samples,
  chartVersion,
  windowSec,
  zh,
}: {
  samples: MutableRefObject<RollerCanControlSample[]>;
  chartVersion: number;
  windowSec: number;
  zh: boolean;
}) {
  const option = useMemo(() => {
    const buffer = samples.current;
    const lastT = buffer.length > 0 ? buffer[buffer.length - 1].t : performance.now();
    const cutoff = lastT - windowSec * 1000;
    const grids = SIGNALS.map((_, index) => ({
      left: 64,
      right: 24,
      top: `${6 + index * 32}%`,
      height: "24%",
    }));
    const xAxis = SIGNALS.map((_, index) => ({
      type: "value" as const,
      gridIndex: index,
      min: -windowSec,
      max: 0,
      axisLine: { lineStyle: { color: "#3a414d" } },
      splitLine: { lineStyle: { color: "#222831" } },
      axisLabel: {
        show: index === SIGNALS.length - 1,
        formatter: "{value}s",
        color: "#8a93a3",
      },
    }));
    const yAxis = SIGNALS.map((_, index) => ({
      type: "value" as const,
      gridIndex: index,
      scale: true,
      axisLine: { lineStyle: { color: "#3a414d" } },
      splitLine: { lineStyle: { color: "#222831" } },
      axisLabel: { color: "#8a93a3" },
    }));
    const series = SIGNALS.map((signal, index) => ({
      name: zh ? signal.zh : signal.en,
      type: "line" as const,
      xAxisIndex: index,
      yAxisIndex: index,
      showSymbol: false,
      animation: false,
      lineStyle: { width: 1.5, color: signal.color },
      itemStyle: { color: signal.color },
      data: buffer
        .filter((sample) => sample.t >= cutoff && sample[signal.key] != null)
        .map((sample) => [(sample.t - lastT) / 1000, sample[signal.key]]),
    }));
    const title = SIGNALS.map((signal, index) => ({
      text: zh ? signal.zh : signal.en,
      right: 24,
      top: `${4 + index * 32}%`,
      textStyle: { color: signal.color, fontSize: 12, fontWeight: "normal" as const },
    }));
    return {
      animation: false,
      textStyle: { color: "#c9d1d9" },
      title,
      grid: grids,
      xAxis,
      yAxis,
      tooltip: { trigger: "axis" },
      series,
    };
  }, [chartVersion, samples, windowSec, zh]);

  return <ReactECharts option={option} lazyUpdate style={{ height: 460, width: "100%" }} />;
}
