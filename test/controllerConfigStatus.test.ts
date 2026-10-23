import assert from "node:assert/strict";
import test from "node:test";
import { recoveryErrors } from "../src/controllerConfigStatus.ts";
import type { ConfigGetDto, LogLine } from "../src/types.ts";

const failure: LogLine = {
  proc: "launcher",
  ts_ns: 9_000_000_000,
  level: "E",
  target: "launcher",
  msg: "子进程 Fatal 报告 → 立即进入恢复模式: phase=CanDevicePreflight code=arm.startup.can_device_preflight_failed summary=CAN can0 device preflight failed: expected node 0x01 was not discovered",
};

function config(overrides: Partial<ConfigGetDto> = {}): ConfigGetDto {
  return {
    yaml: "robots: [{ id: arm0, kind: arm, can: can0 }]",
    sha256: "unchanged-config",
    path: "/userdata/hexmeow/launch.yaml",
    mtime_unix: 0,
    schema_version: null,
    recovery_mode: true,
    recovery_logs: [failure],
    ...overrides,
  };
}

test("valid file in recovery preserves the hardware failure instead of inventing a YAML error", () => {
  const lines = recoveryErrors(config());
  assert.equal(lines[0].msg, failure.msg);
  assert.match(lines[0].msg, /CanDevicePreflight/);
  assert.match(lines[0].msg, /expected node 0x01 was not discovered/);
});

test("leaving recovery with the same file hides old failures", () => {
  assert.deepEqual(recoveryErrors(config({ recovery_mode: false })), []);
});

test("unavailable or empty logs do not manufacture an invalid configuration diagnosis", () => {
  assert.deepEqual(recoveryErrors(config({ recovery_logs: null })), []);
  assert.deepEqual(recoveryErrors(config({ recovery_logs: [] })), []);
});

test("newest failure is shown first and later warnings do not replace it", () => {
  const newer = { ...failure, msg: "配置不可用 → 进入恢复模式:YAML 解析失败" };
  const warning = { ...failure, level: "W", msg: "public frontend reconnecting" };
  assert.deepEqual(recoveryErrors(config({ recovery_logs: [failure, newer, warning] })), [newer, failure]);
});
