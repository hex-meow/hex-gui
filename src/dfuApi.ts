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

export type CanDfuAuthorization =
  | "enabled"
  | "known_disabled"
  | "unsupported";

export interface CanDfuDevice {
  node_id: number;
  node_id_hex: string;
  device_name: string | null;
  vendor_id: number;
  vendor_id_hex: string;
  product_code: number;
  product_code_hex: string;
  software_revision: number;
  software_revision_hex: string;
  serial_number: number;
  serial_number_hex: string;
  hardware_version: number | null;
  hardware_version_hex: string | null;
  authorization: CanDfuAuthorization;
  backend: "stm32_canopen" | "cobs_can_iap_v1" | null;
  profile_id: string | null;
  display_name: string | null;
  reason: string;
}

export interface CanDfuDiscoveryIssue {
  node_id: number;
  node_id_hex: string;
  reason: string;
}

export interface CanDfuDiscovery {
  devices: CanDfuDevice[];
  issues: CanDfuDiscoveryIssue[];
}

export interface CanDfuPrepared {
  token: string;
  device: CanDfuDevice;
  backend: "stm32_canopen" | "cobs_can_iap_v1";
  artifact_kind: "meowpkg" | "compatible_img";
  artifact_sha256: string;
  artifact_size: number;
  mcu: "stm32g431" | "stm32g474" | "stm32g0b1" | null;
  format_version: number | null;
  encrypted: boolean;
  img_device_id: number | null;
  img_device_id_hex: string | null;
  firmware_id: number;
  firmware_id_hex: string;
  firmware_version: number;
  firmware_version_hex: string;
  plaintext_size: number | null;
  wire_size: number;
  img_start_address_hex: string | null;
  img_end_address_hex: string | null;
  // Proven downgrade targets are rejected by Rust before staging. "unknown"
  // is reserved for a local Motor IMG whose header version cannot prove the
  // post-flash CANopen revision.
  version_warning: "unknown" | "none" | "reinstall";
  artifact_source: "local" | "r2";
  release_version: string | null;
  expected_postflash_revision: number | null;
  expected_postflash_revision_hex: string | null;
  // Local partner IMG files require an explicit, token-bound acknowledgement
  // after their exact metadata has been shown to the operator.
  manual_risk_required: boolean;
  manual_risk_acknowledged: boolean;
  // Meow Motor profiles require a verified 0x4001 factory-calibration backup
  // before the first firmware mutation.
  factory_backup_required: boolean;
}

export type CanDfuStage =
  | "revalidating"
  | "backing_up_factory_data"
  | "entering_bootloader"
  | "writing_header"
  | "clearing"
  | "writing"
  | "verifying_and_starting"
  | "confirming_application"
  | "resetting"
  | "entering_compatible_bootloader"
  | "validating_compatible_identity"
  | "starting_download"
  | "finalizing"
  | "verifying"
  | "checking_factory_data";

export interface CanDfuProgress {
  stage: CanDfuStage;
  completed: number;
  total: number;
  cancellable: boolean;
}

export type CanDfuOutcomeStatus =
  | "application_verified"
  | "verify_acked_startup_unconfirmed"
  | "verify_acked_factory_data_preserved"
  | "verify_acked_factory_data_recovery_required"
  | "cancelled_before_write"
  | "cancelled_recoverable";

export interface CanDfuOutcome {
  status: CanDfuOutcomeStatus;
  startup_confirmed: boolean;
  recoverable_bootloader_expected: boolean;
  factory_backup_path: string | null;
  factory_backup_sha256: string | null;
  factory_data_status:
    | null
    | "preserved"
    | "recovery_required"
    | "startup_unconfirmed";
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

export const canDfuApi = {
  discover: (spec: string) =>
    invoke<CanDfuDiscovery>("stm32_can_dfu_discover", { spec }),

  select: (nodeId: number) =>
    invoke<void>("stm32_can_dfu_select", { nodeId }),

  // As with HPM, firmware bytes use Tauri's raw IPC body. The selected,
  // authorized identity is held in the Rust session and cannot be supplied by
  // WebView JSON.
  prepare: (bytes: Uint8Array) =>
    invoke<CanDfuPrepared>("stm32_can_dfu_prepare", bytes),

  // The Rust session owns the selected identity and constructs the one fixed
  // HTTPS R2 path. The WebView cannot supply an identity or arbitrary URL.
  prepareLatest: () =>
    invoke<CanDfuPrepared>("stm32_can_dfu_prepare_latest"),

  acknowledgeManual: (token: string) =>
    invoke<CanDfuPrepared>("stm32_can_dfu_acknowledge_manual", { token }),

  start: (
    token: string,
    onProgress: (progress: CanDfuProgress) => void
  ) => {
    const onEvent = new Channel<CanDfuProgress>(onProgress);
    return invoke<CanDfuOutcome>("stm32_can_dfu_start", { token, onEvent });
  },

  cancel: () => invoke<boolean>("stm32_can_dfu_cancel"),
  leave: () => invoke<void>("stm32_can_dfu_leave"),
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
