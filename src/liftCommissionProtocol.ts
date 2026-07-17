export const LIFT_COMMISSION_DEVICE_NAME = "hexmeow-lift-commission";
export const LIFT_COMMISSION_ABI = 2;

export function isLiftCommissionAbi2(
  deviceName: string,
  abi: number,
  available: boolean
): boolean {
  return (
    available &&
    deviceName === LIFT_COMMISSION_DEVICE_NAME &&
    abi === LIFT_COMMISSION_ABI
  );
}

export const NMT_OPERATIONAL = 0x05;
export const NMT_PRE_OPERATIONAL = 0x7f;

export const CHALLENGE_NONE = 0;
export const CHALLENGE_ARM = 1;
export const CHALLENGE_CLEAR_FAULT = 2;

export const EPOCH_READY = 0;
export const EPOCH_MISSING_OR_UNREADABLE = 1;
export const EPOCH_CORRUPT = 2;
export const EPOCH_EXHAUSTED = 3;
export const EPOCH_WRITE_FAILED = 4;

export function challengeKindLabel(kind: number): string {
  switch (kind) {
    case CHALLENGE_NONE:
      return "None";
    case CHALLENGE_ARM:
      return "Arm";
    case CHALLENGE_CLEAR_FAULT:
      return "ClearFault";
    default:
      return `Unknown(${kind})`;
  }
}

export function epochStatusLabel(status: number): string {
  switch (status) {
    case EPOCH_READY:
      return "Ready";
    case EPOCH_MISSING_OR_UNREADABLE:
      return "MissingOrUnreadable";
    case EPOCH_CORRUPT:
      return "Corrupt";
    case EPOCH_EXHAUSTED:
      return "Exhausted";
    case EPOCH_WRITE_FAILED:
      return "WriteFailed";
    default:
      return `Unknown(${status})`;
  }
}

/** Stage A is intentionally offered only for recoverable continuity loss. */
export function shouldOfferEpochServiceAction(epochStatus: number): boolean {
  return (
    epochStatus === EPOCH_MISSING_OR_UNREADABLE ||
    epochStatus === EPOCH_CORRUPT
  );
}

export interface EpochServiceGate {
  nmtState: number;
  motorDisconnected: boolean;
  epochStatus: number;
  commissionState: number;
  activeSession: number;
  flags: number;
  bootEpoch: number;
}

/**
 * Host-side mirror of the Stage A service interlock. The backend and firmware
 * independently enforce the same conditions before writing EPOCH_SERVICE.
 */
export function canWriteEpochService(gate: EpochServiceGate): boolean {
  return (
    gate.nmtState === NMT_PRE_OPERATIONAL &&
    gate.motorDisconnected &&
    gate.commissionState === 0 &&
    gate.activeSession === 0 &&
    (gate.flags & ((1 << 0) | (1 << 2))) === 0 &&
    gate.bootEpoch === 0 &&
    shouldOfferEpochServiceAction(gate.epochStatus)
  );
}
