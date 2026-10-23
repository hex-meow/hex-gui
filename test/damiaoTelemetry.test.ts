import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test, { type TestContext } from "node:test";
import ts from "typescript";
import type { DamiaoRegisterDefinition, DamiaoState } from "../src/types.ts";

const require = createRequire(import.meta.url);
const React = require("react");
const { act, create } = require("react-test-renderer");

function loadSource(relative: string, mocks: Record<string, unknown>, extra = "") {
  const url = new URL(relative, import.meta.url);
  const { outputText } = ts.transpileModule(readFileSync(url, "utf8") + extra, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2021 },
    fileName: url.pathname,
  });
  const module = { exports: {} as any };
  new Function("require", "module", "exports", outputText)(
    (name: string) => Object.hasOwn(mocks, name) ? mocks[name] : require(name),
    module, module.exports,
  );
  return module.exports;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function snapshot(overrides: Partial<DamiaoState> = {}): DamiaoState {
  return {
    attached: true, requires_reconnect: false, motor_id: 1, master_id: 0,
    feedback_can_id: 0, mode: "Mit", p_max: 12.5, v_max: 30, t_max: 10,
    online: true, enabled: true, streaming: false, status_code: 1, status: "Enabled",
    position_rad: 7, feedback_position_rad: 7, precise_position_rad: 7,
    output_position_rad: 6.9, position_read_age_ms: 0, position_read_count: 1,
    position_read_error: null, velocity_rad_s: 0, torque_nm: 0,
    mos_temp_c: 25, rotor_temp_c: 25, feedback_age_ms: 0,
    feedback_rate_hz: 50, rx_count: 1, last_error: null, ...overrides,
  };
}

type HookProps = { motorId: number | null; connected: boolean; rateHz: number; chartActive: boolean };

function mountHook(t: TestContext) {
  const timeouts = new Map<number, () => unknown>();
  const intervals = new Map<number, () => unknown>();
  let timerId = 0;
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", { configurable: true, value: {
    setTimeout(callback: () => unknown) { timeouts.set(++timerId, callback); return timerId; },
    clearTimeout(id: number) { timeouts.delete(id); },
    setInterval(callback: () => unknown) { intervals.set(++timerId, callback); return timerId; },
    clearInterval(id: number) { intervals.delete(id); },
  } });
  const requests: Array<ReturnType<typeof deferred<DamiaoState>> & { motorId: number }> = [];
  const { useDamiaoTelemetry } = loadSource("../src/useDamiaoTelemetry.ts", {
    "./api": {
      api: { damiaoGetState(motorId: number) {
        const request = { ...deferred<DamiaoState>(), motorId };
        requests.push(request);
        return request.promise;
      } },
      errMsg: (error: Error) => error.message,
    },
  });
  let state: ReturnType<typeof useDamiaoTelemetry>;
  let props: HookProps = { motorId: 1, connected: true, rateHz: 50, chartActive: false };
  function Harness(p: HookProps) {
    state = useDamiaoTelemetry(p.motorId, p.connected, p.rateHz, p.chartActive);
    return null;
  }
  let renderer: any;
  act(() => { renderer = create(React.createElement(Harness, props)); });
  t.after(() => {
    act(() => renderer.unmount());
    if (previousWindow) Object.defineProperty(globalThis, "window", previousWindow);
    else Reflect.deleteProperty(globalThis, "window");
  });
  return {
    requests,
    get state() { return state; },
    async tick() {
      await act(async () => {
        for (const [id, callback] of [...timeouts]) { timeouts.delete(id); callback(); }
        for (const callback of [...intervals.values()]) callback();
      });
    },
    async resolve(index: number, value: DamiaoState) {
      await act(async () => { requests[index].resolve(value); });
    },
    async reject(index: number, message: string) {
      await act(async () => { requests[index].reject(new Error(message)); });
    },
    update(next: Partial<HookProps>) {
      props = { ...props, ...next };
      act(() => { renderer.update(React.createElement(Harness, props)); });
    },
  };
}

test("DAMIAO polling serializes slow IPC and charts only new feedback or position readings", async (t) => {
  const h = mountHook(t);
  for (let i = 0; i < 5; i += 1) await h.tick();
  assert.equal(h.requests.length, 1);
  await h.resolve(0, snapshot());
  await h.tick();
  await h.resolve(1, snapshot());
  assert.equal(h.state.samples.current.length, 1);
  await h.tick();
  await h.resolve(2, snapshot({ position_read_count: 2, output_position_rad: 7.1 }));
  assert.deepEqual(h.state.samples.current.map((sample: any) => sample.outputPosition), [6.9, 7.1]);
  await h.tick();
  await h.resolve(3, snapshot({ rx_count: 2, position_read_count: 2, velocity_rad_s: 1 }));
  assert.equal(h.state.samples.current.length, 3);
});

test("command feedback remains authoritative when an older poll completes", async (t) => {
  const h = mountHook(t);
  const disabled = snapshot({ enabled: false, status_code: 0, status: "Disabled" });
  act(() => { h.state.acceptSnapshot(disabled); });
  await h.resolve(0, snapshot());
  assert.equal(h.state.latest.enabled, false);
  assert.equal(h.state.samples.current.length, 0);
  await h.tick();
  await h.resolve(1, disabled);
  assert.equal(h.state.samples.current.length, 1);
});

test("changing zero discards old coordinates and invalidates an in-flight position poll", async (t) => {
  const h = mountHook(t);
  await h.resolve(0, snapshot());
  await h.tick();
  const zeroed = snapshot({ position_rad: 0, precise_position_rad: 0, output_position_rad: 0, rx_count: 2, position_read_count: 2 });
  act(() => { h.state.resetHistory(zeroed); });
  await h.resolve(1, snapshot({ position_rad: 8, output_position_rad: 7.9 }));
  assert.equal(h.state.latest.position_rad, 0);
  assert.deepEqual(h.state.samples.current.map((sample: any) => [sample.position, sample.outputPosition]), [[0, 0]]);
  await h.tick();
  await h.resolve(2, zeroed);
  assert.equal(h.state.samples.current.length, 1);
});

test("failed feedback marks the motor offline and makes one chart gap until recovery", async (t) => {
  const h = mountHook(t);
  await h.resolve(0, snapshot());
  await h.tick();
  await h.reject(1, "Adapter disconnected");
  assert.equal(h.state.latest.online, false);
  assert.equal(h.state.latest.position_rad, null);
  assert.equal(h.state.latest.output_position_rad, null);
  assert.equal(h.state.latest.last_error, "Adapter disconnected");
  assert.equal(h.state.samples.current.length, 2);
  assert.equal(h.state.samples.current[1].position, null);
  await h.tick();
  await h.reject(2, "Adapter disconnected");
  assert.equal(h.state.samples.current.length, 2);
  await h.tick();
  await h.resolve(3, snapshot({ rx_count: 2 }));
  assert.equal(h.state.latest.online, true);
  assert.equal(h.state.latest.last_error, null);
  assert.equal(h.state.samples.current.length, 3);
});

test("changing motors rejects pending feedback from the previous motor", async (t) => {
  const h = mountHook(t);
  h.update({ motorId: 2 });
  assert.equal(h.requests[1].motorId, 2);
  await h.resolve(0, snapshot());
  assert.equal(h.state.latest, null);
  await h.resolve(1, snapshot({ motor_id: 2, position_rad: 12 }));
  assert.equal(h.state.latest.motor_id, 2);
  assert.deepEqual(h.state.samples.current.map((sample: any) => sample.position), [12]);
});

test("disconnecting clears samples and rejects pending success or failure", async (t) => {
  const h = mountHook(t);
  await h.resolve(0, snapshot());
  await h.tick();
  h.update({ connected: false });
  await h.reject(1, "Adapter disconnected");
  assert.equal(h.state.latest, null);
  assert.equal(h.state.samples.current.length, 0);
  await h.tick();
  assert.equal(h.requests.length, 2);
  h.update({ connected: true });
  await h.resolve(2, snapshot({ rx_count: 3 }));
  assert.equal(h.state.latest.online, true);
  assert.equal(h.state.samples.current.length, 1);
});

const { validateDraft } = loadSource("../src/components/DamiaoRegisterPanel.tsx", {
  antd: {}, "../api": {},
}, "\nexport { validateDraft };\n");

const register: DamiaoRegisterDefinition = {
  address: 0x15, name: "PMAX", label_zh: "位置映射", label_en: "Position mapping",
  group: "mapping", data_type: "Float32", writable: true,
  min: 0, max: null, min_inclusive: false, max_inclusive: true,
  range: "(0, fmax]", unit: "rad", note_zh: "", note_en: "",
};

test("register editor rejects floating-point values that become zero or infinity on the wire", () => {
  for (const value of ["", "NaN", "Infinity", "1e100", "1e-50", "0", "-1"]) {
    assert.equal(typeof validateDraft(register, value, false), "string", value);
  }
  assert.equal(validateDraft(register, "1.25e1", false), 12.5);
});

test("register editor enforces exact integers and exclusive float32 range endpoints", () => {
  const idRegister = { ...register, data_type: "Uint32", min: 0, max: 0x7ff, min_inclusive: true };
  assert.equal(validateDraft(idRegister, "0x7FF", false), 0x7ff);
  for (const value of ["1.5", "-1", "2048", "4294967296"]) {
    assert.equal(typeof validateDraft(idRegister, value, false), "string", value);
  }
  const exclusive = { ...register, max: 1, max_inclusive: false };
  assert.equal(typeof validateDraft(exclusive, "0.999999999", false), "string");
  assert.equal(validateDraft(exclusive, "0.5", false), 0.5);
});
