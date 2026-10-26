import assert from "node:assert/strict";
import test from "node:test";
import { isReservedCanNode, reservedCanNodeMessage } from "../src/canNodePolicy.ts";
import { hpmCanCaptureOptions } from "../src/hpmV3.ts";
test("only 100/101 collide with the LSS request/response IDs in the IAP transmit range", () => {
  for (let node = 1; node <= 127; node++) {
    assert.equal(isReservedCanNode(node), [0x7e4, 0x7e5].includes(0x780 + node));
  }
  for (const node of [100, 101]) for (const lang of ["zh", "en"] as const) assert.match(reservedCanNodeMessage(lang, node), /0x7E4\/0x7E5/);
});
test("HPM refuses the reserved IDs both as current addresses and as LSS assignments", () => {
  for (const node of [100, 101]) for (const unconfigured of [false, true]) {
    assert.throws(() => hpmCanCaptureOptions({spec:"can0",unconfigured,node:String(node),assignedNode:String(node),vendor:"0x6865786d",product:"0x45564b33",uid:""}));
  }
});
