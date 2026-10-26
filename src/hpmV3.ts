export interface HpmCanCaptureFields {
  spec: string;
  unconfigured: boolean;
  node: string;
  assignedNode: string;
  vendor: string;
  product: string;
  uid: string;
}

// UI validation only. Rust independently checks every field and live identity.
export function hpmCanCaptureOptions(fields: HpmCanCaptureFields) {
  const spec = fields.spec.trim();
  if (!spec) throw new Error("CAN interface is required");
  const node = (value: string, assign: boolean) => {
    if (!/^\d+$/.test(value.trim())) throw new Error("Node-ID must be 1..127");
    const n = Number(value);
    if (n < 1 || n > 127 || (assign && [100, 101].includes(n))) {
      throw new Error("Choose a free Node-ID 1..127; 100/101 are reserved for assignment");
    }
    return n;
  };
  const word = (value: string) => {
    const s = value.trim();
    if (!/^(?:0x[\da-f]+|\d+)$/i.test(s)) throw new Error("LSS vendor/product must be u32");
    const n = Number(s);
    if (!Number.isSafeInteger(n) || n < 0 || n > 0xffffffff) throw new Error("LSS vendor/product must be u32");
    return n;
  };
  const uid = fields.uid.trim();
  if (uid && !/^[\da-f]{32}$/i.test(uid)) throw new Error("UID must contain exactly 16 bytes of hex");
  return {
    spec,
    nodeId: fields.unconfigured ? null : node(fields.node, false),
    assignNodeId: fields.unconfigured ? node(fields.assignedNode, true) : null,
    vendor: fields.unconfigured ? word(fields.vendor) : 0,
    product: fields.unconfigured ? word(fields.product) : 0,
    uid: uid || null,
  };
}
