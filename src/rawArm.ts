import { invoke } from "@tauri-apps/api/core";

export interface Pose { xyz: number[]; rpy: number[] }
export type ToolKind = "gp80" | "gr80";
export type Product = "firefly_y6" | ToolKind;
interface ModelJoint { name: string; lower: number; upper: number }
export interface MountedTool { kind: ToolKind; joint: ModelJoint }
export interface RawModel { joints: ModelJoint[]; home: number[]; tcp: Pose; tool: MountedTool | null }
export interface MotionLimits { velocity: number; acceleration: number; jerk: number }
export interface ArmGains { kp: number[]; kd: number[]; damping: number }
export interface Installation {
  direction: number[];
  offset: number[];
  factor: (number | null)[];
  finger_m_per_rev: number;
  finger_offset_m: number;
  jaw_rad_per_rev: number;
  jaw_offset_rad: number;
}
export type ArmTarget = { kind: "joints"; q: number[] } | { kind: "tcp"; xyz: number[]; rpy: number[] | null };
export interface ArmPreview { id: number; goal: number[]; duration: number; tcp: Pose; joints: number[][]; path: number[][] }
export interface GripperMotion {
  kind: ToolKind; target: "open" | "closed" | { position: number };
  speed: number; acceleration: number; max_torque_permille: number;
}
export interface ToolFeedback { position: number; velocity: number | null; torque_permille: number | null; mode_display: number | null }
export interface ArmFeedback {
  q: number[]; velocity: number[]; tool: ToolFeedback | null; tcp: Pose; gravity_nm: number[] | null; moving: boolean;
  preview_seed: { q: number[]; tcp: Pose } | { error: string };
}
export interface ArmStatus {
  mode: "monitor" | "hold" | "float" | "recovery";
  gravity_scale: number[];
  feedback: ArmFeedback | null;
  error: string | null;
}
interface Connection {
  iface: string; data_bitrate: number; installation: Installation; factory: (number | null)[];
  arm_xml: string; tool_xml: string | null; model: RawModel;
}

export const rawArm = {
  session: () => invoke<Connection | null>("raw_arm_session"),
  model: (armXml: string, toolXml: string | null) => invoke<RawModel>("raw_arm_model", { armXml, toolXml }),
  connect: (iface: string, dataBitrate: number, installation: Installation) =>
    invoke<(number | null)[]>("raw_arm_connect", { iface, dataBitrate, installation }),
  disconnect: () => invoke<void>("raw_arm_disconnect"),
  status: () => invoke<ArmStatus>("raw_arm_status"),
  preview: (target: ArmTarget, limits: MotionLimits) => invoke<ArmPreview>("raw_arm_preview", { target, limits }),
  control: (mode: "hold" | "float", gains: ArmGains) =>
    invoke<void>("raw_arm_control", { mode, gains }),
  setGravityScale: (scale: number[]) => invoke<void>("raw_arm_set_gravity_scale", { scale }),
  execute: (id: number, gains: ArmGains) => invoke<void>("raw_arm_execute", { id, gains }),
  moveGripper: (motion: GripperMotion) => invoke<void>("raw_arm_move_gripper", { motion }),
  disableGripper: () => invoke<void>("raw_arm_disable_gripper"),
  offsets: (offsets: number[]) => invoke<void>("raw_arm_offsets", { offsets }),
  stop: (limits: MotionLimits) => invoke<void>("raw_arm_stop", { limits }),
  release: () => invoke<void>("raw_arm_release"),
};

const TAU = 2 * Math.PI;
// Firefly Y6 v1.1 installation reference in this GUI's original six-axis profile.
// All six measured offsets can be replaced for a particular arm.
export const foldedQ = [0, -1.57, 3.14, 0, 0, 0];
const parkRev = [0.000269, 0.249714, 0.251193, -0.00159, -0.004028, 0.000018];
const signs = [-1, -1, 1, 1, 1, 1];
export const referenceInstallation: Installation = {
  direction: signs,
  offset: foldedQ.map((q, i) => q - signs[i] * TAU * parkRev[i]),
  factor: Array(6).fill(null),
  finger_m_per_rev: 0.007 * TAU,
  finger_offset_m: 0,
  // GR80 v0.0.1 describes 0.6 rad of jaw travel per quarter turn of the gear.
  jaw_rad_per_rev: 0.6 / 0.25,
  jaw_offset_rad: 0,
};

// The GP80 URDF joint is one finger in metres. Its command and readout use full width.
export const gripperUnits = {
  gp80: { displayPerJoint: 2000, siPerDisplay: 0.001, unit: "mm", step: 0.1 },
  gr80: { displayPerJoint: 1, siPerDisplay: 1, unit: "rad", step: 0.01 },
} as const;

export const defaultGains: ArmGains = { kp: Array(6).fill(80), kd: Array(6).fill(15), damping: 0.8 };
export const defaultGravityScale: number[] = Array(6).fill(1);
export const defaultLimits: MotionLimits = { velocity: 0.5, acceleration: 1, jerk: 4 };

export function samplePreview(preview: ArmPreview, fraction: number): number[] {
  const last = preview.joints.length - 1;
  const t = Math.max(0, Math.min(1, fraction)) * last;
  const index = Math.floor(t);
  const next = preview.joints[Math.min(last, index + 1)];
  return preview.joints[index].map((v, i) => v + (next[i] - v) * (t - index));
}

function parseUrdf(xml: string) {
  const doc = new DOMParser().parseFromString(xml, "application/xml");
  if (doc.documentElement.tagName !== "robot") {
    throw new Error(doc.querySelector("parsererror")?.textContent ?? "Expected a URDF robot element");
  }
  return doc;
}

export function importProductUrdf(xml: string): { product: Product; xml: string } {
  const doc = parseUrdf(xml);
  const robot = doc.documentElement;
  const name = robot.getAttribute("name");
  if (name === "gp80" || name === "gr80" || name === "firefly_y6") return { product: name, xml };
  if (robot.querySelector('joint[name="joint_1"]')) return { product: "firefly_y6", xml };
  // Imported copies can have a custom robot name. The driver's gripper API
  // requires the canonical product name as well as the independent joint.
  for (const [product, selector] of [
    ["gp80", 'joint[name="finger_joint1"][type="prismatic"]'],
    ["gr80", 'joint[name="jaw_joint1"][type="revolute"]'],
  ] as const) {
    if (robot.querySelector(selector)) {
      robot.setAttribute("name", product);
      return { product, xml: new XMLSerializer().serializeToString(doc) };
    }
  }
  throw new Error("Expected a Firefly Y6, GP80 or GR80 URDF");
}

/** Mount the gripper's base_link coincident with link_6, as in hex_arm::ToolModel.
 * Prefix the tool's link, joint, mimic and material references together. */
export function assembledUrdf(armXml: string, toolXml: string | null): string {
  const doc = parseUrdf(armXml);
  if (!toolXml) return armXml;
  const tool = parseUrdf(toolXml);
  for (const [selector, attr] of [
    ["link[name], joint[name], material[name]", "name"], ["parent[link], child[link]", "link"], ["mimic[joint]", "joint"],
  ]) {
    for (const el of tool.querySelectorAll(selector)) {
      const name = el.getAttribute(attr);
      if (name) el.setAttribute(attr, `ee_${name}`);
    }
  }
  for (const el of tool.documentElement.children) doc.documentElement.appendChild(doc.importNode(el, true));
  const mount = doc.createElement("joint");
  mount.setAttribute("name", `${tool.documentElement.getAttribute("name")}_mount`);
  mount.setAttribute("type", "fixed");
  for (const [tag, link] of [["parent", "link_6"], ["child", "ee_base_link"]]) {
    const el = doc.createElement(tag);
    el.setAttribute("link", link);
    mount.appendChild(el);
  }
  doc.documentElement.appendChild(mount);
  return new XMLSerializer().serializeToString(doc);
}
