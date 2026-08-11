import assert from "node:assert/strict";
import test from "node:test";

import {
  MEOW_MOTOR_4310_PRODUCT_CODE,
  MEOW_MOTOR_4342_PRODUCT_CODE,
  RADIANS_PER_REVOLUTION,
  formatMeowDetailedError,
  formatMeowModeDisplay,
  formatPdoRateHz,
  meowMitGainLimitSi,
  meowMitTargetFromSi,
  meowVelocityLimit,
  pdoRateHz,
  torquePermilleToNm,
  torqueNmToPermille,
} from "../src/meowMotorUi.ts";

test("PV limits follow the exact Meow Motor product", () => {
  assert.equal(meowVelocityLimit(MEOW_MOTOR_4310_PRODUCT_CODE), 8);
  assert.equal(meowVelocityLimit(MEOW_MOTOR_4342_PRODUCT_CODE), 3);
  assert.equal(meowVelocityLimit(null), 3);
});

test("PDO profiles expose the exact rate represented by an integer millisecond timer", () => {
  assert.equal(pdoRateHz(1), 1000);
  assert.equal(pdoRateHz(2), 500);
  assert.equal(pdoRateHz(4), 250);
  assert.equal(formatPdoRateHz(3), "333.3");
  assert.throws(() => pdoRateHz(0), RangeError);
  assert.throws(() => pdoRateHz(101), RangeError);
  assert.throws(() => pdoRateHz(1.5), RangeError);
});

test("N·m torque targets convert to signed permille and reject values beyond peak", () => {
  assert.equal(torqueNmToPermille(0, 8), 0);
  assert.equal(torqueNmToPermille(4, 8), 500);
  assert.equal(torqueNmToPermille(-4, 8), -500);
  assert.equal(torqueNmToPermille(8, 8), 1000);
  assert.equal(torqueNmToPermille(-8, 8), -1000);
  assert.throws(() => torqueNmToPermille(9, 8), RangeError);
  assert.throws(() => torqueNmToPermille(-9, 8), RangeError);
  assert.throws(() => torqueNmToPermille(1, 0), RangeError);
});

test("the reachable torque range follows the factory factor, not peak torque", () => {
  // The backend multiplies the permille by the factory torque factor, so the
  // physical range the operator may ask for is peak ÷ factor. The permille
  // denominator stays peak torque either way.
  const peak = 8;

  // factor 1.12: the raw domain saturates before the operator reaches peak.
  const shrunk = peak / 1.12;
  assert.equal(torqueNmToPermille(shrunk, peak, shrunk), 893);
  assert.throws(() => torqueNmToPermille(peak, peak, shrunk), RangeError);

  // factor 0.85: physical torque above peak is reachable and must be allowed.
  const widened = peak / 0.85;
  assert.equal(torqueNmToPermille(9, peak, widened), 1125);
  assert.throws(() => torqueNmToPermille(widened + 0.1, peak, widened), RangeError);

  assert.throws(() => torqueNmToPermille(1, peak, 0), RangeError);
  assert.throws(() => torqueNmToPermille(1, peak, Number.NaN), RangeError);
});

test("MIT SI targets convert to the protocol's Rev and raw gain fields", () => {
  const target = meowMitTargetFromSi(
    {
      position_rad: RADIANS_PER_REVOLUTION,
      velocity_rad_per_s: 2 * RADIANS_PER_REVOLUTION,
      torque_nm: 1.5,
      kp_nm_per_rad: 1,
      kd_nm_s_per_rad: 0.1,
      kp_kd_limit_permille: 800,
    },
    0.01,
  );

  assert.deepEqual(target, {
    position_rev: 1,
    velocity_rev_per_s: 2,
    torque_nm: 1.5,
    kp: 628,
    kd: 63,
    kp_kd_limit_permille: 800,
  });
});

test("MIT SI conversion rejects an invalid factor or unrepresentable gain", () => {
  const base = {
    position_rad: 0,
    velocity_rad_per_s: 0,
    torque_nm: 0,
    kp_nm_per_rad: 0,
    kd_nm_s_per_rad: 0,
    kp_kd_limit_permille: 1000,
  };
  assert.throws(() => meowMitTargetFromSi(base, 0), RangeError);
  assert.throws(
    () =>
      meowMitTargetFromSi(
        { ...base, kp_nm_per_rad: meowMitGainLimitSi(0.01) + 0.01 },
        0.01,
      ),
    RangeError,
  );
});

test("telemetry helpers convert torque and name mode/error codes", () => {
  assert.equal(torquePermilleToNm(-375, 8), -3);
  assert.equal(formatMeowModeDisplay(2, "en"), "Profile Velocity (2)");
  assert.equal(formatMeowModeDisplay(0xa4, "zh"), "心跳检测错误 (0xA4)");
  assert.equal(formatMeowDetailedError(0x8130, "en"), "Heartbeat error (0x8130)");
  assert.equal(formatMeowDetailedError(0xbeef, "zh"), "未知错误 (0xBEEF)");
});
