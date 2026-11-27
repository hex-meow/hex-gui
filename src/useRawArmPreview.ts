import { useEffect, useRef, useState } from "react";
import { errMsg } from "./api";
import { rawArm, type ArmPreview, type ArmTarget, type MotionLimits } from "./rawArm";

interface PreviewRequest {
  model: object | null;
  target: ArmTarget;
  limits: MotionLimits;
  available: boolean;
}

interface Result {
  request: PreviewRequest;
  preview?: ArmPreview;
  pending?: boolean;
  error?: string;
}

function startMoved(measured?: number[], start?: number[]) {
  return !!start && !!measured && measured.some((value, i) => Math.abs(value - start[i]) >= 0.01);
}

export function useRawArmPreview(request: PreviewRequest, measured: number[] | undefined, zh: boolean) {
  const [result, setResult] = useState<Result | null>(null);
  const lastPlanAt = useRef(-Infinity);
  const latestMeasured = useRef(measured);
  const checkStart = useRef<(() => void) | undefined>();
  const { target } = request;
  const values = target.kind === "joints" ? target.q : [...target.xyz, ...(target.rpy ?? [])];
  const complete = values.length > 0 && values.every(Number.isFinite);

  useEffect(() => {
    latestMeasured.current = measured;
    checkStart.current?.();
  }, [measured]);

  // A new draft owns its own IK solution. Feedback refreshes only replan the
  // path to that solution, even if an intermediate refresh fails.
  useEffect(() => {
    if (!request.model || !request.available || !complete) return;
    let live = true;
    let timer: number | undefined;
    let planning = false;
    let attempted = false;
    let start: number[] | undefined;
    let endpoint: number[] | undefined;
    let displayed: ArmPreview | undefined;

    function schedule() {
      if (!live || planning || timer !== undefined) return;
      if (attempted && !startMoved(latestMeasured.current, start)) return;
      const interval = attempted ? 200 : 70;
      const delay = Math.max(0, lastPlanAt.current + interval - performance.now());
      timer = window.setTimeout(plan, delay);
    }

    async function plan() {
      timer = undefined;
      if (!live || (attempted && !startMoved(latestMeasured.current, start))) return;
      attempted = true;
      planning = true;
      lastPlanAt.current = performance.now();
      start = latestMeasured.current;
      if (displayed) setResult({ request, preview: displayed, pending: true });
      try {
        const next = await rawArm.preview(endpoint ? { kind: "joints", q: endpoint } : request.target, request.limits);
        if (!live) return;
        start = next.joints[0];
        endpoint ??= next.goal;
        displayed = next;
        setResult({ request, preview: next });
      } catch (e) {
        if (!live) return;
        displayed = undefined;
        setResult({ request, error: errMsg(e) });
      } finally {
        planning = false;
        schedule();
      }
    }

    checkStart.current = schedule;
    schedule();
    return () => {
      live = false;
      checkStart.current = undefined;
      window.clearTimeout(timer);
    };
  }, [request, complete]);

  const current = result?.request === request ? result : null;
  const preview = current?.preview ?? null;
  const executablePreview = !current?.pending && !startMoved(measured, preview?.joints[0]) ? preview : null;
  const incomplete = request.model && request.available && !complete;
  const planError = incomplete ? (zh ? "请补全目标坐标。" : "Complete the target coordinates.") : current?.error;
  return { preview, executablePreview, planError };
}
