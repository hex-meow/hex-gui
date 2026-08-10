export const MEOW_MOTOR_4310_PRODUCT_CODE = 0x6c64_bc78;
export const MEOW_MOTOR_4342_PRODUCT_CODE = 0x6c64_bcaa;
export const RADIANS_PER_REVOLUTION = 2 * Math.PI;

/** `hex-motor::cia402::PdoProfile` accepts integer event timers in this range. */
export const MIN_PDO_EVENT_TIMER_MS = 1;
export const MAX_PDO_EVENT_TIMER_MS = 100;

/** The CANopen event timer is authoritative; its corresponding rate may be fractional. */
export function pdoRateHz(eventTimerMs: number): number {
  if (
    !Number.isInteger(eventTimerMs) ||
    eventTimerMs < MIN_PDO_EVENT_TIMER_MS ||
    eventTimerMs > MAX_PDO_EVENT_TIMER_MS
  ) {
    throw new RangeError(
      `TPDO event timer must be an integer from ${MIN_PDO_EVENT_TIMER_MS} to ${MAX_PDO_EVENT_TIMER_MS} ms`,
    );
  }
  return 1000 / eventTimerMs;
}

export function formatPdoRateHz(eventTimerMs: number): string {
  const rate = pdoRateHz(eventTimerMs);
  return Number.isInteger(rate) ? rate.toFixed(0) : rate.toFixed(1);
}

const U16_MAX = 65_535;

export interface MeowMitSiTarget {
  position_rad: number;
  velocity_rad_per_s: number;
  torque_nm: number;
  kp_nm_per_rad: number;
  kd_nm_s_per_rad: number;
  kp_kd_limit_permille: number;
}

export interface MeowMitWireTarget {
  position_rev: number;
  velocity_rev_per_s: number;
  torque_nm: number;
  kp: number;
  kd: number;
  kp_kd_limit_permille: number;
}

type DisplayLanguage = "en" | "zh";
type LocalizedName = Record<DisplayLanguage, string>;

const MODE_DISPLAY_NAMES: Partial<Record<number, LocalizedName>> = {
  0x00: { en: "Disabled", zh: "失能" },
  0x01: { en: "Profile Position", zh: "轮廓位置" },
  0x02: { en: "Profile Velocity", zh: "轮廓速度" },
  0x03: { en: "Torque", zh: "力矩" },
  0x04: { en: "MIT", zh: "MIT" },
  0xa1: { en: "Overcurrent error", zh: "过流错误" },
  0xa2: { en: "Overvoltage error", zh: "过压错误" },
  0xa3: { en: "Undervoltage error", zh: "欠压错误" },
  0xa4: { en: "Heartbeat error", zh: "心跳检测错误" },
  0xa5: { en: "Driver overtemperature", zh: "驱动器过温" },
  0xa6: { en: "Motor overtemperature", zh: "电机过温" },
  0xaf: { en: "Other error", zh: "其他错误" },
};

const DETAILED_ERROR_NAMES: Partial<Record<number, LocalizedName>> = {
  0x0000: { en: "No error", zh: "无错误" },
  0x2310: { en: "Phase overcurrent", zh: "相线过流" },
  0x3210: { en: "Bus overvoltage", zh: "母线过压" },
  0x3220: { en: "Bus undervoltage", zh: "母线欠压" },
  0x4210: { en: "Driver overtemperature", zh: "驱动器过温" },
  0x5300: { en: "CPU error", zh: "CPU 错误" },
  0x5530: { en: "EEPROM storage error", zh: "EEPROM 存储错误" },
  0x7121: { en: "Motor stall", zh: "电机堵转" },
  0x7124: { en: "Motor overtemperature", zh: "电机过温" },
  0x7330: { en: "Motor Hall sensor error", zh: "电机霍尔传感器错误" },
  0x7340: { en: "Motor encoder error", zh: "电机编码器错误" },
  0x8130: { en: "Heartbeat error", zh: "心跳检测错误" },
  0x8140: { en: "Bus recovery error", zh: "总线恢复错误" },
  0x8312: { en: "Startup difficulty", zh: "启动困难" },
  0x8400: { en: "Velocity control error", zh: "速度控制错误" },
  0x8500: { en: "Position control error", zh: "位置控制错误" },
};

/** Model-specific PV target limit in Rev/s. Unknown products fail closed at 3 Rev/s. */
export function meowVelocityLimit(productCode: number | null | undefined): number {
  switch (productCode) {
    case MEOW_MOTOR_4310_PRODUCT_CODE:
      return 8;
    case MEOW_MOTOR_4342_PRODUCT_CODE:
    default:
      return 3;
  }
}

export function clampToRange(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/** Convert the GUI's N·m target to the signed permille object used on wire. */
export function torqueNmToPermille(torqueNm: number, peakTorqueNm: number): number {
  if (!Number.isFinite(peakTorqueNm) || peakTorqueNm <= 0) {
    throw new RangeError("peak torque must be finite and greater than zero");
  }
  if (!Number.isFinite(torqueNm)) {
    throw new RangeError("torque target must be finite");
  }
  if (torqueNm < -peakTorqueNm || torqueNm > peakTorqueNm) {
    throw new RangeError("torque target exceeds the motor peak torque");
  }
  return Math.round((torqueNm / peakTorqueNm) * 1000);
}

export function torquePermilleToNm(torquePermille: number, peakTorqueNm: number): number {
  if (!Number.isFinite(torquePermille)) {
    throw new RangeError("torque feedback must be finite");
  }
  if (!Number.isFinite(peakTorqueNm) || peakTorqueNm <= 0) {
    throw new RangeError("peak torque must be finite and greater than zero");
  }
  return (torquePermille * peakTorqueNm) / 1000;
}

export function formatMeowModeDisplay(code: number, lang: DisplayLanguage): string {
  const name = MODE_DISPLAY_NAMES[code]?.[lang] ?? (lang === "zh" ? "未知模式" : "Unknown mode");
  const number = code >= 0xa0 ? formatHex(code, 2) : String(code);
  return `${name} (${number})`;
}

export function formatMeowDetailedError(code: number, lang: DisplayLanguage): string {
  const name =
    DETAILED_ERROR_NAMES[code]?.[lang] ?? (lang === "zh" ? "未知错误" : "Unknown error");
  return `${name} (${formatHex(code, 4)})`;
}

/** Largest SI gain representable by the motor's unsigned 16-bit MIT gain field. */
export function meowMitGainLimitSi(factorNmPerRev: number): number {
  requirePositiveFactor(factorNmPerRev);
  return (U16_MAX * factorNmPerRev) / RADIANS_PER_REVOLUTION;
}

/**
 * Convert operator-facing SI MIT values to the new protocol's Rev/u16 fields.
 * The motor applies `gain_raw * factor * error_rev`, so an SI gain must first
 * be converted from per-radian to per-revolution before dividing by `factor`.
 */
export function meowMitTargetFromSi(
  target: MeowMitSiTarget,
  factorNmPerRev: number,
): MeowMitWireTarget {
  requirePositiveFactor(factorNmPerRev);
  for (const [field, value] of [
    ["position", target.position_rad],
    ["velocity", target.velocity_rad_per_s],
    ["torque", target.torque_nm],
  ] as const) {
    if (!Number.isFinite(value)) throw new RangeError(`${field} must be finite`);
  }
  if (
    !Number.isInteger(target.kp_kd_limit_permille) ||
    target.kp_kd_limit_permille < 0 ||
    target.kp_kd_limit_permille > 1000
  ) {
    throw new RangeError("Kp/Kd result limit must be an integer from 0 to 1000");
  }

  return {
    position_rev: target.position_rad / RADIANS_PER_REVOLUTION,
    velocity_rev_per_s: target.velocity_rad_per_s / RADIANS_PER_REVOLUTION,
    torque_nm: target.torque_nm,
    kp: meowMitGainToRaw(target.kp_nm_per_rad, factorNmPerRev),
    kd: meowMitGainToRaw(target.kd_nm_s_per_rad, factorNmPerRev),
    kp_kd_limit_permille: target.kp_kd_limit_permille,
  };
}

function meowMitGainToRaw(gainSi: number, factorNmPerRev: number): number {
  if (!Number.isFinite(gainSi) || gainSi < 0) {
    throw new RangeError("MIT gain must be finite and non-negative");
  }
  const raw = Math.round((gainSi * RADIANS_PER_REVOLUTION) / factorNmPerRev);
  if (raw > U16_MAX) throw new RangeError("MIT gain exceeds the motor's u16 range");
  return raw;
}

function requirePositiveFactor(factorNmPerRev: number): void {
  if (!Number.isFinite(factorNmPerRev) || factorNmPerRev <= 0) {
    throw new RangeError("MIT Kp/Kd factor must be finite and greater than zero");
  }
}

function formatHex(value: number, width: number): string {
  return `0x${value.toString(16).toUpperCase().padStart(width, "0")}`;
}
