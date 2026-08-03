import assert from "node:assert/strict";
import test from "node:test";

import {
  MEOW_MOTOR_4310_PRODUCT_CODE,
  MEOW_MOTOR_4342_PRODUCT_CODE,
  RADIANS_PER_REVOLUTION,
  formatMeowDetailedError,
  formatMeowModeDisplay,
  meowMitGainLimitSi,
  meowMitTargetFromSi,
  meowVelocityLimit,
  torquePermilleToNm,
  torqueNmToPermille,
} from "../src/meowMotorUi.ts";

test("PV limits follow the exact Meow Motor product", () => {
  assert.equal(meowVelocityLimit(MEOW_MOTOR_4310_PRODUCT_CODE), 8);
  assert.equal(meowVelocityLimit(MEOW_MOTOR_4342_PRODUCT_CODE), 3);
  assert.equal(meowVelocityLimit(null), 3);
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
