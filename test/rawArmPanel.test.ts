import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test, { type TestContext } from "node:test";
import ts from "typescript";
import type { FactoryReading, RawArmPreferences } from "../src/rawArm.ts";

const require = createRequire(import.meta.url);
const React = require("react");
const { act, create } = require("react-test-renderer");

function load(path: string, mocks: Record<string, unknown>, fallback: (id: string) => unknown = require) {
  const url = new URL(path, import.meta.url);
  const js = ts.transpileModule(readFileSync(url, "utf8"), {
    fileName: url.pathname,
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const exports: Record<string, any> = {};
  new Function("require", "exports", js)((id: string) => Object.hasOwn(mocks, id) ? mocks[id] : fallback(id), exports);
  return exports;
}

test("the launcher resumes an active Rust session unless the user has selected a tool", async t => {
  let lookup = deferred<object | null>();
  const translate = (key: string) => key;
  const message = {};
  const Layout = Object.assign(({ children }: any) => children, { Content: "main" });
  const { default: App } = load("../src/App.tsx", {
    "./rawArm": { rawArm: { session: () => lookup.promise } },
    "./api": { api: { disconnect: async () => {} } },
    "./i18n": { useI18n: () => ({ t: translate, lang: "en" }) },
    "./components/RawArmPanel": { RawArmPanel: "RawArmPanel" },
    "./components/ZenohPanel": { ZenohPanel: "ZenohPanel" },
    "./components/Tutorial": { TUTORIALS: {}, TutorialModal: "TutorialModal" },
    "@tauri-apps/api/event": { listen: async () => () => {} },
    antd: { App: { useApp: () => ({ message }) }, Button: "Button", Tooltip: "Tooltip",
      Collapse: "Collapse", Layout, Typography: { Title: "h2", Text: "span" } },
  }, id => id.startsWith("./") ? {} : require(id));
  let node: any;
  t.after(() => { if (node) act(() => node.unmount()); });
  const mount = () => act(() => { node = create(React.createElement(App)); });
  mount();
  await act(async () => { lookup.resolve({ iface: "can0" }); });
  assert.equal(node.root.findAllByType("RawArmPanel").length, 1);
  act(() => node.unmount());
  lookup = deferred<object | null>();
  mount();
  const pickZenoh = node.root.findAllByType("button").find((button: any) =>
    button.findAllByType("span").some((span: any) => span.props.children === "toolBaseZenoh"));
  act(() => pickZenoh.props.onClick());
  assert.equal(node.root.findAllByType("ZenohPanel").length, 1);
  await act(async () => { await node.root.findByProps({ className: "app-chrome__back" }).props.onClick(); });
  await act(async () => { lookup.resolve({ iface: "can0" }); });
  assert.equal(node.root.findAllByType("RawArmPanel").length, 0);
  assert.equal(node.root.findAllByProps({ className: "tool-picker" }).length, 1);
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
}

function panel(t: TestContext) {
  let node: any;
  t.after(() => { if (node) act(() => node.unmount()); });
  function installGlobal(key: string, value: unknown) {
    const previous = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { value, configurable: true });
    t.after(() => {
      if (previous) Object.defineProperty(globalThis, key, previous);
      else Reflect.deleteProperty(globalThis, key);
    });
  }
  const storageCalls: string[] = [];
  installGlobal("localStorage", {
    getItem(key: string) {
      storageCalls.push(key);
      return JSON.stringify({ installation: { offset: Array(6).fill(0.42) } });
    },
    setItem(key: string) { storageCalls.push(key); },
  });
  installGlobal("window", { setTimeout() { return 1; }, clearTimeout() {} });
  installGlobal("navigator", { userAgent: "Linux" });
  installGlobal("fetch", async () => ({ ok: true, text: async () => "<robot />" }));
  const calls: { command: string; args: any }[] = [];
  const ipc: Record<string, (args: any) => unknown> = {};
  const api = load("../src/rawArm.ts", { "@tauri-apps/api/core": {
    invoke: async (command: string, args: any) => {
      calls.push({ command, args });
      assert.ok(ipc[command], `unexpected IPC: ${command}`);
      return ipc[command](args);
    },
  } });
  const defaults: RawArmPreferences = api.defaultRawArmPreferences();
  const pose = { xyz: [0, 0, 0.2], rpy: [0, 0, 0] };
  const q = Array(6).fill(0);
  const model = { joints: q.map((_, i) => ({ name: `joint_${i + 1}`, lower: -3, upper: 3 })), home: q, tcp: pose,
    tool: { kind: "gp80", joint: { name: "finger_joint1", lower: 0, upper: 0.04 } } };
  const factory: FactoryReading[] = q.map(() => ({ state: "valid", factor: 1 }));
  const session = { iface: "can0", data_bitrate: 5000000, arm_xml: "<robot />", tool_xml: "<tool />", model, factory, gravity_scale: q.map(() => 1),
    installation: defaults.installation, gains: defaults.gains, limits: defaults.limits, goal_tolerance: defaults.goalTolerance };
  const status = { mode: "monitor", gravity_scale: q.map(() => 1), goal_tolerance: defaults.goalTolerance, error: null,
    feedback: { tcp: pose, q, preview_seed: { q, tcp: pose }, tool: { position: 0 }, gravity_nm: null } };
  Object.assign(ipc, {
    raw_arm_session: () => null, raw_arm_model: () => model, raw_arm_connect: () => factory, raw_arm_disconnect: () => undefined,
    raw_arm_status: () => status,
  });
  const hosts: any = Object.fromEntries(["Alert", "Button", "Card", "Checkbox", "Input", "InputNumber", "Segmented", "Select", "Slider", "Space", "Tag"]
    .map(k => [k, k]));
  hosts.App = { useApp: () => ({ message: { success() {} } }) };
  hosts.Collapse = ({ items }: any) => items.map((item: any) => React.createElement("section", { key: item.key }, item.children));
  const mocks = { antd: hosts, "../i18n": { useI18n: () => ({ lang: "en" }) }, "../rawArm": { ...api, assembledUrdf: () => "assembled" } };
  const settings = load("../src/components/RawArmSettings.tsx", mocks);
  const { RawArmPanel } = load("../src/components/RawArmPanel.tsx", {
    ...mocks, "../api": { errMsg: String }, "../canProfile": { isGsUsbSpec: () => false },
    "../urdfModelLoader": { URDF_ASSET_PRODUCTS: { firefly_y6: { urdfUrl: "arm.urdf" }, gp80: { urdfUrl: "tool.urdf" } } },
    "./RawArmSettings": settings, "./RawArmPanel.css": {}, "./RawArmGripper": { RawArmGripper: "Gripper" },
    "../useRawArmPreview": { useRawArmPreview: () => ({}) }, "./RawArmPreview": { RawArmPreview: () => null },
  });
  return {
    calls, ipc, session, status, storageCalls, defaults,
    async open() {
      if (node) act(() => node.unmount());
      await act(async () => { node = create(React.createElement(RawArmPanel)); });
    },
    button: (label: string) => node.root.findAllByType("Button").find((item: any) =>
      item.props.children === label || item.props["aria-label"] === label),
    input: (label: string) => node.root.findAllByType("InputNumber").find((item: any) => item.props["aria-label"] === label),
    get settings() { return node.root.findByType(settings.RawArmSettings).props; },
    get gripper() { return node.root.findByType("Gripper").props; },
    get alerts() { return node.root.findAllByType("Alert").map((item: any) => item.props.message); },
  };
}

test("opening starts with source defaults, reconnect keeps edits, and a live session restores its effective settings", async t => {
  const p = panel(t);
  await p.open();
  const expected = p.settings.prefs;
  assert.deepEqual(expected.installation.direction, [-1, -1, 1, 1, 1, 1]);
  assert.deepEqual(expected.installation.offset, Array(6).fill(0));
  assert.deepEqual(expected.installation.factor, Array(6).fill(null));
  assert.deepEqual(expected.installation.max_torque_permille, Array(6).fill(null));
  assert.deepEqual(expected.gains, { kp: Array(6).fill(80), kd: Array(6).fill(15), damping: 0.01 });
  assert.equal(expected.installation.finger_m_per_rev, 0.0439822971502571);
  assert.equal(expected.installation.jaw_rad_per_rev, 2.4);
  const edited = { ...expected, gains: { ...expected.gains, damping: 0.2 }, installation: { ...expected.installation, offset: [0.42, 0, 0, 0, 0, 0] } };
  act(() => { p.settings.change(edited); });
  for (const label of ["Connect and initialize", "Disconnect", "Connect and initialize", "Disconnect"]) {
    await act(async () => { p.button(label).props.onClick(); });
  }
  for (const call of p.calls.filter(c => c.command === "raw_arm_connect")) {
    assert.deepEqual(call.args.installation, edited.installation);
    assert.deepEqual(call.args.gains, edited.gains);
  }
  await p.open();
  assert.deepEqual(p.settings.prefs, expected);
  p.ipc.raw_arm_session = () => ({ ...p.session, gains: edited.gains, installation: edited.installation });
  const before = p.calls.length;
  await p.open();
  assert.deepEqual(p.settings.prefs, edited);
  assert.deepEqual(p.calls.slice(before).map(c => c.command), ["raw_arm_session", "raw_arm_status"]);
  assert.deepEqual(p.storageCalls, []);
});

test("reconnect retains gripper settings and keeps gravity drafts separate from applied scales", async t => {
  const p = panel(t);
  p.ipc.raw_arm_set_gravity_scale = ({ scale }) => { p.status.gravity_scale = [...scale]; };
  await p.open();
  const applied = [1, 0.8, 1, 1, 1, 1];
  const editedGripper = { speed: 18, acceleration: 24, torque: 150 };
  const gravity = () => p.input("J2 gravity feedforward scale");
  const click = async (label: string) => {
    await act(async () => { p.button(label).props.onClick(); });
  };
  await click("Connect and initialize");
  act(() => { gravity().props.onChange(0.8); p.gripper.setSettings(editedGripper); });
  await click("Apply scales");
  act(() => gravity().props.onChange(1.2));
  await click("Disconnect");
  assert.equal(gravity().props.value, 1.2);
  assert.deepEqual(p.gripper.settings, editedGripper);
  const beforeReconnect = p.calls.length;
  await click("Connect and initialize");
  assert.deepEqual(p.calls.slice(beforeReconnect).map(c => c.command), ["raw_arm_connect", "raw_arm_status"]);
  assert.deepEqual(p.calls[beforeReconnect].args.gravityScale, applied);
  assert.equal(gravity().props.value, 1.2);
  assert.equal(p.button("Apply scales").props.disabled, false);
  assert.deepEqual(p.gripper.settings, editedGripper);
  await click("Disconnect");
  await p.open();
  assert.deepEqual(p.gripper.settings, { speed: 10, acceleration: 20, torque: 120 });
  assert.equal(gravity().props.value, 1);
});

test("J3 reread updates only its row and a failed retry retains the last valid factor", async t => {
  const p = panel(t);
  p.session.factory[2] = { state: "error", error: "timeout", last_valid: null };
  p.ipc.raw_arm_session = () => p.session;
  await p.open();
  assert.equal(p.button("Hold current pose").props.disabled, true);
  const started = deferred<FactoryReading>();
  p.ipc.raw_arm_reread_factory_calibration = () => started.promise;
  await act(async () => { p.button("J3 reread calibration").props.onClick(); });
  assert.equal(p.button("J3 reread calibration").props.loading, true);
  assert.equal(p.button("J4 reread calibration").props.loading, false);
  await act(async () => { started.resolve({ state: "valid", factor: 0.85 }); });
  assert.deepEqual(p.settings.factory[2], { state: "valid", factor: 0.85 });
  assert.deepEqual(p.settings.factory[3], { state: "valid", factor: 1 });
  assert.equal(p.button("Hold current pose").props.disabled, false);
  p.ipc.raw_arm_reread_factory_calibration = () => ({ state: "error", error: "timeout", last_valid: 0.85 });
  await act(async () => { p.button("J3 reread calibration").props.onClick(); });
  assert.equal(p.button("Hold current pose").props.disabled, false);
  assert.deepEqual(p.calls.filter(c => c.command === "raw_arm_reread_factory_calibration").map(c => c.args), [{ nodeId: 3 }, { nodeId: 3 }]);
});

test("an outstanding gripper request allows braking and release, including while brake is pending", async t => {
  const p = panel(t);
  p.ipc.raw_arm_session = () => p.session;
  p.status.mode = "hold";
  const gripper = deferred<void>(), brake = deferred<void>();
  p.ipc.raw_arm_move_gripper = () => gripper.promise;
  p.ipc.raw_arm_stop = () => brake.promise;
  p.ipc.raw_arm_release = () => { p.status.mode = "monitor"; };
  await p.open();
  await act(async () => { p.gripper.move({}); });
  assert.equal(p.button("Brake and hold").props.disabled, false);
  await act(async () => { p.button("Brake and hold").props.onClick(); });
  const release = p.button("Release arm and gripper");
  assert.equal(release.props.disabled, false);
  await act(async () => { release.props.onClick(); release.props.onClick(); });
  await act(async () => { brake.reject(new Error("obsolete brake")); gripper.reject(new Error("obsolete gripper")); });
  assert.deepEqual(p.calls.filter(c => ["raw_arm_move_gripper", "raw_arm_stop", "raw_arm_release"].includes(c.command)).map(c => c.command),
    ["raw_arm_move_gripper", "raw_arm_stop", "raw_arm_release"]);
  assert.deepEqual(p.alerts, []);
  assert.equal(p.button("Release arm and gripper").props.disabled, true);
});
