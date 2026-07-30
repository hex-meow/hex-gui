import assert from "node:assert/strict";
import test from "node:test";

import {
  isGsUsbSpec,
  validateDeviceProfiles,
  validateHostProfile,
} from "../src/canProfile.ts";
import type { ConnectionInfo, MotorInfo } from "../src/types.ts";

function host(dataBitrate = 5_000_000, dataSamplePoint = 750): ConnectionInfo {
  return {
    backend: "socketcan",
    fd_enabled: true,
    nominal: { bitrate: 1_000_000, sample_point_per_mille: 800 },
    data: {
      bitrate: dataBitrate,
      sample_point_per_mille: dataSamplePoint,
    },
    inspection_error: null,
  };
}

function device(overrides: Partial<MotorInfo> = {}): MotorInfo {
  return {
    node_id: 1,
    friendly_name: "Device 1",
    identity: null,
    can_config: {
      status: "available",
      config: {
        nominal_bitrate: 1_000_000,
        data_bitrate: 5_000_000,
        transmit_pdo_brs: true,
      },
    },
    lifecycle: { kind: "Identified" },
    online: true,
    logic: null,
    nmt_state: "PreOperational",
    is_ready: false,
    can_initialize: true,
    peak_torque_nm: null,
    device_type: "unknown",
    ...overrides,
  };
}

test("accepts all four standard host data profiles", () => {
  for (const [rate, samplePoint] of [
    [1_000_000, 800],
    [2_000_000, 800],
    [4_000_000, 800],
    [5_000_000, 750],
  ]) {
    assert.deepEqual(validateHostProfile(host(rate, samplePoint)), []);
  }
});

test("reports host timing mismatches without rejecting the connection", () => {
  const issues = validateHostProfile({
    ...host(4_000_000, 750),
    inspection_error: "netlink unavailable",
  });
  assert.ok(issues.some((issue) => issue.code === "inspection_failed"));
  assert.ok(issues.some((issue) => issue.code === "data_sample_point"));
});

test("device rate must match host but transmit BRS may differ", () => {
  const noBrs = device({
    can_config: {
      status: "available",
      config: {
        nominal_bitrate: 1_000_000,
        data_bitrate: 5_000_000,
        transmit_pdo_brs: false,
      },
    },
  });
  assert.deepEqual(validateDeviceProfiles(host(), [noBrs]), []);

  const mismatch = device({
    can_config: {
      status: "available",
      config: {
        nominal_bitrate: 1_000_000,
        data_bitrate: 4_000_000,
        transmit_pdo_brs: true,
      },
    },
  });
  assert.ok(
    validateDeviceProfiles(host(), [mismatch]).some(
      (issue) => issue.code === "data_bitrate",
    ),
  );
});

test("pending, unsupported and read failures remain distinct", () => {
  assert.deepEqual(
    validateDeviceProfiles(host(), [
      device({ can_config: { status: "pending" } }),
    ]),
    [],
  );
  assert.equal(
    validateDeviceProfiles(host(), [
      device({ can_config: { status: "unsupported" } }),
    ])[0]?.code,
    "unsupported",
  );
  assert.equal(
    validateDeviceProfiles(host(), [
      device({
        can_config: { status: "read_failed", reason: "SDO timeout" },
      }),
    ])[0]?.code,
    "read_failed",
  );
});

test("recognizes supported gs_usb spelling without confusing SocketCAN names", () => {
  for (const spec of ["gs_usb", "gs_usb0", "gs_usb:2", "gsusb3"]) {
    assert.equal(isGsUsbSpec(spec), true);
  }
  for (const spec of ["can0", "socketcan:can0", "gs_usb:bad"]) {
    assert.equal(isGsUsbSpec(spec), false);
  }
});
