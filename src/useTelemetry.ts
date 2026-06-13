// Polls `get_status` for the selected motor at ~50 Hz, exposing the latest
// snapshot (for the numeric panel) and a rolling buffer of samples (for the
// chart). The chart buffer is kept in a ref and surfaced via a version
// counter so chart re-renders stay decoupled from the poll loop.

import { useEffect, useRef, useState } from "react";
import { api } from "./api";
import type { LiveState } from "./types";

export interface Sample {
  t: number; // ms since page load (performance.now)
  position: number | null;
  velocity: number | null;
  torque: number | null;
}

const BUFFER_MS = 60_000; // keep up to 60 s of history

// `rateHz` drives both the poll loop and the chart redraw. It's user-selectable
// in the UI (currently 50 / 100 Hz). The motor reports at ~1 kHz over CAN; the
// cap here is purely to keep the JS side responsive — `get_status` only ever
// hands back the latest snapshot, so the in-between frames are dropped, not
// averaged. Use CSV logging for the full-rate stream.
export function useTelemetry(
  nid: number | null,
  connected: boolean,
  rateHz = 50,
) {
  const intervalMs = Math.max(1, Math.round(1000 / rateHz));
  const [latest, setLatest] = useState<LiveState | null>(null);
  const bufRef = useRef<Sample[]>([]);
  const [chartVersion, setChartVersion] = useState(0);

  // Reset buffer whenever the selected motor changes.
  useEffect(() => {
    bufRef.current = [];
    setLatest(null);
    setChartVersion((v) => v + 1);
  }, [nid]);

  useEffect(() => {
    if (nid == null || !connected) return;
    let alive = true;

    const poll = window.setInterval(async () => {
      try {
        const ls = await api.getStatus(nid);
        if (!alive) return;
        setLatest(ls);
        const m = ls.measurements;
        const now = performance.now();
        const buf = bufRef.current;
        buf.push({
          t: now,
          position: m.position_rev,
          velocity: m.velocity_rev_per_s,
          torque: m.torque_nm,
        });
        // Trim old samples.
        const cutoff = now - BUFFER_MS;
        while (buf.length > 0 && buf[0].t < cutoff) buf.shift();
      } catch {
        // transient (e.g. just disconnected) — ignore
      }
    }, intervalMs);

    const chartTick = window.setInterval(() => {
      if (alive) setChartVersion((v) => v + 1);
    }, intervalMs);

    return () => {
      alive = false;
      window.clearInterval(poll);
      window.clearInterval(chartTick);
    };
  }, [nid, connected, intervalMs]);

  return { latest, samples: bufRef, chartVersion };
}
