import { Channel, invoke } from "@tauri-apps/api/core";

export interface HpmDfuDevice {
  uid: string;
  chip_family_id: number;
  chip_family_id_hex: string;
  product_code: number;
  product_code_hex: string;
  product_code_ascii: string;
  hardware_version: number;
  hardware_version_hex: string;
  hardware_version_valid: boolean;
  bootloader_version: string;
  app0_address_hex: string;
  app0_max_size: number;
  sector_size: number;
  page_size: number;
  key_fingerprint_hex: string;
  pubkey_fingerprint_hex: string;
  security_mode: "development" | "production_confidential";
  otp_app_arv_floor: number;
  otp_app_arv_floor_state:
    | "informational_not_enforced"
    | "corrupt_informational";
}

export interface HpmDfuPrepared {
  token: string;
  device: HpmDfuDevice;
  artifact_kind: "development_raw" | "legacy_hpmota_v2";
  source_sha256: string;
  wire_image_sha256: string;
  source_size: number;
  wire_image_size: number;
  erase_size: number;
  app_arv: number | null;
  app_arv_state: "metadata_only_not_enforced" | "not_present";
  pack_tool_version: string | null;
}

export type HpmDfuStage =
  | "connecting"
  | "revalidating"
  | "erasing"
  | "writing"
  | "verifying_crc32"
  | "writing_kn_data"
  | "requesting_jump";

export interface HpmDfuProgress {
  stage: HpmDfuStage;
  completed: number;
  total: number;
  cancellable: boolean;
}

export type HpmDfuOutcomeStatus =
  | "jump_acked_startup_unconfirmed"
  | "jump_outcome_unknown"
  | "cancelled_before_erase"
  | "cancelled_recoverable";

export interface HpmDfuOutcome {
  status: HpmDfuOutcomeStatus;
  startup_confirmed: false;
  recoverable_bootloader_expected: boolean;
}

export const hpmDfuApi = {
  probe: () => invoke<HpmDfuDevice>("hpm_dfu_probe"),

  // A Uint8Array becomes Tauri's raw InvokeBody. It is not expanded into a
  // large JSON number array and does not disclose a host filesystem path.
  prepare: (bytes: Uint8Array) =>
    invoke<HpmDfuPrepared>("hpm_dfu_prepare", bytes),

  start: (
    token: string,
    onProgress: (progress: HpmDfuProgress) => void
  ) => {
    const onEvent = new Channel<HpmDfuProgress>(onProgress);
    return invoke<HpmDfuOutcome>("hpm_dfu_start", { token, onEvent });
  },

  cancel: () => invoke<boolean>("hpm_dfu_cancel"),
  leave: () => invoke<void>("hpm_dfu_leave"),
};

export function dfuError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}
