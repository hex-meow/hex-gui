import { Alert, Button, Card, InputNumber, Space, Typography } from "antd";
import { nid2hex } from "../format";
import { useI18n } from "../i18n";
import { SettingsField as Field } from "./SettingsField";

// One view for both motor types; protocol routing stays in the backend.
export function PositionPresetCard({
  nodeId,
  currentPosition,
  presetPosition,
  blocker,
  disabled,
  loading,
  onRead,
  onSave,
  onPresetChange,
}: {
  nodeId: number;
  currentPosition: number | undefined;
  presetPosition: number;
  blocker: string | null;
  disabled: boolean;
  loading: boolean;
  onRead: () => void;
  onSave: () => void;
  onPresetChange: (value: number) => void;
}) {
  const { t } = useI18n();
  return (
    <Card size="small" title={t("settingsZeroTitle")}>
      <Typography.Paragraph type="secondary">
        {t("settingsZeroHint")}
      </Typography.Paragraph>
      {blocker && (
        <Alert
          type="warning"
          showIcon
          message={blocker}
          style={{ marginBottom: 12 }}
        />
      )}
      <Space align="end" wrap size={12}>
        <Field label={t("currentId")}>
          <Typography.Text code>{nid2hex(nodeId)}</Typography.Text>
        </Field>
        <Button
          disabled={disabled}
          loading={loading}
          onClick={onRead}
        >
          {t("readPos")}
        </Button>
        <Typography.Text>
          {t("currentPos")}:{" "}
          <b>
            {currentPosition == null
              ? "—"
              : `${currentPosition.toFixed(4)} rev`}
          </b>
        </Typography.Text>
      </Space>
      <Space align="end" wrap size={12} style={{ marginTop: 12 }}>
        <Field label={t("presetPos")}>
          <InputNumber
            min={-0.5}
            max={0.5}
            step={0.01}
            value={presetPosition}
            disabled={disabled}
            onChange={(value) => onPresetChange(value ?? 0)}
            style={{ width: 150 }}
          />
        </Field>
        <Button
          type="primary"
          disabled={disabled}
          loading={loading}
          onClick={onSave}
        >
          {t("savePos")}
        </Button>
      </Space>
    </Card>
  );
}
