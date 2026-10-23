import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test, { type TestContext } from "node:test";
import ts from "typescript";
import * as canProfile from "../src/canProfile.ts";
import * as format from "../src/format.ts";
import type { ConnectionInfo, MotorInfo } from "../src/types.ts";

const require = createRequire(import.meta.url);
const React = require("react");
const { act, create } = require("react-test-renderer");

type Props = {
  connected: boolean;
  broadcastHeartbeat: boolean;
  devices: MotorInfo[];
  disconnectDisabled?: boolean;
  damiao?: boolean;
};
type ConnectRequest = [string, number, number, boolean, number | undefined];

// Deliberately incompatible with the CANopen host/device profile. DAMIAO can
// legitimately use this timing, while other workspaces must retain warnings.
const classicLink: ConnectionInfo = {
  backend: "gs_usb",
  fd_enabled: false,
  nominal: { bitrate: 250_000, sample_point_per_mille: 800 },
  data: null,
  inspection_error: null,
};
const motor: MotorInfo = {
  node_id: 1,
  friendly_name: "Device 1",
  identity: null,
  can_config: { status: "available", config: {
    nominal_bitrate: 1_000_000, data_bitrate: 5_000_000, transmit_pdo_brs: true,
  } },
  lifecycle: { kind: "Identified" },
  online: true,
  logic: null,
  nmt_state: "PreOperational",
  is_ready: false,
  can_initialize: true,
  peak_torque_nm: null,
  device_type: "motor",
};

// Keep the actual component, React state/effects, ID parser and CAN profile
// validation. Only Tauri IPC, translation and presentation hosts are replaced.
function mountConnection(t: TestContext, initial: Partial<Props> = {}) {
  const requests: ConnectRequest[] = [];
  const changes: boolean[] = [];
  const errors: string[] = [];
  let disconnects = 0;
  const previousNavigator = Object.getOwnPropertyDescriptor(globalThis, "navigator");
  Object.defineProperty(globalThis, "navigator", {
    configurable: true, value: { userAgent: "Windows" },
  });
  const host = (name: string) => (props: any) => React.createElement(name, props, props.children);
  const antd: any = Object.fromEntries(
    ["Alert", "Button", "Input", "Select", "Space", "Tag"].map((name) => [name, host(name)]),
  );
  antd.Typography = { Text: host("Text"), Paragraph: host("Paragraph") };
  antd.App = { useApp: () => ({ message: {
    success() {}, info() {}, error: (error: string) => errors.push(error),
  } }) };
  const mocks: Record<string, unknown> = {
    antd,
    "../api": { api: {
      async connect(...args: ConnectRequest) { requests.push(args); return classicLink; },
      async disconnect() { disconnects += 1; },
    }, errMsg: String },
    "../canProfile": canProfile,
    "../format": format,
    "../i18n": { useI18n: () => ({ t: (key: string) => key, lang: "en" }) },
  };
  const url = new URL("../src/components/ConnectBar.tsx", import.meta.url);
  const { outputText } = ts.transpileModule(readFileSync(url, "utf8"), {
    compilerOptions: {
      module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2021,
    },
    fileName: url.pathname,
  });
  const module = { exports: {} as any };
  new Function("require", "module", "exports", outputText)(
    (name: string) => Object.hasOwn(mocks, name) ? mocks[name] : require(name), module, module.exports,
  );
  const { ConnectBar } = module.exports;
  let props: Props = {
    connected: false, broadcastHeartbeat: true, devices: [motor], ...initial,
  };
  let renderer: any;
  const render = () => React.createElement(ConnectBar, { ...props, onChange(connected: boolean) {
    changes.push(connected);
    props = { ...props, connected };
    renderer.update(render());
  } });
  act(() => { renderer = create(render()); });
  t.after(() => {
    act(() => renderer.unmount());
    if (previousNavigator) Object.defineProperty(globalThis, "navigator", previousNavigator);
    else Reflect.deleteProperty(globalThis, "navigator");
  });
  const button = (label: string) => renderer.root.findAllByType("Button")
    .find((node: any) => node.props.children === label);
  return {
    requests, changes, errors, button,
    get disconnects() { return disconnects; },
    get inputs() { return renderer.root.findAllByType("Input"); },
    get selects() { return renderer.root.findAllByType("Select"); },
    get alerts() { return renderer.root.findAllByType("Alert"); },
    setInput(index: number, value: string) {
      act(() => renderer.root.findAllByType("Input")[index].props.onChange({ target: { value } }));
    },
    setBitrate(value: number) { act(() => renderer.root.findByType("Select").props.onChange(value)); },
    async click(label: string) {
      const node = button(label);
      assert.ok(node, `Missing ${label} button`);
      assert.ok(!node.props.disabled, `${label} must be enabled before clicking`);
      await act(async () => { await node.props.onClick(); });
    },
    update(next: Partial<Props>) {
      props = { ...props, ...next };
      act(() => renderer.update(render()));
    },
  };
}

test("CANopen connection retains its default profile, host node and heartbeat", async (t) => {
  const h = mountConnection(t);
  assert.equal(h.inputs.length, 2);
  assert.equal(h.inputs[1].props.value, "0x0A");
  assert.equal(h.selects[0].props.value, 5_000_000);
  assert.deepEqual(h.selects[0].props.options.map((option: any) => option.value),
    [1_000_000, 2_000_000, 4_000_000, 5_000_000]);
  await h.click("connect");
  assert.deepEqual(h.requests, [["gs_usb0", 5_000_000, 10, true, undefined]]);
  assert.deepEqual(h.changes, [true]);
  assert.deepEqual(h.alerts.map((node: any) => [node.props.type, node.props.message]), [
    ["warning", "hostCanProfileWarning"], ["error", "deviceCanProfileError"],
  ]);
  assert.deepEqual(h.errors, []);
});

test("CANopen connection preserves edited data rate, host node and heartbeat setting", async (t) => {
  const h = mountConnection(t, { broadcastHeartbeat: false });
  h.setInput(0, " gs_usb:1 ");
  h.setInput(1, "0x2A");
  h.setBitrate(2_000_000);
  await h.click("connect");
  assert.deepEqual(h.requests, [["gs_usb:1", 2_000_000, 42, false, undefined]]);
  assert.equal(h.alerts.length, 2);
});

for (const bitrate of [250_000, 3_200_000]) {
  test(`DAMIAO ${bitrate} connection uses dedicated timing without CANopen warnings`, async (t) => {
    const h = mountConnection(t, { damiao: true, broadcastHeartbeat: false });
    assert.equal(h.inputs.length, 1, "The CANopen host node field must be hidden");
    assert.equal(h.selects[0].props.value, 1_000_000);
    assert.deepEqual(h.selects[0].props.options.map((option: any) => option.value), [
      125_000, 200_000, 250_000, 500_000, 1_000_000,
      2_000_000, 2_500_000, 3_200_000, 4_000_000, 5_000_000,
    ]);
    h.setBitrate(bitrate);
    await h.click("connect");
    assert.deepEqual(h.requests, [["gs_usb0", 5_000_000, 10, false, bitrate]]);
    assert.deepEqual(h.changes, [true]);
    assert.equal(h.alerts.length, 0, "CANopen validation must not reject a DAMIAO profile");
    assert.equal(h.inputs[0].props.disabled, true);
    assert.equal(h.selects[0].props.disabled, true);
    assert.deepEqual(h.errors, []);
  });
}

for (const damiao of [false, true]) {
  test(`${damiao ? "DAMIAO" : "CANopen"} disconnect stays disabled during a protected operation`, async (t) => {
    const h = mountConnection(t, { connected: true, damiao, disconnectDisabled: true });
    assert.equal(h.button("disconnect").props.disabled, true);
    assert.equal(h.disconnects, 0);
    assert.deepEqual(h.changes, []);
    h.update({ disconnectDisabled: false });
    assert.equal(h.button("disconnect").props.disabled, false);
    await h.click("disconnect");
    assert.equal(h.disconnects, 1);
    assert.deepEqual(h.changes, [false]);
    assert.ok(h.button("connect"));
    assert.equal(h.alerts.length, 0);
  });
}
