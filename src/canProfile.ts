import type { ConnectionInfo, MotorInfo } from "./types.ts";

export const STANDARD_DATA_BITRATES = [
  1_000_000,
  2_000_000,
  4_000_000,
  5_000_000,
] as const;

export type StandardDataBitrate = (typeof STANDARD_DATA_BITRATES)[number];

export type HostProfileIssueCode =
  | "inspection_failed"
  | "fd_unknown"
  | "fd_disabled"
  | "nominal_unknown"
  | "nominal_bitrate"
  | "nominal_sample_point"
  | "data_unknown"
  | "data_bitrate"
  | "data_sample_point";

export interface HostProfileIssue {
  code: HostProfileIssueCode;
  actual?: number | string;
  expected?: number | string;
}

export type DeviceProfileIssueCode =
  | "unsupported"
  | "read_failed"
  | "nominal_bitrate"
  | "data_bitrate"
  | "classic_on_fd";

export interface DeviceProfileIssue {
  nodeId: number;
  name: string;
  code: DeviceProfileIssueCode;
  actual?: number | string;
  expected?: number | string;
  reason?: string;
}

export function isGsUsbSpec(spec: string): boolean {
  const normalized = spec.trim().toLowerCase();
  const rest = normalized.startsWith("gs_usb")
    ? normalized.slice("gs_usb".length)
    : normalized.startsWith("gsusb")
      ? normalized.slice("gsusb".length)
      : null;
  if (rest == null) return false;
  const channel = rest.startsWith(":") ? rest.slice(1) : rest;
  return channel === "" || /^\d+$/.test(channel);
}

export function expectedDataSamplePoint(bitrate: number): number | null {
  if (bitrate === 5_000_000) return 750;
  if (
    bitrate === 1_000_000 ||
    bitrate === 2_000_000 ||
    bitrate === 4_000_000
  ) {
    return 800;
  }
  return null;
}

/** Validate the production manager profile. Analyzer sessions do not call it. */
export function validateHostProfile(info: ConnectionInfo): HostProfileIssue[] {
  const issues: HostProfileIssue[] = [];
  if (info.inspection_error) {
    issues.push({
      code: "inspection_failed",
      actual: info.inspection_error,
    });
  }

  if (info.fd_enabled == null) {
    issues.push({ code: "fd_unknown" });
  } else if (!info.fd_enabled) {
    issues.push({ code: "fd_disabled" });
  }

  if (!info.nominal || info.nominal.bitrate == null) {
    issues.push({ code: "nominal_unknown" });
  } else if (info.nominal.bitrate !== 1_000_000) {
    issues.push({
      code: "nominal_bitrate",
      actual: info.nominal.bitrate,
      expected: 1_000_000,
    });
  }
  if (info.nominal?.sample_point_per_mille == null) {
    issues.push({
      code: "nominal_sample_point",
      actual: "unknown",
      expected: 800,
    });
  } else if (info.nominal.sample_point_per_mille !== 800) {
    issues.push({
      code: "nominal_sample_point",
      actual: info.nominal.sample_point_per_mille,
      expected: 800,
    });
  }

  if (!info.data || info.data.bitrate == null) {
    issues.push({ code: "data_unknown" });
  } else {
    const expectedSamplePoint = expectedDataSamplePoint(info.data.bitrate);
    if (expectedSamplePoint == null) {
      issues.push({
        code: "data_bitrate",
        actual: info.data.bitrate,
        expected: "1/2/4/5 Mbit/s",
      });
    }
    if (info.data.sample_point_per_mille == null) {
      issues.push({
        code: "data_sample_point",
        actual: "unknown",
        expected: expectedSamplePoint ?? "profile-defined",
      });
    } else if (
      expectedSamplePoint != null &&
      info.data.sample_point_per_mille !== expectedSamplePoint
    ) {
      issues.push({
        code: "data_sample_point",
        actual: info.data.sample_point_per_mille,
        expected: expectedSamplePoint,
      });
    }
  }
  return issues;
}

/** Compare cached device snapshots with the host link. BRS is informational. */
export function validateDeviceProfiles(
  info: ConnectionInfo,
  devices: MotorInfo[],
): DeviceProfileIssue[] {
  const issues: DeviceProfileIssue[] = [];
  for (const device of devices) {
    // Unknown exact identities are inventory-only. We neither guess their
    // object dictionary nor turn the absence of a registered schema into a
    // fleet-profile error.
    if (device.device_type === "unknown") continue;

    const status = device.can_config;
    if (status.status === "pending") continue;
    if (status.status === "unsupported") {
      issues.push({
        nodeId: device.node_id,
        name: device.friendly_name,
        code: "unsupported",
      });
      continue;
    }
    if (status.status === "read_failed") {
      issues.push({
        nodeId: device.node_id,
        name: device.friendly_name,
        code: "read_failed",
        reason: status.reason,
      });
      continue;
    }

    const config = status.config;
    if (config.nominal_bitrate !== 1_000_000) {
      issues.push({
        nodeId: device.node_id,
        name: device.friendly_name,
        code: "nominal_bitrate",
        actual: config.nominal_bitrate,
        expected: 1_000_000,
      });
    } else if (
      info.nominal?.bitrate != null &&
      config.nominal_bitrate !== info.nominal.bitrate
    ) {
      issues.push({
        nodeId: device.node_id,
        name: device.friendly_name,
        code: "nominal_bitrate",
        actual: config.nominal_bitrate,
        expected: info.nominal.bitrate,
      });
    }

    if (config.data_bitrate == null) {
      if (info.fd_enabled === true) {
        issues.push({
          nodeId: device.node_id,
          name: device.friendly_name,
          code: "classic_on_fd",
          actual: "Classic CAN",
          expected: info.data?.bitrate ?? "CAN-FD",
        });
      }
      continue;
    }

    if (expectedDataSamplePoint(config.data_bitrate) == null) {
      issues.push({
        nodeId: device.node_id,
        name: device.friendly_name,
        code: "data_bitrate",
        actual: config.data_bitrate,
        expected: "1/2/4/5 Mbit/s",
      });
    } else if (
      info.data?.bitrate != null &&
      config.data_bitrate !== info.data.bitrate
    ) {
      issues.push({
        nodeId: device.node_id,
        name: device.friendly_name,
        code: "data_bitrate",
        actual: config.data_bitrate,
        expected: info.data.bitrate,
      });
    } else if (info.fd_enabled === false) {
      issues.push({
        nodeId: device.node_id,
        name: device.friendly_name,
        code: "data_bitrate",
        actual: config.data_bitrate,
        expected: "Classic CAN",
      });
    }
  }
  return issues;
}

export function formatBitrate(bitrate: number | null | undefined): string {
  if (bitrate == null) return "?";
  if (bitrate % 1_000_000 === 0) return `${bitrate / 1_000_000}M`;
  if (bitrate % 1_000 === 0) return `${bitrate / 1_000}k`;
  return `${bitrate} bit/s`;
}

export function formatSamplePoint(
  samplePointPerMille: number | null | undefined,
): string {
  return samplePointPerMille == null
    ? "SP ?"
    : `SP ${(samplePointPerMille / 1000).toFixed(3)}`;
}
