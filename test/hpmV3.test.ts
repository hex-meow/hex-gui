import assert from "node:assert/strict";
import test from "node:test";
import { hpmCanCaptureOptions } from "../src/hpmV3.ts";
const fields = { spec: " can0 ", unconfigured: false, node: "31", assignedNode: "32", vendor: "0x6865786D", product: "0x45564B33", uid: "" };
test("configured HPM capture targets one node without requesting LSS writes", () => {
  const o = hpmCanCaptureOptions(fields);
  assert.equal(o.nodeId, 31); assert.equal(o.assignNodeId, null); assert.equal(o.spec, "can0");
});
test("unconfigured HPM capture requires an explicit usable address and known LSS identity", () => {
  const o = hpmCanCaptureOptions({ ...fields, unconfigured: true });
  assert.equal(o.nodeId, null); assert.equal(o.assignNodeId, 32); assert.equal(o.vendor, 0x6865786D); assert.equal(o.product, 0x45564B33);
  for (const id of ["0", "128", "100", "101", "1.5", ""]) assert.throws(() => hpmCanCaptureOptions({ ...fields, unconfigured: true, assignedNode: id }));
  for (const vendor of ["", "-1", "0x100000000", "garbage"]) assert.throws(() => hpmCanCaptureOptions({ ...fields, unconfigured: true, vendor }));
});
test("capture rejects invalid interface, address, and truncated target UID", () => {
  assert.throws(() => hpmCanCaptureOptions({ ...fields, spec: " " }));
  assert.throws(() => hpmCanCaptureOptions({ ...fields, node: "0" }));
  assert.throws(() => hpmCanCaptureOptions({ ...fields, uid: "0102" }));
  assert.equal(hpmCanCaptureOptions({ ...fields, uid: "01".repeat(16) }).uid, "01".repeat(16));
});
