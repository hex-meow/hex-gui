import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";
import ts from "typescript";
import type { ArmPreview, ArmTarget } from "../src/rawArm.ts";

const require = createRequire(import.meta.url);
const React = require("react");
const { create, act } = require("react-test-renderer");

function load(file: string, mocks: Record<string, unknown>) {
  const url = new URL(file, import.meta.url);
  const js = ts.transpileModule(readFileSync(url, "utf8"), {
    fileName: url.pathname,
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const exports: Record<string, any> = {};
  new Function("require", "exports", js)((id: string) => Object.hasOwn(mocks, id) ? mocks[id] : require(id), exports);
  return exports;
}

test("live previews coalesce drift, preserve the TCP solution and ignore superseded replies", async (t) => {
  let now = 0;
  t.mock.method(performance, "now", () => now);
  const timers = new Map<number, { run: () => void; at: number }>();
  let serial = 0;
  (globalThis as any).window = {
    setTimeout(run: () => void, delay: number) { timers.set(++serial, { run, at: now + delay }); return serial; },
    clearTimeout(id: number) { timers.delete(id); },
  };
  const calls: { target: ArmTarget; resolve: (p: ArmPreview) => void }[] = [];
  const { useRawArmPreview } = load("../src/useRawArmPreview.ts", {
    "./api": { errMsg: String },
    "./rawArm": { rawArm: { preview: (target: ArmTarget) => new Promise(resolve => calls.push({ target, resolve })) } },
  });
  const q = (v: number) => [v, 0, 0, 0, 0, 0];
  let measured = q(0);
  let request = { model: {}, available: true, target: { kind: "tcp", xyz: [1, 0, 0], rpy: null } as ArmTarget,
    limits: { velocity: 1, acceleration: 1, jerk: 1 } };
  let state: any, root: any;
  function Probe() { state = useRawArmPreview(request, measured, false); return null; }
  act(() => { root = create(React.createElement(Probe)); });
  t.after(() => { act(() => root.unmount()); Reflect.deleteProperty(globalThis, "window"); });
  const render = () => act(() => root.update(React.createElement(Probe)));
  const runAt = (time: number) => {
    now = time;
    act(() => {
      for (const [id, timer] of timers) {
        if (timer.at <= now) { timers.delete(id); timer.run(); }
      }
    });
  };
  const reply = async (index: number, start: number, end: number) => {
    await act(async () => { calls[index].resolve({ id: index + 1, goal: q(end), joints: [q(start), q(end)],
      duration: 1, path: [], tcp: { xyz: [1, 0, 0], rpy: [0, 0, 0] } }); });
  };

  runAt(0);
  assert.equal(calls[0].target.kind, "tcp");
  await reply(0, 0, 0.8);
  measured = q(0.009); render();
  assert.equal(timers.size, 0);
  assert.equal(state.executablePreview.id, 1);
  measured = q(0.01); render();
  assert.equal(state.executablePreview, null);
  measured = q(0.45); render();
  runAt(199); assert.equal(calls.length, 1);
  runAt(200); assert.deepEqual(calls[1].target, { kind: "joints", q: q(0.8) });
  measured = q(0.6); render();
  assert.equal(timers.size, 0);
  await reply(1, 0.45, 0.8);
  assert.equal(state.preview.joints[0][0], 0.45);
  assert.equal(state.executablePreview, null);
  runAt(399); assert.equal(calls.length, 2);
  runAt(400); assert.deepEqual(calls[2].target, calls[1].target);
  request = { ...request, target: { kind: "joints", q: q(-0.2) } };
  render();
  assert.equal(state.preview, null);
  runAt(470); assert.deepEqual(calls[3].target, request.target);
  await reply(3, 0.6, -0.2);
  await reply(2, 0.6, 0.8);
  assert.equal(state.executablePreview.id, 4);
});

test("arm viewer draws changed poses, camera, path and viewport on demand and idles on identical telemetry", async (t) => {
  const three = require("three");
  const frames = new Map<number, FrameRequestCallback>();
  let nextId = 0, draws = 0, resizes = 0;
  let orbit: any;
  const win = new EventTarget() as any;
  win.devicePixelRatio = 1;
  (globalThis as any).window = win;
  (globalThis as any).requestAnimationFrame = (callback: FrameRequestCallback) => { frames.set(++nextId, callback); return nextId; };
  (globalThis as any).cancelAnimationFrame = (id: number) => frames.delete(id);
  t.after(() => { for (const name of ["window", "requestAnimationFrame", "cancelAnimationFrame"]) Reflect.deleteProperty(globalThis, name); });
  class Renderer {
    domElement = new EventTarget();
    render() { draws++; }
    setSize() { resizes++; }
    setPixelRatio() {}
    dispose() {}
  }
  class Controls extends three.EventDispatcher {
    target = new three.Vector3();
    constructor() { super(); orbit = this; }
    update() {}
    dispose() {}
  }
  class Robot extends three.Group {
    joints = { joint_1: { jointType: "revolute", value: 0 } };
    links = {};
    setJointValue(_: string, value: number) {
      const changed = this.joints.joint_1.value !== value;
      this.joints.joint_1.value = value;
      return changed;
    }
  }
  const { ArmViewer } = load("../src/components/ArmViewer.tsx", {
    three: { ...three, WebGLRenderer: Renderer },
    "three/addons/controls/OrbitControls.js": { OrbitControls: Controls },
    "../urdfModelLoader": {
      beginUrdfLoad: () => ({ promise: Promise.resolve({ robot: new Robot() }), cancel() {} }),
      disposeObject3D() {}, isUrdfAbort: () => false,
    },
  });
  const mount = { clientWidth: 600, appendChild() {} };
  let props = { q: [0], jointNames: ["joint_1"], gravity: [0, 0, -9.8], previewQ: [1], tcpPath: [[0, 0, 0]] };
  let root: any;
  await act(async () => { root = create(React.createElement(ArmViewer, props), { createNodeMock: () => mount }); });
  const render = () => act(() => root.update(React.createElement(ArmViewer, props)));
  const draw = () => {
    assert.equal(frames.size, 1);
    const [[id, cb]] = frames;
    frames.delete(id); cb(0);
    assert.equal(frames.size, 0);
  };
  draw(); assert.equal(draws, 1);
  props = { ...props, q: [0], jointNames: ["joint_1"], previewQ: [1], gravity: [0, 0, -9.8] };
  render(); assert.equal(frames.size, 0);
  props = { ...props, q: [0.4], previewQ: [0.5] };
  render(); draw(); assert.equal(draws, 2);
  props = { ...props, tcpPath: [] };
  render(); draw();
  orbit.dispatchEvent({ type: "change" }); draw();
  mount.clientWidth = 700; win.dispatchEvent(new Event("resize")); draw(); assert.equal(resizes, 2);
  orbit.dispatchEvent({ type: "change" });
  act(() => root.unmount()); assert.equal(frames.size, 0);
});
