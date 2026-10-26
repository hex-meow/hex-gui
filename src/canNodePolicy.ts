export const isReservedCanNode = (node: number) => node === 100 || node === 101;
export const reservedCanNodeMessage = (lang: "zh" | "en", node: number) => lang === "zh"
  ? `Node-ID ${node}（0x${node.toString(16).toUpperCase()}）为保留地址：电机 IAP 与 LSS 的 0x7E4/0x7E5 冲突。请在 Device Settings 改为其他 ID，再进行控制或升级。`
  : `Node-ID ${node} (0x${node.toString(16).toUpperCase()}) is reserved: Motor IAP collides with LSS 0x7E4/0x7E5. Change it in Device Settings before control or update.`;
