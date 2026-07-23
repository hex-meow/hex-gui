import { useEffect, useRef, useState } from "react";
import { api } from "./api";
import type { DamiaoState } from "./types";

export interface DamiaoSample {
  t: number;
  position: number | null;
  velocity: number | null;
  torque: number | null;
}

const BUFFER_MS = 60_000;
const CHART_REDRAW_MS = 50;

/**
 * Poll the latest DAMIAO snapshot without allowing overlapping Tauri invokes.
 * Only new CAN feedback (`rx_count` changed) is appended to the chart, so a
 * high UI refresh setting never fabricates duplicate motor samples.
 */
export function useDamiaoTelemetry(
  motorId: number | null,
  connected: boolean,
  rateHz: number,
  chartActive: boolean,
) {
  const intervalMs = Math.max(1, Math.round(1000 / rateHz));
  const [latest, setLatest] = useState<DamiaoState | null>(null);
  const samples = useRef<DamiaoSample[]>([]);
  const lastRxCount = useRef<number | null>(null);
  const wasOnline = useRef(false);
  const [chartVersion, setChartVersion] = useState(0);

  useEffect(() => {
    samples.current = [];
    lastRxCount.current = null;
    wasOnline.current = false;
    setLatest(null);
    setChartVersion((value) => value + 1);
  }, [motorId]);

  useEffect(() => {
    if (!connected) setLatest(null);
    if (motorId == null || !connected) return;

    let alive = true;
    let timer: number | undefined;

    const poll = async () => {
      const started = performance.now();
      try {
        const next = await api.damiaoGetState(motorId);
        if (!alive) return;
        setLatest(next);

        const now = performance.now();
        const buffer = samples.current;
        if (next.rx_count !== lastRxCount.current) {
          lastRxCount.current = next.rx_count;
          buffer.push({
            t: now,
            position: next.position_rad,
            velocity: next.velocity_rad_s,
            torque: next.torque_nm,
          });
        } else if (wasOnline.current && !next.online) {
          // A null point makes ECharts show a real gap instead of drawing a
          // misleading straight line across a feedback outage.
          buffer.push({ t: now, position: null, velocity: null, torque: null });
        }
        wasOnline.current = next.online;

        const cutoff = now - BUFFER_MS;
        let removeCount = 0;
        while (removeCount < buffer.length && buffer[removeCount].t < cutoff) {
          removeCount += 1;
        }
        if (removeCount > 0) buffer.splice(0, removeCount);
      } catch {
        // Disconnect, detach and mode changes can race one poll. The next
        // serialized iteration retries without growing an invoke backlog.
      } finally {
        if (alive) {
          const elapsed = performance.now() - started;
          timer = window.setTimeout(poll, Math.max(0, intervalMs - elapsed));
        }
      }
    };

    void poll();
    return () => {
      alive = false;
      if (timer != null) window.clearTimeout(timer);
    };
  }, [connected, intervalMs, motorId]);

  useEffect(() => {
    if (!chartActive) return;
    const timer = window.setInterval(
      () => setChartVersion((value) => value + 1),
      CHART_REDRAW_MS,
    );
    return () => window.clearInterval(timer);
  }, [chartActive]);

  return { latest, samples, chartVersion };
}
