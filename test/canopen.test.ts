import assert from "node:assert/strict";
import test from "node:test";
import { decodeCanopen } from "../src/canopen.ts";

test("SDO initiate commands retain their object address in both directions", () => {
  for (const [id, command, op] of [
    [0x601, 0x40, "read-req"],
    [0x581, 0x4f, "read"],
    [0x581, 0x41, "read"],
    [0x601, 0x2f, "write"],
    [0x601, 0x21, "write"],
    [0x581, 0x60, "write-ack"],
  ] as const) {
    const decoded = decodeCanopen(id, false, [command, 0x18, 0x10, 0x01, 4, 0, 0, 0]);
    assert.equal(decoded.detail, `${op} 0x1018:0x01`);
    assert.equal(decoded.node, 1);
  }
});

test("SDO upload and download segments show flags without treating payload as an index", () => {
  const hello = [0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x2c, 0x20];
  assert.equal(decodeCanopen(0x581, false, [0x00, ...hello]).detail, "upload-seg toggle=0");
  assert.equal(decodeCanopen(0x601, false, [0x00, ...hello]).detail, "write-seg toggle=0");
  const last = [0x17, 0x77, 0x6f, 0x72, 0x6c, 0, 0, 0];
  assert.equal(decodeCanopen(0x581, false, last).detail, "upload-seg toggle=1 last");
  assert.equal(decodeCanopen(0x601, false, last).detail, "write-seg toggle=1 last");
});

test("SDO segment requests and acknowledgments are decoded by direction", () => {
  for (const toggle of [0, 1]) {
    assert.equal(
      decodeCanopen(0x601, false, [0x60 | (toggle << 4), 0, 0, 0, 0, 0, 0, 0]).detail,
      `upload-seg-req toggle=${toggle}`,
    );
    assert.equal(
      decodeCanopen(0x581, false, [0x20 | (toggle << 4), 0, 0, 0, 0, 0, 0, 0]).detail,
      `write-seg-ack toggle=${toggle}`,
    );
  }
});

test("SDO aborts retain their abort code and object address", () => {
  for (const id of [0x601, 0x581]) {
    assert.equal(
      decodeCanopen(id, false, [0x80, 0x18, 0x10, 0x01, 0, 0, 2, 6]).detail,
      "ABORT 0x1018:0x01 = 0x06020000",
    );
  }
});

test("unsupported SDO commands and truncated initiates do not invent an object address", () => {
  assert.equal(decodeCanopen(0x581, false, [0xa0, 0x68, 0x65, 0x6c]).detail, undefined);
  assert.equal(decodeCanopen(0x601, false, [0xc0, 0x68, 0x65, 0x6c]).detail, undefined);
  assert.equal(decodeCanopen(0x601, false, [0x40, 0x18, 0x10]).detail, undefined);
  assert.equal(decodeCanopen(0x581, false, []).detail, undefined);
});

test("NMT, heartbeat and extended identifiers retain their existing classification", () => {
  assert.equal(decodeCanopen(0, false, [1, 0]).detail, "Start → all");
  assert.equal(decodeCanopen(0x701, false, [0x7f]).detail, "Pre-op");
  assert.deepEqual(decodeCanopen(0x581, true, [0x00, 1, 2, 3]), { kind: "RAW_EXT", label: "0x581 (ext)" });
});
