import { useCallback, useEffect, useRef, useState } from "react";
import { api, errMsg } from "./api";
import type { DamiaoState } from "./types";

export interface DamiaoSample {
  t: number;
  position: number | null;
  outputPosition: number | null;
  velocity: number | null;
  torque: number | null;
}

const BUFFER_MS = 60_000;
const CHART_REDRAW_MS = 50;

/**
 * Poll the latest DAMIAO snapshot without allowing overlapping Tauri invokes.
 * Only new CAN feedback or position-register readings are appended, so a
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
  const lastPositionReadCount = useRef<number | null>(null);
  const wasOnline = useRef(false);
  const referenceVersion = useRef(0);
  const [chartVersion, setChartVersion] = useState(0);

  const acceptSnapshot = useCallback((snapshot: DamiaoState) => {
    // Do not let a poll started before a command overwrite its result.
    referenceVersion.current += 1;
    setLatest(snapshot);
  }, []);

  const resetHistory = useCallback((snapshot?: DamiaoState) => {
    // A zero change starts a new physical position reference. Invalidate an
    // in-flight poll too, so its old coordinates cannot re-enter the buffer.
    referenceVersion.current += 1;
    samples.current = snapshot ? [{
      t: performance.now(),
      position: snapshot.position_rad,
      outputPosition: snapshot.output_position_rad,
      velocity: snapshot.velocity_rad_s,
      torque: snapshot.torque_nm,
    }] : [];
    lastRxCount.current = snapshot?.rx_count ?? null;
    lastPositionReadCount.current = snapshot?.position_read_count ?? null;
    wasOnline.current = snapshot?.online ?? false;
    if (snapshot) setLatest(snapshot);
    setChartVersion((value) => value + 1);
  }, []);

  useEffect(() => {
    samples.current = [];
    lastRxCount.current = null;
    lastPositionReadCount.current = null;
    wasOnline.current = false;
    setLatest(null);
    setChartVersion((value) => value + 1);
  }, [motorId, connected]);

  useEffect(() => {
    if (!connected) setLatest(null);
    if (motorId == null || !connected) return;

    let alive = true;
    let timer: number | undefined;

    const poll = async () => {
      const started = performance.now();
      const referenceAtStart = referenceVersion.current;
      try {
        const next = await api.damiaoGetState(motorId);
        if (!alive || referenceAtStart !== referenceVersion.current) return;
        setLatest(next);

        const now = performance.now();
        const buffer = samples.current;
        if (next.rx_count !== lastRxCount.current
          || next.position_read_count !== lastPositionReadCount.current) {
          lastRxCount.current = next.rx_count;
          lastPositionReadCount.current = next.position_read_count;
          buffer.push({
            t: now,
            position: next.position_rad,
            outputPosition: next.output_position_rad,
            velocity: next.velocity_rad_s,
            torque: next.torque_nm,
          });
        } else if (wasOnline.current && !next.online) {
          // A null point makes ECharts show a real gap instead of drawing a
          // misleading straight line across a feedback outage.
          buffer.push({ t: now, position: null, outputPosition: null, velocity: null, torque: null });
        }
        wasOnline.current = next.online;

        const cutoff = now - BUFFER_MS;
        let removeCount = 0;
        while (removeCount < buffer.length && buffer[removeCount].t < cutoff) {
          removeCount += 1;
        }
        if (removeCount > 0) buffer.splice(0, removeCount);
      } catch (error) {
        if (!alive || referenceAtStart !== referenceVersion.current) return;
        // A failed snapshot must not leave stale "online" state enabling
        // motion controls. A successful retry restores the actual feedback.
        setLatest((previous) => previous && {
          ...previous,
          online: false,
          position_rad: null,
          precise_position_rad: null,
          output_position_rad: null,
          last_error: errMsg(error),
        });
        if (wasOnline.current) {
          samples.current.push({ t: performance.now(), position: null, outputPosition: null, velocity: null, torque: null });
          wasOnline.current = false;
        }
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

  return { latest, samples, chartVersion, resetHistory, acceptSnapshot };
}
