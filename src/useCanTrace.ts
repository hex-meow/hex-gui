// Polling hook for the CAN analyzer (mirrors useImuTelemetry's decoupling).
//
// A frontend refresh timer drains bounded batches from the backend into refs;
// a matching render timer bumps a version counter to trigger a re-render.
// Nothing renders per frame. Trace mode uses a seq cursor (so each poll only
// pulls *new* frames); grouped mode pulls the whole small per-ID table. The
// trace view owns its frozen snapshot; polling and status updates stay live.

import { useEffect, useRef, useState } from "react";
import { api } from "./api";
import type { CanAggRow, CanAnalyzerStatus, CanBusHealth, CanFilterSpec, CanTraceFrame } from "./types";

const HEALTH_MS = 1000; // 1 Hz controller-health poll (netlink / USB control)
const MAX_ROWS = 3000; // client trace buffer cap
const MAX_BATCH = 2000; // frames requested per poll
const RATE_WINDOW_MS = 1500; // rolling window for the fps estimate (smooths sparse traffic)
/** Bus is "active" if a frame arrived within this window. Must exceed the
 *  slowest expected heartbeat (CANopen nodes heartbeat every ~0.5–1 s) so a
 *  single node doesn't flicker idle/active between beats. */
export const ACTIVE_WINDOW_MS = 2500;

export type CanMode = "trace" | "grouped";

export interface CanTraceState {
  /** Rolling trace buffer (newest last). Lives in a ref; read on each version. */
  bufRef: React.MutableRefObject<CanTraceFrame[]>;
  /** Latest grouped rows (trace mode leaves this empty). */
  groupedRef: React.MutableRefObject<CanAggRow[]>;
  statusRef: React.MutableRefObject<CanAnalyzerStatus | null>;
  /** Derived receive rate (frames/s) from status.rx_total deltas. */
  rateRef: React.MutableRefObject<number>;
  /** Sticky: some frames rolled off the backend ring before we read them. */
  gapRef: React.MutableRefObject<boolean>;
  /** Cumulative rows evicted from the FRONT of the local buffer (for the trace
   *  view to compensate scroll position and keep older frames pinned). */
  evictedRef: React.MutableRefObject<number>;
  /** performance.now() of the last time the frame count increased. The strip
   *  derives active/idle from this (vs ACTIVE_WINDOW_MS), not instantaneous rate. */
  lastActivityRef: React.MutableRefObject<number>;
  /** Controller health (error counters / bus-off), polled at 1 Hz. */
  healthRef: React.MutableRefObject<CanBusHealth | null>;
  version: number;
  /** Clear backend + local buffers and reset the cursor. */
  clear: () => Promise<void>;
}

export function useCanTrace(
  running: boolean,
  mode: CanMode,
  filter: CanFilterSpec,
  refreshHz: number,
): CanTraceState {
  const bufRef = useRef<CanTraceFrame[]>([]);
  const groupedRef = useRef<CanAggRow[]>([]);
  const statusRef = useRef<CanAnalyzerStatus | null>(null);
  const rateRef = useRef(0);
  const gapRef = useRef(false);
  const evictedRef = useRef(0);
  const lastActivityRef = useRef(0);
  const healthRef = useRef<CanBusHealth | null>(null);
  const rateWindowRef = useRef<{ t: number; total: number }[]>([]);
  const cursorRef = useRef(0);
  // Invalidate replies when a clear or a new view/session supersedes them.
  const generationRef = useRef(0);
  const sessionRef = useRef(0);
  const clearingRef = useRef(false);
  const pollInFlightRef = useRef(false);
  const prevRxTotalRef = useRef(0);
  const [version, setVersion] = useState(0);
  const refreshMs = Math.max(1, Math.round(1000 / Math.max(1, refreshHz)));

  const filterKey = JSON.stringify(filter);

  const resetLocal = (cursor = 0) => {
    bufRef.current = [];
    groupedRef.current = [];
    cursorRef.current = cursor;
    gapRef.current = false;
    evictedRef.current = 0;
  };

  const resetStatistics = () => {
    lastActivityRef.current = 0;
    rateWindowRef.current = [];
    prevRxTotalRef.current = 0;
    rateRef.current = 0;
  };

  const clear = async () => {
    if (clearingRef.current) return;
    generationRef.current++;
    const session = sessionRef.current;
    clearingRef.current = true;
    try {
      const next = await api.analyzerClear();
      if (session !== sessionRef.current) return;
      // The backend keeps sequence numbers monotonic across clear. Adopt its
      // cursor, otherwise the next poll reports a false gap or replays frames.
      resetLocal(next);
      resetStatistics();
      const status = statusRef.current;
      if (status) {
        // Clearing traffic never changes the connection's CAN-FD capability.
        statusRef.current = {
          ...status, total: 0, rx_total: 0, our_dropped: 0, distinct_ids: 0,
          agg_overflow: 0, ring_len: 0, next_seq: next + 1,
        };
      }
      setVersion((v) => v + 1);
    } catch {
      /* ignore */
    } finally {
      if (session === sessionRef.current) clearingRef.current = false;
    }
  };

  // A pending clear still applies after changing the filter/mode. Only a new
  // connection (or unmount) can supersede it and resume polling independently.
  useEffect(() => {
    clearingRef.current = false;
    statusRef.current = null;
    healthRef.current = null;
    resetStatistics();
    return () => {
      sessionRef.current++;
    };
  }, [running]);

  // Reset local buffers only when the session, mode, or filter changes. Changing
  // the refresh rate should not wipe the user's current trace view.
  useEffect(() => {
    generationRef.current++;
    // Filter/mode changed → re-pull from the ring start with the new predicate.
    resetLocal();
    setVersion((v) => v + 1);
    return () => {
      generationRef.current++;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running, mode, filterKey]);

  // Re-arm frontend polling/rendering whenever the refresh rate changes.
  useEffect(() => {
    if (!running) return;
    let alive = true;
    let firstPoll = true;

    const updateRate = (status: CanAnalyzerStatus) => {
      const now = performance.now();
      // fps over a rolling window (stable for sparse heartbeat traffic).
      const w = rateWindowRef.current;
      w.push({ t: now, total: status.rx_total });
      while (w.length > 1 && now - w[0].t > RATE_WINDOW_MS) w.shift();
      if (w.length >= 2) {
        const dt = (now - w[0].t) / 1000;
        if (dt > 0) rateRef.current = Math.max(0, (status.rx_total - w[0].total) / dt);
      }
      // Only received frames establish receive activity. A TX mirror entry
      // means accepted by the driver, not that another device responded.
      if (status.rx_total > prevRxTotalRef.current) lastActivityRef.current = now;
      prevRxTotalRef.current = status.rx_total;
      if (!status.capturing) {
        rateRef.current = 0;
        lastActivityRef.current = 0;
      }
      statusRef.current = status;
    };

    const poll = window.setInterval(async () => {
      // IPC may take longer than a refresh interval, especially during SDO.
      // Concurrent requests would read the same cursor and append duplicate
      // seq keys, corrupting reconciliation of the virtual trace rows.
      if (pollInFlightRef.current || clearingRef.current) return;
      pollInFlightRef.current = true;
      const generation = generationRef.current;
      try {
        if (mode === "trace") {
          const reply = await api.analyzerGetTrace(cursorRef.current, MAX_BATCH, filter);
          if (!alive || generation !== generationRef.current) return;
          cursorRef.current = reply.next_seq;
          if (reply.gap && !firstPoll) gapRef.current = true;
          if (reply.frames.length > 0) {
            const buf = bufRef.current;
            buf.push(...reply.frames);
            if (buf.length > MAX_ROWS) {
              const removed = buf.length - MAX_ROWS;
              buf.splice(0, removed);
              evictedRef.current += removed;
            }
          }
          updateRate(reply.status);
        } else {
          const reply = await api.analyzerGetAggregates(filter);
          if (!alive || generation !== generationRef.current) return;
          groupedRef.current = reply.rows;
          updateRate(reply.status);
        }
        firstPoll = false;
      } catch {
        /* transient (e.g. just stopped) — ignore */
      } finally {
        pollInFlightRef.current = false;
      }
    }, refreshMs);

    const tick = window.setInterval(() => {
      if (alive) setVersion((v) => v + 1);
    }, refreshMs);

    return () => {
      alive = false;
      window.clearInterval(poll);
      window.clearInterval(tick);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running, mode, filterKey, refreshMs]);

  useEffect(() => {
    if (!running) return;
    let alive = true;

    let healthFails = 0;
    let inFlight = false;
    const health = window.setInterval(async () => {
      if (inFlight) return;
      inFlight = true;
      try {
        const h = await api.analyzerBusState();
        if (alive) {
          healthRef.current = h;
          healthFails = 0;
        }
      } catch {
        // Don't let a stale chip pretend to be fresh: drop it after a few
        // consecutive failures (transient errors are still tolerated).
        if (alive && ++healthFails >= 3) healthRef.current = null;
      } finally {
        inFlight = false;
      }
    }, HEALTH_MS);

    return () => {
      alive = false;
      window.clearInterval(health);
    };
  }, [running]);

  return { bufRef, groupedRef, statusRef, rateRef, gapRef, evictedRef, lastActivityRef, healthRef, version, clear };
}
