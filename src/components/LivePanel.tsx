import { Descriptions, Space, Tag } from "antd";
import { fmtHex } from "../format";
import { useI18n } from "../i18n";
import { LogicTag, NmtTag, OnlineTag } from "../tags";
import type { LiveState, MotorInfo } from "../types";

// Measurement number with a reserved sign slot and tabular (equal-width)
// figures, so the cell width never changes when the value flips sign or its
// digits change — otherwise a near-zero velocity jittering between + and −
// makes the whole Descriptions table reflow on every poll.
function Num({
  value,
  digits = 4,
}: {
  value: number | null | undefined;
  digits?: number;
}) {
  if (value == null || Number.isNaN(value)) return <span>—</span>;
  // Round first so sub-resolution noise shows "0.0000", never "-0.0000".
  const rounded = Number(value.toFixed(digits));
  const sign = rounded < 0 ? "-" : " "; // figure space = one digit wide
  return (
    <span style={{ fontVariantNumeric: "tabular-nums" }}>
      {sign}
      {Math.abs(rounded).toFixed(digits)}
    </span>
  );
}

// CiA402 status word bits we surface as chips.
const SW_BITS: [number, string][] = [
  [0x0001, "RTSO"],
  [0x0002, "SO"],
  [0x0004, "OpEn"],
  [0x0008, "Fault"],
  [0x0020, "QStop"],
  [0x0040, "SOD"],
  [0x0400, "TgtReached"],
];

function StatusWord({ sw }: { sw: number | null }) {
  if (sw == null) return <span>—</span>;
  return (
    <Space size={4} wrap>
      <Tag>{fmtHex(sw, 4)}</Tag>
      {SW_BITS.filter(([m]) => (sw & m) !== 0).map(([, name]) => (
        <Tag key={name} color={name === "Fault" ? "red" : "blue"}>
          {name}
        </Tag>
      ))}
    </Space>
  );
}

export function LivePanel({
  info,
  live,
}: {
  info: MotorInfo;
  live: LiveState | null;
}) {
  const { t } = useI18n();
  const m = live?.measurements;
  return (
    <Descriptions
      bordered
      size="small"
      column={2}
      items={[
        {
          key: "online",
          label: t("online"),
          children: (
            <Space>
              <OnlineTag online={info.online} />
              <NmtTag nmt={live?.connection.nmt_state ?? info.nmt_state} />
            </Space>
          ),
        },
        {
          key: "logic",
          label: t("logic"),
          children: <LogicTag logic={live?.logic ?? info.logic} />,
        },
        { key: "pos", label: t("position"), children: <Num value={m?.position_rev} /> },
        { key: "vel", label: t("velocity"), children: <Num value={m?.velocity_rev_per_s} /> },
        { key: "tor", label: t("torque"), children: <Num value={m?.torque_nm} /> },
        {
          key: "ts",
          label: t("motorTs"),
          children: (
            <span style={{ fontVariantNumeric: "tabular-nums" }}>
              {m?.timestamp_us ?? "—"}
            </span>
          ),
        },
        {
          key: "sw",
          label: t("statusWord"),
          span: 2,
          children: <StatusWord sw={m?.status_word ?? null} />,
        },
        { key: "mode", label: t("modeDisplay"), children: m?.mode_display ?? "—" },
        { key: "err", label: t("errorReg"), children: fmtHex(m?.error_register ?? null, 2) },
        { key: "tdrv", label: t("driverTemp"), children: <Num value={m?.driver_temp_c} digits={1} /> },
        { key: "tmot", label: t("motorTemp"), children: <Num value={m?.motor_temp_c} digits={1} /> },
      ]}
    />
  );
}
