import { Tag } from "antd";
import type { Lifecycle, Logic, NmtState } from "./types";
import { fmtHex } from "./format";

export function LifecycleTag({ lc }: { lc: Lifecycle }) {
  switch (lc.kind) {
    case "Initialized":
      return <Tag color="green">Initialized</Tag>;
    case "Initializing":
      return <Tag color="gold">Initializing</Tag>;
    case "Identified":
      return <Tag color="blue">Identified</Tag>;
    case "Unknown":
      return <Tag>Unknown</Tag>;
    case "NeedsReinit":
      return <Tag color="red">NeedsReinit ({lc.reason})</Tag>;
  }
}

export function LogicTag({ logic }: { logic: Logic | null }) {
  if (!logic) return <Tag>—</Tag>;
  switch (logic.state) {
    case "Enabled":
      return <Tag color="green">Enabled({logic.mode})</Tag>;
    case "Disabled":
      return <Tag color="default">Disabled</Tag>;
    case "Error":
      return (
        <Tag color="red">
          Error({logic.kind} @ {fmtHex(logic.raw_code, 4)})
        </Tag>
      );
  }
}

export function OnlineTag({ online }: { online: boolean }) {
  return online ? <Tag color="green">online</Tag> : <Tag color="red">offline</Tag>;
}

export function NmtTag({ nmt }: { nmt: NmtState | null }) {
  if (!nmt) return <Tag>—</Tag>;
  const color =
    nmt === "Operational" ? "green" : nmt === "Stopped" ? "red" : "default";
  return <Tag color={color}>{nmt}</Tag>;
}
