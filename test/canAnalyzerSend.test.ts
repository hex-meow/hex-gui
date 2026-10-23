import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test, { type TestContext } from "node:test";
import ts from "typescript";
import type { CanSendSpec } from "../src/types.ts";

const require = createRequire(import.meta.url);
const React = require("react");
const { act, create } = require("react-test-renderer");

type Props = { running: boolean; fd: boolean; maxDlen: number };

// Run the real component with React state/effects, replacing IPC and UI hosts.
// Timers are explicit so a queued callback can be tested after disconnecting.
function mountSender(t: TestContext, initial: Partial<Props> = {}) {
  const requests: CanSendSpec[] = [];
  const errors: string[] = [];
  const timers = new Map<number, () => Promise<void>>();
  let timerId = 0;
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", { configurable: true, value: {
    setInterval(callback: () => Promise<void>) { timers.set(++timerId, callback); return timerId; },
    clearInterval(id: number) { timers.delete(id); },
  } });
  const host = (name: string) => (props: any) => React.createElement(name, props, props.children);
  const antd: any = Object.fromEntries(["Button", "Checkbox", "Input", "InputNumber", "Space"].map((name) => [name, host(name)]));
  antd.Input.TextArea = host("TextArea");
  antd.Typography = { Text: host("Text") };
  antd.App = { useApp: () => ({ message: { error: (error: string) => errors.push(error) } }) };
  const mocks: Record<string, unknown> = {
    antd,
    "@ant-design/icons": {},
    "../api": { api: { analyzerSend: async (spec: CanSendSpec) => { requests.push(spec); } }, errMsg: String },
    "../canProfile": {},
    "../format": {},
    "../i18n": { useI18n: () => ({ t: (key: string) => key }) },
    "../canopen": {},
    "../useCanTrace": {},
    "./CanAnalyzerPanel.css": {},
  };
  const url = new URL("../src/components/CanAnalyzerPanel.tsx", import.meta.url);
  const { outputText } = ts.transpileModule(readFileSync(url, "utf8") + "\nexport { SendWidget };\n", {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2021 },
    fileName: url.pathname,
  });
  const module = { exports: {} as any };
  new Function("require", "module", "exports", outputText)(
    (name: string) => Object.hasOwn(mocks, name) ? mocks[name] : require(name), module, module.exports,
  );
  const { SendWidget } = module.exports;
  let props: Props = { running: true, fd: false, maxDlen: 8, ...initial };
  let renderer: any;
  act(() => { renderer = create(React.createElement(SendWidget, props)); });
  t.after(() => {
    act(() => renderer.unmount());
    if (previousWindow) Object.defineProperty(globalThis, "window", previousWindow);
    else Reflect.deleteProperty(globalThis, "window");
  });
  const checkbox = (label: string) => renderer.root.findAllByType("Checkbox").find((node: any) => node.props.children === label);
  return {
    requests, errors, timers, checkbox,
    get sendButton() { return renderer.root.findAllByType("Button")[0]; },
    get repeatButton() { return renderer.root.findAllByType("Button")[1]; },
    setData(value: string) { act(() => renderer.root.findByType("TextArea").props.onChange({ target: { value } })); },
    setId(value: string) { act(() => renderer.root.findByType("Input").props.onChange({ target: { value } })); },
    setChecked(label: string, checked: boolean) { act(() => checkbox(label).props.onChange({ target: { checked } })); },
    setRtrDlc(value: number) { act(() => renderer.root.findAllByType("InputNumber")[0].props.onChange(value)); },
    async send() { await act(async () => { await renderer.root.findAllByType("Button")[0].props.onClick(); }); },
    repeat() { act(() => renderer.root.findAllByType("Button")[1].props.onClick()); },
    async tick() { await act(async () => { for (const callback of [...timers.values()]) await callback(); }); },
    update(next: Partial<Props>) {
      props = { ...props, ...next };
      act(() => renderer.update(React.createElement(SendWidget, props)));
    },
  };
}

test("manual CAN send accepts complete hexadecimal bytes and rejects trailing garbage", async (t) => {
  const h = mountSender(t);
  for (const token of ["12zz", "1.5", "0x2q", "-1", "+01", "100", "0001", "0x"]) {
    h.setData(`11 ${token} 22`);
    await h.send();
  }
  assert.equal(h.requests.length, 0);
  assert.equal(h.errors.length, 8);
  h.setData("00, a 0Xff\n2B");
  await h.send();
  assert.deepEqual(h.requests[0].data, [0, 10, 255, 43]);
});

test("manual CAN send validates standard and extended ID widths", async (t) => {
  const h = mountSender(t);
  for (const id of ["0x800", "0x20000000", "0x10000000000000000"]) {
    h.setId(id);
    await h.send();
  }
  assert.equal(h.requests.length, 0);
  h.setId("0x7ff");
  await h.send();
  assert.equal(h.requests[0].id, 0x7ff);
  h.setChecked("canExt", true);
  h.setId("0x1fffffff");
  await h.send();
  assert.equal(h.requests[1].id, 0x1fffffff);
  assert.equal(h.requests[1].extended, true);
  h.setId("0x20000000");
  await h.send();
  assert.equal(h.requests.length, 2);
});

test("manual CAN send respects classic and current FD payload limits", async (t) => {
  const h = mountSender(t, { fd: true, maxDlen: 12 });
  h.setData(Array(9).fill("00").join(" "));
  await h.send();
  assert.equal(h.requests.length, 0);
  h.setChecked("FD", true);
  h.setData(Array(12).fill("00").join(" "));
  await h.send();
  assert.equal(h.requests[0].fd, true);
  assert.equal(h.requests[0].data.length, 12);
  h.setData(Array(13).fill("00").join(" "));
  await h.send();
  assert.equal(h.requests.length, 1);
});

test("losing FD capability stops repeats without silently sending classic frames", async (t) => {
  const h = mountSender(t, { fd: true, maxDlen: 64 });
  h.setChecked("FD", true);
  h.setChecked("BRS", true);
  h.repeat();
  await h.tick();
  assert.equal(h.requests[0].fd, true);
  h.update({ fd: false, maxDlen: 8 });
  assert.equal(h.timers.size, 0);
  await h.send();
  assert.equal(h.requests.length, 1);
  assert.match(h.errors.at(-1)!, /FD is unavailable/);
  assert.equal(h.checkbox("FD").props.checked, true);
  assert.equal(h.checkbox("FD").props.disabled, false);
  h.setChecked("FD", false);
  assert.equal(h.checkbox("BRS").props.checked, false);
  assert.equal(h.checkbox("RTR").props.disabled, false);
  await h.send();
  assert.equal(h.requests[1].fd, false);
});

test("repeat uses live fields and stops when edited data becomes invalid", async (t) => {
  const h = mountSender(t);
  h.repeat();
  await h.tick();
  h.setId("0x456");
  h.setData("aa bb");
  await h.tick();
  assert.equal(h.requests[1].id, 0x456);
  assert.deepEqual(h.requests[1].data, [170, 187]);
  h.setData("12zz");
  await h.tick();
  assert.equal(h.requests.length, 2);
  assert.equal(h.timers.size, 0);
  assert.equal(h.errors.length, 1);
});

test("disconnect blocks old button handlers and queued repeat callbacks", async (t) => {
  const h = mountSender(t);
  const send = h.sendButton.props.onClick;
  const repeat = h.repeatButton.props.onClick;
  h.repeat();
  const queued = [...h.timers.values()][0];
  h.update({ running: false });
  assert.equal(h.timers.size, 0);
  await act(async () => { await queued(); await send(); repeat(); });
  assert.equal(h.requests.length, 0);
  assert.equal(h.timers.size, 0);
  assert.equal(h.errors.length, 0);
});

test("one render cannot start duplicate repeat timers or overlapping sends", async (t) => {
  const h = mountSender(t);
  const repeat = h.repeatButton.props.onClick;
  act(() => { repeat(); repeat(); });
  assert.equal(h.timers.size, 1);
  const send = h.sendButton.props.onClick;
  await act(async () => { await Promise.all([send(), send()]); });
  assert.equal(h.requests.length, 1);
});

test("RTR sends validate DLC and do not parse an unused data field", async (t) => {
  const h = mountSender(t);
  h.setData("invalid");
  h.setChecked("RTR", true);
  for (const dlc of [-1, 1.5, 9]) {
    h.setRtrDlc(dlc);
    await h.send();
  }
  assert.equal(h.requests.length, 0);
  h.setRtrDlc(8);
  await h.send();
  assert.equal(h.requests[0].rtr, true);
  assert.equal(h.requests[0].dlc, 8);
  assert.deepEqual(h.requests[0].data, []);
});
