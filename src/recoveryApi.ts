import { Channel, invoke } from "@tauri-apps/api/core";
export interface RecoveryProfile { id: string; family: "hpm" | "stm32" | "motor_iap"; name: string; description: string; image: string; available: boolean; reason: string | null; vendor: number; product: number }
export interface RecoverySelection { profile: RecoveryProfile; token: string }
export interface RecoveryHit { node_id: number; device_id: number | null; firmware_id: number | null; version: number | null; selectable: boolean; reason: string }
export interface RecoveryPrepared { token: string; node_id: number; model: string; sha256: string; bytes: number; wire_bytes: number; device_id: number; firmware_id: number; firmware_version: number }
export interface RecoveryProgress { stage: string; completed: number; total: number; cancellable: boolean }
export const recoveryApi = {
  profiles: () => invoke<RecoveryProfile[]>("can_recovery_profiles"),
  begin: (profileId: string) => invoke<RecoverySelection>("can_recovery_begin", { profileId }),
  scan: (spec: string) => invoke<RecoveryHit[]>("can_recovery_scan", { spec }),
  select: (nodeId: number) => invoke<void>("can_recovery_select", { nodeId }),
  prepare: (bytes: Uint8Array) => invoke<RecoveryPrepared>("can_recovery_prepare", bytes),
  start: (token: string, progress: (p: RecoveryProgress) => void) => invoke<string>("can_recovery_start", { token, onEvent: new Channel<RecoveryProgress>(progress) }),
  cancel: () => invoke<boolean>("can_recovery_cancel"),
  leave: (token?: string) => invoke<void>("can_recovery_leave", { token }),
};
