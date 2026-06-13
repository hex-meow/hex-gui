import { useEffect, useState } from "react";
import { Alert, App, Button, Card, InputNumber, Space, Typography } from "antd";
import { api, errMsg } from "../api";
import { nid2hex } from "../format";
import { useI18n } from "../i18n";
import type { MotorInfo } from "../types";

export function ChangeIdTool({
  devices,
  selectedNid,
  connected,
}: {
  devices: MotorInfo[];
  selectedNid: number | null;
  connected: boolean;
}) {
  const { message } = App.useApp();
  const { t } = useI18n();
  const [currentId, setCurrentId] = useState<number | null>(selectedNid);
  const [newId, setNewId] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);

  // Follow the sidebar selection into the "current ID" field.
  useEffect(() => {
    if (selectedNid != null) setCurrentId(selectedNid);
  }, [selectedNid]);

  const cur = currentId != null ? devices.find((d) => d.node_id === currentId) : undefined;

  const change = async () => {
    if (currentId == null || newId == null) return;
    if (newId === currentId) {
      message.error(t("sameIdError"));
      return;
    }
    setBusy(true);
    try {
      await api.changeNodeId(currentId, newId);
      message.success(`${t("changeIdOk")} ${nid2hex(currentId)} → ${nid2hex(newId)}`);
    } catch (e) {
      message.error(`${t("changeIdFailed")}: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Space direction="vertical" size={12} style={{ width: "100%", maxWidth: 560 }}>
      <Card size="small" title={t("changeIdTitle")}>
        {!connected && <Alert type="info" showIcon message={t("connectFirst")} style={{ marginBottom: 12 }} />}

        <Typography.Paragraph type="secondary" style={{ marginBottom: 12 }}>
          {t("changeIdPick")}
        </Typography.Paragraph>

        <Space align="end" wrap>
          <Field label={t("currentId")}>
            <InputNumber
              min={1}
              max={127}
              value={currentId}
              onChange={(v) => setCurrentId(v)}
              style={{ width: 110 }}
            />
          </Field>
          <span style={{ paddingBottom: 6 }}>→</span>
          <Field label={t("newId")}>
            <InputNumber
              min={1}
              max={127}
              value={newId}
              onChange={(v) => setNewId(v)}
              style={{ width: 110 }}
            />
          </Field>
          <Button
            type="primary"
            loading={busy}
            disabled={!connected || currentId == null || newId == null}
            onClick={change}
          >
            {t("changeIdBtn")}
          </Button>
        </Space>

        {cur?.identity && (
          <Typography.Paragraph type="secondary" style={{ fontSize: 12, marginTop: 8, marginBottom: 0 }}>
            {cur.friendly_name} · serial 0x{cur.identity.serial_number.toString(16)}
          </Typography.Paragraph>
        )}

        <Alert
          type="warning"
          showIcon
          style={{ marginTop: 12 }}
          message={t("changeIdInstr")}
        />
      </Card>
    </Space>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div style={{ fontSize: 12, color: "#8a93a3", marginBottom: 2 }}>{label}</div>
      {children}
    </div>
  );
}
