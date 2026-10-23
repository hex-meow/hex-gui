import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test, { type TestContext } from "node:test";
import ts from "typescript";
import type { CanAggReply, CanBusHealth, CanFilterSpec, CanTraceReply } from "../src/types.ts";
import type { CanMode, CanTraceState } from "../src/useCanTrace.ts";

const require = createRequire(import.meta.url);
const React = require("react");
const { act, create } = require("react-test-renderer");

// Exercise the actual hook/component without loading Tauri or a browser. Only
// IPC, timers, and presentation components are replaced; React manages state.
function loadSource(relative: string, mocks: Record<string, unknown>, extra = "") {
  const url = new URL(relative, import.meta.url);
  const source = readFileSync(url, "utf8") + extra;
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2021 },
    fileName: url.pathname,
  });
  const module = { exports: {} as any };
  new Function("require", "module", "exports", outputText)(
    (name: string) => Object.hasOwn(mocks, name) ? mocks[name] : require(name),
    module,
    module.exports,
  );
  return module.exports;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function traceReply(...seqs: number[]): CanTraceReply {
  const next = seqs.at(-1) ?? 0;
  return {
    frames: seqs.map((seq) => ({ seq, t_us: seq * 1000, id: 0x701, extended: false, kind: "data", dlc: 1, data: "7f", dir: "rx" })),
    next_seq: next,
    gap: false,
    status: { capturing: true, connection_error: null, total: next, rx_total: next, our_dropped: 0, distinct_ids: 1, agg_overflow: 0, ring_len: seqs.length, next_seq: next + 1, fd: false, max_dlen: 8, hw_ts: false },
  };
}

type HookProps = { running: boolean; mode: CanMode; filter: CanFilterSpec; paused: boolean; refreshHz: number };

function mountHook(t: TestContext) {
  let timerId = 0;
  const timers = new Map<number, { callback: () => unknown; ms: number }>();
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", { configurable: true, value: {
    setInterval(callback: () => unknown, ms: number) { timers.set(++timerId, { callback, ms }); return timerId; },
    clearInterval(id: number) { timers.delete(id); },
  } });
  const traces: Array<ReturnType<typeof deferred<CanTraceReply>> & { cursor: number; filter: CanFilterSpec }> = [];
  const aggregates: Array<ReturnType<typeof deferred<CanAggReply>>> = [];
  const clears: Array<ReturnType<typeof deferred<number>>> = [];
  const health: Array<ReturnType<typeof deferred<CanBusHealth>>> = [];
  const api = {
    analyzerGetTrace(cursor: number, _max: number, filter: CanFilterSpec) {
      const request = { ...deferred<CanTraceReply>(), cursor, filter };
      traces.push(request);
      return request.promise;
    },
    analyzerGetAggregates() { const request = deferred<CanAggReply>(); aggregates.push(request); return request.promise; },
    analyzerClear() { const request = deferred<number>(); clears.push(request); return request.promise; },
    analyzerBusState() { const request = deferred<CanBusHealth>(); health.push(request); return request.promise; },
  };
  const { useCanTrace } = loadSource("../src/useCanTrace.ts", { "./api": { api } });
  let state!: CanTraceState;
  let props: HookProps = { running: true, mode: "trace", filter: { kind: "all" }, paused: false, refreshHz: 60 };
  function Harness(p: HookProps) {
    state = useCanTrace(p.running, p.mode, p.filter, p.refreshHz);
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
    traces, aggregates, clears, health,
    get state() { return state; },
    async tick() {
      await act(async () => {
        for (const timer of [...timers.values()]) if (timer.ms < 1000) timer.callback();
      });
    },
    async tickHealth() {
      await act(async () => {
        for (const timer of [...timers.values()]) if (timer.ms === 1000) timer.callback();
      });
    },
    update(next: Partial<HookProps>) {
      props = { ...props, ...next };
      act(() => renderer.update(React.createElement(Harness, props)));
    },
  };
}

async function resolve<T>(request: ReturnType<typeof deferred<T>>, value: T) {
  await act(async () => request.resolve(value));
}

test("slow analyzer IPC cannot overlap or duplicate trace sequence numbers", async (t) => {
  const h = mountHook(t);
  for (let i = 0; i < 8; i++) await h.tick();
  assert.equal(h.traces.length, 1);
  await resolve(h.traces[0], traceReply(1, 2));
  await h.tick();
  assert.equal(h.traces[1].cursor, 2);
  await resolve(h.traces[1], traceReply(3));
  assert.deepEqual(h.state.bufRef.current.map((frame) => frame.seq), [1, 2, 3]);
});

test("clear blocks polling, discards old replies, and adopts the backend cursor", async (t) => {
  const h = mountHook(t);
  await h.tick();
  await resolve(h.traces[0], traceReply(1));
  await h.tick();
  const clearing = h.state.clear();
  await h.tick();
  assert.equal(h.traces.length, 2);
  await resolve(h.clears[0], 50);
  await clearing;
  await resolve(h.traces[1], traceReply(2));
  assert.deepEqual(h.state.bufRef.current, []);
  assert.equal(h.state.statusRef.current?.total, 0);
  assert.equal(h.state.statusRef.current?.rx_total, 0);
  assert.equal(h.state.statusRef.current?.capturing, true);
  await h.tick();
  assert.equal(h.traces[2].cursor, 50);
  await resolve(h.traces[2], traceReply(51));
  assert.deepEqual(h.state.bufRef.current.map((frame) => frame.seq), [51]);
  assert.equal(h.state.gapRef.current, false);
});

for (const change of ["filter", "mode"] as const) {
  test(`changing ${change} during clear waits for its cursor and rejects stale frames`, async (t) => {
    const h = mountHook(t);
    await h.tick();
    const clearing = h.state.clear();
    if (change === "filter") h.update({ filter: { kind: "node", node: 1, include_nodeless: true } });
    if (change === "mode") h.update({ mode: "grouped" });
    await resolve(h.traces[0], traceReply(40));
    assert.deepEqual(h.state.bufRef.current, []);
    await h.tick();
    assert.equal(h.traces.length, 1);
    assert.equal(h.aggregates.length, 0);
    await resolve(h.clears[0], 99);
    await clearing;
    await h.tick();
    if (change === "mode") {
      const row = { id: 0x701, extended: false, count: 1, rate_hz: 1, last_dlc: 1, last_kind: "data" as const, last_data: "7f", first_us: 1000, last_us: 1000 };
      await resolve(h.aggregates[0], { rows: [row], status: traceReply(1).status });
      assert.deepEqual(h.state.groupedRef.current, [row]);
    } else {
      assert.equal(h.traces[1].cursor, 99);
      assert.deepEqual(h.traces[1].filter, { kind: "node", node: 1, include_nodeless: true });
      await resolve(h.traces[1], traceReply(100));
      assert.deepEqual(h.state.bufRef.current.map((frame) => frame.seq), [100]);
    }
  });
}

test("late clear and trace replies cannot overwrite a reconnected session", async (t) => {
  const h = mountHook(t);
  await h.tick();
  const clearing = h.state.clear();
  h.update({ running: false });
  h.update({ running: true });
  await resolve(h.traces[0], traceReply(40));
  assert.deepEqual(h.state.bufRef.current, []);
  await h.tick();
  assert.equal(h.traces[1].cursor, 0);
  await resolve(h.traces[1], traceReply(1));
  await resolve(h.clears[0], 99);
  await clearing;
  assert.deepEqual(h.state.bufRef.current.map((frame) => frame.seq), [1]);
  await h.tick();
  assert.equal(h.traces[2].cursor, 1);
});

test("refresh changes preserve buffered frames and discard the old pending reply", async (t) => {
  const h = mountHook(t);
  await h.tick();
  await resolve(h.traces[0], traceReply(1));
  await h.tick();
  h.update({ refreshHz: 90 });
  await h.tick();
  assert.equal(h.traces.length, 2);
  await resolve(h.traces[1], traceReply(2));
  assert.deepEqual(h.state.bufRef.current.map((frame) => frame.seq), [1]);
  await h.tick();
  assert.equal(h.traces[2].cursor, 1);
  await resolve(h.traces[2], traceReply(2, 3));
  assert.deepEqual(h.state.bufRef.current.map((frame) => frame.seq), [1, 2, 3]);
});

test("trace and clear failures release polling locks without losing the cursor", async (t) => {
  const h = mountHook(t);
  await h.tick();
  await act(async () => h.traces[0].reject(new Error("transient IPC failure")));
  await h.tick();
  assert.equal(h.traces.length, 2);
  await resolve(h.traces[1], traceReply(4));
  const clearing = h.state.clear();
  await act(async () => h.clears[0].reject(new Error("clear failed")));
  await clearing;
  await h.tick();
  assert.equal(h.traces[2].cursor, 4);
  await resolve(h.traces[2], traceReply(5));
  assert.deepEqual(h.state.bufRef.current.map((frame) => frame.seq), [4, 5]);
});

test("slow health polls stay serialized and recover after consecutive failures", async (t) => {
  const h = mountHook(t);
  for (let i = 0; i < 4; i++) await h.tickHealth();
  assert.equal(h.health.length, 1);
  const status: CanBusHealth = { supported: true, state: "error_active", tx_errors: 0, rx_errors: 0 };
  await resolve(h.health[0], status);
  assert.deepEqual(h.state.healthRef.current, status);
  for (let i = 1; i <= 3; i++) {
    await h.tickHealth();
    await act(async () => h.health[i].reject(new Error("controller temporarily unavailable")));
  }
  assert.equal(h.state.healthRef.current, null);
  await h.tickHealth();
  await resolve(h.health[4], status);
  assert.deepEqual(h.state.healthRef.current, status);
});

test("filter, mode and clear preserve CAN-FD capability until the connection changes", async (t) => {
  const h = mountHook(t);
  await h.tick();
  const reply = traceReply(1);
  reply.status.fd = true;
  reply.status.max_dlen = 64;
  await resolve(h.traces[0], reply);
  h.update({ filter: { kind: "node", node: 1, include_nodeless: true } });
  assert.equal(h.state.statusRef.current?.fd, true);
  h.update({ mode: "grouped", paused: true });
  assert.equal(h.state.statusRef.current?.max_dlen, 64);
  const clearing = h.state.clear();
  await resolve(h.clears[0], 10);
  await clearing;
  assert.equal(h.state.statusRef.current?.fd, true);
  assert.equal(h.state.statusRef.current?.max_dlen, 64);
  assert.equal(h.state.statusRef.current?.total, 0);
  const version = h.state.version;
  await h.tick();
  assert.ok(h.state.version > version, "status and grouped rendering must remain live");
  h.update({ running: false });
  assert.equal(h.state.statusRef.current, null);
  h.update({ running: true });
  assert.equal(h.state.statusRef.current, null, "a new connection must wait for its own capabilities");
});

test("local TX does not count as RX rate or activity, and capture failure clears activity", async (t) => {
  const h = mountHook(t);
  let now = 1000;
  t.mock.method(performance, "now", () => now);
  for (let i = 0; i < 3; i++) {
    await h.tick();
    const reply = traceReply(i + 1);
    reply.frames[0].dir = "tx";
    reply.status.rx_total = 0;
    await resolve(h.traces[i], reply);
    now += 400;
  }
  assert.equal(h.state.rateRef.current, 0);
  assert.equal(h.state.lastActivityRef.current, 0);
  await h.tick();
  const received = traceReply(4);
  received.status.rx_total = 1;
  await resolve(h.traces[3], received);
  assert.ok(h.state.rateRef.current > 0);
  assert.equal(h.state.lastActivityRef.current, now);
  h.update({ filter: { kind: "node", node: 1, include_nodeless: false } });
  assert.equal(h.state.lastActivityRef.current, now, "filter changes must not invent new RX activity");
  now += 400;
  await h.tick();
  const stopped = traceReply(4);
  stopped.status.rx_total = 1;
  stopped.status.capturing = false;
  stopped.status.connection_error = "CAN adapter disconnected";
  await resolve(h.traces[4], stopped);
  assert.equal(h.state.statusRef.current?.capturing, false);
  assert.equal(h.state.statusRef.current?.connection_error, "CAN adapter disconnected");
  assert.equal(h.state.rateRef.current, 0);
  assert.equal(h.state.lastActivityRef.current, 0);
});

test("frozen trace survives new frames, eviction, decode changes and scrolling", (t) => {
  const host = (name: string) => (props: any) => React.createElement(name, props, props.children);
  const { TraceList } = loadSource("../src/components/CanAnalyzerPanel.tsx", {
    antd: { Card: host("Card"), Tag: host("Tag"), Tooltip: host("Tooltip") },
    "@ant-design/icons": {},
    "../api": {}, "../canProfile": {}, "../format": {},
    "../i18n": { useI18n: () => ({ t: (key: string) => key }) },
    "../canopen": loadSource("../src/canopen.ts", {}),
    "../useCanTrace": {}, "./CanAnalyzerPanel.css": {},
  }, "\nexport { TraceList };\n");
  const frames = traceReply(...Array.from({ length: 40 }, (_, i) => i + 1)).frames;
  const evictedRef = { current: 0 };
  let props = { frames, evictedRef, interpret: true, paused: false, version: 0 };
  const scroll = { scrollTop: 0, scrollHeight: 880, clientHeight: 440 };
  let renderer: any;
  act(() => { renderer = create(React.createElement(TraceList, props), {
    createNodeMock: (element: any) => element.props.className === "can-trace-body" ? scroll : null,
  }); });
  t.after(() => act(() => renderer.unmount()));
  const update = (next: Partial<typeof props>) => {
    props = { ...props, ...next };
    act(() => renderer.update(React.createElement(TraceList, props)));
  };
  const seqs = () => renderer.root.findAllByProps({ className: "can-trace-row" })
    .map((row: any) => Number(row.findAllByType("span")[0].children[0]));
  update({ paused: true });
  const frozen = seqs();
  frames.splice(0, 40, ...traceReply(...Array.from({ length: 40 }, (_, i) => i + 101)).frames);
  evictedRef.current = 40;
  update({ version: 1, interpret: false });
  assert.deepEqual(seqs(), frozen);
  assert.equal(renderer.root.findAllByType("Tag").length, 0);
  scroll.scrollTop = 440;
  act(() => renderer.root.findByProps({ className: "can-trace-body" }).props.onScroll({ currentTarget: scroll }));
  assert.ok(seqs().every((seq: number) => seq <= 40));
  assert.equal(seqs()[0], 15);
  update({ paused: false });
  assert.ok(seqs().every((seq: number) => seq >= 101));
  update({ paused: true });
  update({ frames: [], version: 2 });
  assert.deepEqual(seqs(), [], "explicit clear must clear the frozen snapshot too");
});

test("SDO read/write handlers from one render submit one operation and log one result", async (t) => {
  const requests: Array<ReturnType<typeof deferred<string>> & { op: "read" | "write" }> = [];
  const submit = (op: "read" | "write") => {
    const request = { ...deferred<string>(), op };
    requests.push(request);
    return request.promise;
  };
  const host = (name: string) => (props: any) => React.createElement(name, props, props.children);
  const antd: any = Object.fromEntries(["Button", "Input", "InputNumber", "Select", "Space", "Tooltip"].map((name) => [name, host(name)]));
  antd.Space.Compact = host("Compact");
  antd.Typography = { Text: host("Text") };
  const { SdoWidget } = loadSource("../src/components/CanAnalyzerPanel.tsx", {
    antd,
    "@ant-design/icons": {},
    "../api": { api: { analyzerSdoRead: () => submit("read"), analyzerSdoWrite: () => submit("write") }, errMsg: String },
    "../canProfile": {},
    "../format": loadSource("../src/format.ts", {}),
    "../i18n": { useI18n: () => ({ t: (key: string) => key }) },
    "../canopen": {},
    "../useCanTrace": {},
    "./CanAnalyzerPanel.css": {},
  }, "\nexport { SdoWidget };\n");
  let renderer: any;
  act(() => { renderer = create(React.createElement(SdoWidget, { running: true })); });
  t.after(() => act(() => renderer.unmount()));
  act(() => renderer.root.findByType("Select").props.onChange("u8"));
  const [read, write] = renderer.root.findAllByType("Button");
  act(() => { read.props.onClick(); read.props.onClick(); write.props.onClick(); });
  assert.deepEqual(requests.map((request) => request.op), ["read"]);
  await resolve(requests[0], "0x1018:00 = 4");
  const resultLines = () => renderer.root.findAllByType("div").filter((node: any) => node.children.length === 1 && typeof node.children[0] === "string");
  assert.equal(resultLines().length, 1);
  const currentWrite = renderer.root.findAllByType("Button")[1];
  act(() => { currentWrite.props.onClick(); currentWrite.props.onClick(); });
  assert.deepEqual(requests.map((request) => request.op), ["read", "write"]);
  await resolve(requests[1], "0x1018:00 ← 4 OK");
  assert.equal(resultLines().length, 2);
  act(() => renderer.update(React.createElement(SdoWidget, { running: false })));
  act(() => { renderer.root.findAllByType("Button")[0].props.onClick(); });
  assert.equal(requests.length, 2);
});
