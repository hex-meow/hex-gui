import { useState } from "react";
import { Card, InputNumber, Segmented, Space, Switch, Tooltip, Typography } from "antd";
import { QuestionCircleOutlined } from "@ant-design/icons";
import { useTelemetry } from "../useTelemetry";
import { nid2hex } from "../format";
import { useI18n } from "../i18n";
import { LifecycleTag, OnlineTag } from "../tags";
import { LivePanel } from "./LivePanel";
import { LiveChart } from "./LiveChart";
import { ControlPanel } from "./ControlPanel";
import type { MotorInfo } from "../types";

export function MotorDetail({
  info,
  connected,
  logging,
  logPath,
  onToggleLog,
}: {
  info: MotorInfo;
  connected: boolean;
  logging: boolean;
  logPath: string | null;
  onToggleLog: (on: boolean) => void;
}) {
  const { t } = useI18n();
  const [rateHz, setRateHz] = useState(50);
  const { latest, samples, chartVersion } = useTelemetry(
    info.node_id,
    connected,
    rateHz,
  );
  const [view, setView] = useState<"panel" | "chart">("panel");
  const [windowSec, setWindowSec] = useState(10);

  return (
    <Space direction="vertical" size={12} style={{ width: "100%" }}>
      <Card size="small">
        <Space style={{ justifyContent: "space-between", width: "100%" }} align="start">
          <Space direction="vertical" size={2}>
            <Space align="center">
              <Typography.Title level={4} style={{ margin: 0 }}>
                {info.friendly_name}
              </Typography.Title>
              <Typography.Text code>{nid2hex(info.node_id)}</Typography.Text>
              <OnlineTag online={info.online} />
              <LifecycleTag lc={info.lifecycle} />
            </Space>
            {info.identity && (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                vendor 0x{info.identity.vendor_id.toString(16)} · serial 0x
                {info.identity.serial_number.toString(16)}
                {info.peak_torque_nm != null
                  ? ` · ${t("peakTorque")} ${info.peak_torque_nm.toFixed(3)} Nm`
                  : ""}
              </Typography.Text>
            )}
          </Space>
          <Space size={16}>
            <Space size={4}>
              <Typography.Text type="secondary">{t("refresh")}</Typography.Text>
              <Tooltip title={t("refreshHint")}>
                <Typography.Text type="secondary">
                  <QuestionCircleOutlined />
                </Typography.Text>
              </Tooltip>
              <Segmented
                size="small"
                value={rateHz}
                onChange={(v) => setRateHz(v as number)}
                options={[
                  { label: t("refreshLow"), value: 50 },
                  { label: t("refreshHigh"), value: 100 },
                ]}
              />
            </Space>
            <Space size={4}>
              <Typography.Text type="secondary">{t("recordCsv")}</Typography.Text>
              <Switch checked={logging} onChange={onToggleLog} />
            </Space>
          </Space>
        </Space>
        {logging && logPath && (
          <Typography.Text type="secondary" style={{ fontSize: 12 }} copyable>
            {logPath}
          </Typography.Text>
        )}
      </Card>

      <Card
        size="small"
        title={t("display")}
        extra={
          <Space>
            {view === "chart" && (
              <Space size={4}>
                <Typography.Text type="secondary">{t("window")}</Typography.Text>
                <InputNumber
                  size="small"
                  min={1}
                  max={60}
                  value={windowSec}
                  onChange={(v) => setWindowSec(v ?? 10)}
                  addonAfter="s"
                  style={{ width: 90 }}
                />
              </Space>
            )}
            <Segmented
              value={view}
              onChange={(v) => setView(v as "panel" | "chart")}
              options={[
                { label: t("numeric"), value: "panel" },
                { label: t("chart"), value: "chart" },
              ]}
            />
          </Space>
        }
      >
        {view === "panel" ? (
          <LivePanel info={info} live={latest} />
        ) : (
          <LiveChart samples={samples} chartVersion={chartVersion} windowSec={windowSec} />
        )}
      </Card>

      <ControlPanel info={info} live={latest} />
    </Space>
  );
}
