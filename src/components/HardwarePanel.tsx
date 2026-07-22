import { Alert, Card, Descriptions, Empty, Space, Tag, Typography } from "antd";
import type { HardwareController, HardwareField, HardwareResource } from "../types";
import { useI18n } from "../i18n";

function ageText(age: number | null): string {
  if (age == null) return "—";
  if (age < 1000) return `${age} ms`;
  return `${(age / 1000).toFixed(age < 10_000 ? 2 : 1)} s`;
}

function fieldValue(resource: HardwareResource, item: HardwareField) {
  if (resource.kind === "estop" && item.name === "engaged") {
    const color = item.value === "true" ? "red" : item.value === "false" ? "green" : "orange";
    return <Tag color={color}>{item.value}</Tag>;
  }
  if (resource.kind === "power" && item.name === "output_state") {
    const color = item.value.startsWith("OUTPUT")
      ? "green"
      : item.value.startsWith("FAULT")
        ? "red"
        : "default";
    return <Tag color={color}>{item.value}</Tag>;
  }
  return <Typography.Text copyable={item.value.length > 40}>{item.value}</Typography.Text>;
}

function isSoftwareEstop(resource: HardwareResource): boolean {
  if (resource.kind !== "estop") return false;
  const source = resource.fields.find((field) => field.name === "source")?.value ?? "";
  return `${resource.model} ${source}`.toLowerCase().includes("software");
}

function ResourceCard({ resource }: { resource: HardwareResource }) {
  const { t } = useI18n();
  return (
    <Card
      size="small"
      title={(
        <Space wrap>
          <Typography.Text code>{resource.id}</Typography.Text>
          <Tag color="blue">{resource.kind || "unknown"}</Tag>
          {resource.model && <Tag>{resource.model}</Tag>}
        </Space>
      )}
      extra={resource.alive
        ? <Tag color="green">{t("hwAlive")}</Tag>
        : <Tag color="red">{t("hwOffline")}</Tag>}
    >
      {isSoftwareEstop(resource) && (
        <Alert
          type="warning"
          showIcon
          message={t("hwSoftwareEstop")}
          style={{ marginBottom: 10 }}
        />
      )}
      <Descriptions size="small" bordered column={{ xs: 1, sm: 2, lg: 3 }}>
        <Descriptions.Item label="key" span={3}>
          <Typography.Text code copyable>{resource.key}</Typography.Text>
        </Descriptions.Item>
        <Descriptions.Item label={t("hwSampleAge")}>{ageText(resource.sample_age_ms)}</Descriptions.Item>
        {resource.sample_bytes != null && (
          <Descriptions.Item label={t("hwPayloadBytes")}>{resource.sample_bytes}</Descriptions.Item>
        )}
        {resource.header_present != null && (
          <Descriptions.Item label="Header">
            <Tag color={resource.header_present ? "green" : "red"}>
              {resource.header_present ? t("hwPresent") : t("hwMissing")}
            </Tag>
          </Descriptions.Item>
        )}
        {resource.seq != null && <Descriptions.Item label="Header.seq">{resource.seq}</Descriptions.Item>}
        {resource.stamp_ns != null && (
          <Descriptions.Item label="Header.stamp_ns">{resource.stamp_ns}</Descriptions.Item>
        )}
        {resource.sync_ns != null && (
          <Descriptions.Item label="Header.sync_ns">{resource.sync_ns}</Descriptions.Item>
        )}
        {resource.fields.map((item) => (
          <Descriptions.Item key={item.name} label={item.name}>
            {fieldValue(resource, item)}
          </Descriptions.Item>
        ))}
      </Descriptions>
      {resource.sample_age_ms == null && (
        <Alert type="info" showIcon message={t("hwWaitingSample")} style={{ marginTop: 10 }} />
      )}
      {resource.header_present === false && (
        <Alert type="error" showIcon message={t("hwHeaderMissing")} style={{ marginTop: 10 }} />
      )}
      {resource.decode_error && (
        <Alert
          type="error"
          showIcon
          message={t("hwDecodeError")}
          description={resource.decode_error}
          style={{ marginTop: 10 }}
        />
      )}
    </Card>
  );
}

export function HardwarePanel({
  cid,
  controller,
  discoveryErrors,
}: {
  cid: string;
  controller?: HardwareController;
  discoveryErrors: string[];
}) {
  const { t } = useI18n();
  if (!controller) {
    return (
      <Card size="small" title={`${t("hwTitle")} · ${cid}`}>
        <Alert type="warning" showIcon message={t("hwNoInfo")} />
        {discoveryErrors.map((error, index) => (
          <Alert key={`${index}:${error}`} type="error" message={error} style={{ marginTop: 8 }} />
        ))}
      </Card>
    );
  }

  const cidMismatch = controller.reported_controller_ids.some((reported) => reported !== cid);
  return (
    <Space direction="vertical" size={10} style={{ width: "100%" }}>
      <Card size="small" title={`${t("hwTitle")} · ${cid}`}>
        <Descriptions size="small" bordered column={{ xs: 1, sm: 2, lg: 3 }}>
          <Descriptions.Item label={t("hwSupervisorVersion")}>
            {controller.supervisor_versions.join(", ") || "—"}
          </Descriptions.Item>
          <Descriptions.Item label={t("hwInfoReplies")}>
            <Tag color={controller.info_reply_count === 1 ? "green" : "red"}>
              {controller.info_reply_count}
            </Tag>
          </Descriptions.Item>
          <Descriptions.Item label={t("hwResourceCount")}>{controller.resources.length}</Descriptions.Item>
          <Descriptions.Item label="HwInfo.controller_id" span={3}>
            <Typography.Text type={cidMismatch ? "danger" : undefined}>
              {controller.reported_controller_ids.join(", ") || "—"}
            </Typography.Text>
          </Descriptions.Item>
        </Descriptions>
        {controller.warnings.map((warning, index) => (
          <Alert key={`${index}:${warning}`} type="error" showIcon message={warning} style={{ marginTop: 8 }} />
        ))}
        {discoveryErrors.map((error, index) => (
          <Alert key={`${index}:${error}`} type="error" showIcon message={error} style={{ marginTop: 8 }} />
        ))}
      </Card>
      {controller.resources.length === 0
        ? <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={t("hwNoResources")} />
        : controller.resources.map((resource) => (
          <ResourceCard key={resource.key} resource={resource} />
        ))}
    </Space>
  );
}
