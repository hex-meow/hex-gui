import assert from "node:assert/strict";
import test from "node:test";

import {
  CHALLENGE_ARM,
  CHALLENGE_CLEAR_FAULT,
  CHALLENGE_NONE,
  EPOCH_CORRUPT,
  EPOCH_EXHAUSTED,
  EPOCH_MISSING_OR_UNREADABLE,
  EPOCH_READY,
  EPOCH_WRITE_FAILED,
  NMT_OPERATIONAL,
  NMT_PRE_OPERATIONAL,
  canWriteEpochService,
  challengeKindLabel,
  epochStatusLabel,
  isLiftCommissionAbi2,
  shouldOfferEpochServiceAction,
} from "../src/liftCommissionProtocol.ts";
const safeStageAGate = {
  nmtState: NMT_PRE_OPERATIONAL,
  motorDisconnected: true,
  epochStatus: EPOCH_MISSING_OR_UNREADABLE,
  commissionState: 0,
  activeSession: 0,
  flags: 0,
  bootEpoch: 0,
};

test("commissioning controls require the exact device name and ABI2", () => {
  assert.equal(isLiftCommissionAbi2("hexmeow-lift-commission", 2, true), true);
  assert.equal(isLiftCommissionAbi2("hexmeow-lift-commission", 1, true), false);
  assert.equal(isLiftCommissionAbi2("hexmeow-lift-driver", 2, true), false);
  assert.equal(isLiftCommissionAbi2("hexmeow-lift-commission", 2, false), false);
});

test("Stage A action is offered only for missing or corrupt epoch records", () => {
  assert.equal(shouldOfferEpochServiceAction(EPOCH_READY), false);
  assert.equal(shouldOfferEpochServiceAction(EPOCH_MISSING_OR_UNREADABLE), true);
  assert.equal(shouldOfferEpochServiceAction(EPOCH_CORRUPT), true);
  assert.equal(shouldOfferEpochServiceAction(EPOCH_EXHAUSTED), false);
  assert.equal(shouldOfferEpochServiceAction(EPOCH_WRITE_FAILED), false);
});

test("Stage A write requires Pre-operational and explicit motor disconnect", () => {
  for (const epochStatus of [
    EPOCH_MISSING_OR_UNREADABLE,
    EPOCH_CORRUPT,
  ]) {
    assert.equal(
      canWriteEpochService({
        ...safeStageAGate,
        epochStatus,
      }),
      true
    );
    assert.equal(
      canWriteEpochService({
        ...safeStageAGate,
        nmtState: NMT_OPERATIONAL,
        epochStatus,
      }),
      false
    );
    assert.equal(
      canWriteEpochService({
        ...safeStageAGate,
        motorDisconnected: false,
        epochStatus,
      }),
      false
    );
  }
});

test("Stage A requires a disarmed, output-clear, pre-epoch session", () => {
  for (const unsafe of [
    { commissionState: 1 },
    { activeSession: 1 },
    { flags: 1 << 0 },
    { flags: 1 << 2 },
    { bootEpoch: 1 },
  ]) {
    assert.equal(canWriteEpochService({ ...safeStageAGate, ...unsafe }), false);
  }
});

test("terminal epoch states never expose or enable Stage A service", () => {
  for (const epochStatus of [EPOCH_EXHAUSTED, EPOCH_WRITE_FAILED]) {
    assert.equal(shouldOfferEpochServiceAction(epochStatus), false);
    assert.equal(
      canWriteEpochService({
        ...safeStageAGate,
        epochStatus,
      }),
      false
    );
  }
});

test("challenge and epoch labels cover the frozen ABI2 values", () => {
  assert.equal(challengeKindLabel(CHALLENGE_NONE), "None");
  assert.equal(challengeKindLabel(CHALLENGE_ARM), "Arm");
  assert.equal(challengeKindLabel(CHALLENGE_CLEAR_FAULT), "ClearFault");
  assert.equal(challengeKindLabel(9), "Unknown(9)");

  assert.equal(epochStatusLabel(EPOCH_READY), "Ready");
  assert.equal(
    epochStatusLabel(EPOCH_MISSING_OR_UNREADABLE),
    "MissingOrUnreadable"
  );
  assert.equal(epochStatusLabel(EPOCH_CORRUPT), "Corrupt");
  assert.equal(epochStatusLabel(EPOCH_EXHAUSTED), "Exhausted");
  assert.equal(epochStatusLabel(EPOCH_WRITE_FAILED), "WriteFailed");
  assert.equal(epochStatusLabel(9), "Unknown(9)");
});
