import { useMemo, type MutableRefObject } from "react";
import ReactECharts from "echarts-for-react";
import type { DamiaoSample } from "../useDamiaoTelemetry";

type SignalKey = "position" | "velocity" | "torque";

const SIGNALS: { key: SignalKey; color: string }[] = [
  { key: "position", color: "#4f8cff" },
  { key: "velocity", color: "#2ecc71" },
  { key: "torque", color: "#f39c12" },
];

const MIN_SPAN: Record<SignalKey, number> = {
  position: 0.05,
  velocity: 0.1,
  torque: 0.05,
};

export function DamiaoLiveChart({
  samples,
  chartVersion,
  windowSec,
  zh,
}: {
  samples: MutableRefObject<DamiaoSample[]>;
  chartVersion: number;
  windowSec: number;
  zh: boolean;
}) {
  const option = useMemo(() => {
    const labels: Record<SignalKey, string> = zh
      ? {
          position: "位置 (rad)",
          velocity: "速度 (rad/s)",
          torque: "转矩 (Nm)",
        }
      : {
          position: "Position (rad)",
          velocity: "Velocity (rad/s)",
          torque: "Torque (Nm)",
        };
    const buffer = samples.current;
    const lastT = buffer.length > 0 ? buffer[buffer.length - 1].t : performance.now();
    const cutoff = lastT - windowSec * 1000;
    const ranges: ({ min: number; max: number } | null)[] = [];

    const series = SIGNALS.map((signal, index) => {
      const data: [number, number | null][] = [];
      let min = Infinity;
      let max = -Infinity;
      for (const sample of buffer) {
        if (sample.t < cutoff) continue;
        const value = sample[signal.key];
        data.push([(sample.t - lastT) / 1000, value]);
        if (value != null && Number.isFinite(value)) {
          min = Math.min(min, value);
          max = Math.max(max, value);
        }
      }
      ranges[index] = min <= max ? { min, max } : null;
      return {
        name: labels[signal.key],
        type: "line" as const,
        xAxisIndex: index,
        yAxisIndex: index,
        showSymbol: false,
        connectNulls: false,
        smooth: false,
        sampling: "lttb" as const,
        lineStyle: { width: 1.5, color: signal.color },
        itemStyle: { color: signal.color },
        data,
      };
    });

    const axisLine = { lineStyle: { color: "#3a414d" } };
    const splitLine = { lineStyle: { color: "#222831" } };
    const grids = SIGNALS.map((_, index) => ({
      left: 64,
      right: 22,
      top: `${6 + index * 32}%`,
      height: "24%",
    }));
    const xAxis = SIGNALS.map((_, index) => ({
      type: "value" as const,
      gridIndex: index,
      min: -windowSec,
      max: 0,
      axisLine,
      splitLine,
      axisLabel: {
        show: index === SIGNALS.length - 1,
        formatter: "{value}s",
        color: "#8a93a3",
      },
    }));
    const yAxis = SIGNALS.map((signal, index) => {
      const range = ranges[index];
      const span = MIN_SPAN[signal.key];
      let min: number | null = null;
      let max: number | null = null;
      if (range && range.max - range.min < span) {
        const center = (range.min + range.max) / 2;
        min = center - span / 2;
        max = center + span / 2;
      }
      return {
        type: "value" as const,
        gridIndex: index,
        scale: true,
        min,
        max,
        axisLine,
        splitLine,
        axisLabel: { color: "#8a93a3" },
      };
    });
    const title = SIGNALS.map((signal, index) => ({
      text: labels[signal.key],
      right: 24,
      top: `${4 + index * 32}%`,
      textAlign: "right" as const,
      textStyle: {
        color: signal.color,
        fontSize: 12,
        fontWeight: "normal" as const,
      },
    }));

    return {
      animation: false,
      textStyle: { color: "#c9d1d9" },
      title,
      grid: grids,
      xAxis,
      yAxis,
      tooltip: {
        trigger: "axis",
        axisPointer: { type: "cross" },
        valueFormatter: (value: unknown) =>
          typeof value === "number" ? value.toFixed(4) : "—",
      },
      series,
    };
  }, [chartVersion, samples, windowSec, zh]);

  return (
    <ReactECharts
      option={option}
      notMerge={false}
      lazyUpdate
      style={{ height: 460, width: "100%" }}
    />
  );
}
