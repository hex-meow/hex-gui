import { useMemo, type MutableRefObject } from "react";
import ReactECharts from "echarts-for-react";
import { useI18n, type I18nKey } from "../i18n";
import type { Sample } from "../useTelemetry";

// Three stacked sub-plots (position / velocity / torque) sharing a rolling
// time axis. One ECharts instance, auto-scaled per signal so different units
// stay readable.
const SIGNALS: { key: keyof Sample; nameKey: I18nKey; color: string }[] = [
  { key: "position", nameKey: "chartPos", color: "#4f8cff" },
  { key: "velocity", nameKey: "chartVel", color: "#2ecc71" },
  { key: "torque", nameKey: "chartTor", color: "#f39c12" },
];

// Minimum y span. When a signal sits nearly flat (a stationary motor's velocity
// noise is only ~0.01 wide), `scale: true` would zoom in until that noise fills
// the plot. If the data spans less than this, we pin a fixed window instead so
// tiny ripple looks tiny. The window is aligned to TICK and is HALF tall on
// each side (a multiple of TICK) so every gridline lands on a clean value —
// otherwise ECharts labels the raw float min/max (e.g. "-0.25258483").
const Y_MIN_SPAN = 0.1;
const Y_TICK = 0.02;
const Y_HALF = 0.06; // 3 × TICK → forced window is 0.12 wide (≥ Y_MIN_SPAN)
const round4 = (x: number) => Math.round(x * 1e4) / 1e4;

export function LiveChart({
  samples,
  chartVersion,
  windowSec,
}: {
  samples: MutableRefObject<Sample[]>;
  chartVersion: number;
  windowSec: number;
}) {
  const { t } = useI18n();
  const option = useMemo(() => {
    const buf = samples.current;
    const lastT = buf.length > 0 ? buf[buf.length - 1].t : performance.now();
    const cutoff = lastT - windowSec * 1000;

    // Track each signal's data range so we can floor the y span below.
    const ranges: ({ min: number; max: number } | null)[] = [];
    const series = SIGNALS.map((sig, i) => {
      const data: [number, number][] = [];
      let lo = Infinity;
      let hi = -Infinity;
      for (const s of buf) {
        if (s.t < cutoff) continue;
        const v = s[sig.key] as number | null;
        if (v == null || Number.isNaN(v)) continue;
        data.push([(s.t - lastT) / 1000, v]); // seconds ago (<= 0)
        if (v < lo) lo = v;
        if (v > hi) hi = v;
      }
      ranges[i] = data.length > 0 ? { min: lo, max: hi } : null;
      return {
        name: t(sig.nameKey),
        type: "line" as const,
        xAxisIndex: i,
        yAxisIndex: i,
        showSymbol: false,
        smooth: false,
        lineStyle: { width: 1.5, color: sig.color },
        itemStyle: { color: sig.color },
        data,
      };
    });

    const grids = SIGNALS.map((_, i) => ({
      left: 60,
      right: 20,
      top: `${6 + i * 32}%`,
      height: "24%",
    }));
    const axisLine = { lineStyle: { color: "#3a414d" } };
    const splitLine = { lineStyle: { color: "#222831" } };
    const xAxes = SIGNALS.map((_, i) => ({
      type: "value" as const,
      gridIndex: i,
      min: -windowSec,
      max: 0,
      axisLine,
      splitLine,
      axisLabel: {
        show: i === SIGNALS.length - 1,
        formatter: "{value}s",
        color: "#8a93a3",
      },
    }));
    const yAxes = SIGNALS.map((_, i) => {
      const r = ranges[i];
      // Force a clean, tick-aligned window only when the data is too flat to
      // auto-scale sensibly; otherwise let `scale: true` pick nice ticks.
      //
      // These keys must ALWAYS be present: the chart renders in merge mode
      // (notMerge=false), so omitting them would leave a previously-pinned
      // window stuck in place even after the signal grows past Y_MIN_SPAN.
      // Setting them to null each render hands scaling back to ECharts.
      let min: number | null = null;
      let max: number | null = null;
      let interval: number | null = null;
      if (r && r.max - r.min < Y_MIN_SPAN) {
        const center = Math.round((r.min + r.max) / 2 / Y_TICK) * Y_TICK;
        min = round4(center - Y_HALF);
        max = round4(center + Y_HALF);
        interval = Y_TICK;
      }
      return {
        type: "value" as const,
        gridIndex: i,
        scale: true,
        min,
        max,
        interval,
        axisLine,
        splitLine,
        axisLabel: { color: "#8a93a3" },
      };
    });

    // Signal labels as right-aligned titles per sub-plot, inset from the right
    // edge so long names ("Velocity") never clip. Colored to match the line.
    const titles = SIGNALS.map((sig, i) => ({
      text: t(sig.nameKey),
      right: 24,
      top: `${4 + i * 32}%`,
      textAlign: "right" as const,
      textStyle: {
        color: sig.color,
        fontSize: 12,
        fontWeight: "normal" as const,
      },
    }));

    return {
      animation: false,
      textStyle: { color: "#c9d1d9" },
      title: titles,
      grid: grids,
      xAxis: xAxes,
      yAxis: yAxes,
      tooltip: { trigger: "axis" },
      series,
    };
  }, [chartVersion, windowSec, samples, t]);

  return (
    <ReactECharts
      option={option}
      notMerge={false}
      lazyUpdate
      style={{ height: 460, width: "100%" }}
    />
  );
}
