import { useEffect, useRef, useState } from "react";
import { api } from "./api";
import type { RollerCanControlState } from "./types";

export interface RollerCanControlSample {
  t: number;
  position: number | null;
  speed: number | null;
  current: number | null;
}

const BUFFER_MS = 60_000;

export function useRollerCanControlTelemetry(
  nodeId: number | null,
  connected: boolean,
  rateHz = 20,
) {
  const intervalMs = Math.max(20, Math.round(1000 / rateHz));
  const [latest, setLatest] = useState<RollerCanControlState | null>(null);
  const samples = useRef<RollerCanControlSample[]>([]);
  const lastRxCount = useRef<number | null>(null);
  const [chartVersion, setChartVersion] = useState(0);

  useEffect(() => {
    samples.current = [];
    lastRxCount.current = null;
    setLatest(null);
    setChartVersion((version) => version + 1);
  }, [nodeId]);

  useEffect(() => {
    if (nodeId == null || !connected) return;
    let alive = true;
    let pollTimer: number | undefined;

    const tick = async () => {
      const started = performance.now();
      try {
        const next = await api.rollerCanControlGetState(nodeId);
        if (!alive) return;
        setLatest(next);
        if (lastRxCount.current === next.rx_count) return;
        lastRxCount.current = next.rx_count;
        const now = performance.now();
        const buffer = samples.current;
        buffer.push({
          t: now,
          position: next.position_deg,
          speed: next.speed_rpm,
          current: next.current_ma,
        });
        const cutoff = now - BUFFER_MS;
        let removeCount = 0;
        while (removeCount < buffer.length && buffer[removeCount].t < cutoff) {
          removeCount += 1;
        }
        if (removeCount > 0) buffer.splice(0, removeCount);
      } catch {
        // Attach/detach and disconnect races are transient; the next tick
        // either succeeds or the component is unmounted.
      } finally {
        if (alive) {
          const elapsed = performance.now() - started;
          pollTimer = window.setTimeout(tick, Math.max(0, intervalMs - elapsed));
        }
      }
    };

    void tick();
    const chartTimer = window.setInterval(
      () => alive && setChartVersion((version) => version + 1),
      intervalMs,
    );
    return () => {
      alive = false;
      if (pollTimer != null) window.clearTimeout(pollTimer);
      window.clearInterval(chartTimer);
    };
  }, [connected, intervalMs, nodeId]);

  return { latest, samples, chartVersion };
}
