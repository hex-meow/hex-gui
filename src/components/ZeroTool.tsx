import { useEffect, useRef, useState } from "react";
import { Alert, App, Button, Card, InputNumber, Space, Table, Tag, Typography } from "antd";
import { api, errMsg } from "../api";
import { nid2hex, fmtNum } from "../format";
import { useI18n } from "../i18n";
import type { MotorInfo } from "../types";

// Read 0x6064 this long after a preset write, so the motor has applied it.
const READ_AFTER_SAVE_MS = 20;
// Small delay before the on-discovery read, to let discovery's identify SDO
// finish first (avoids the per-node inflight-op clash).
const READ_ON_DISCOVERY_MS = 250;

export function ZeroTool({
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
  const [presetPos, setPresetPos] = useState(0);
  const [busy, setBusy] = useState(false);
  // nid -> last-read position (Rev)
  const [positions, setPositions] = useState<Record<number, number>>({});
  const readNids = useRef<Set<number>>(new Set());

  useEffect(() => {
    if (selectedNid != null) setCurrentId(selectedNid);
  }, [selectedNid]);

  const readOne = (nid: number) =>
    api.readPosition(nid).then((p) => setPositions((m) => ({ ...m, [nid]: p })));

  // Read each motor once when it first appears; re-read after it disappears and
  // comes back (e.g. power-cycle). NEVER poll — only read on discovery here.
  useEffect(() => {
    if (!connected) {
      readNids.current.clear();
      setPositions({});
      return;
    }
    const online = new Set(devices.filter((d) => d.online).map((d) => d.node_id));
    // forget motors that went away (so they re-read on return)
    for (const nid of [...readNids.current]) {
      if (!online.has(nid)) {
        readNids.current.delete(nid);
        setPositions((m) => {
          const n = { ...m };
          delete n[nid];
          return n;
        });
      }
    }
    // read newly-online motors once
    for (const nid of online) {
      if (!readNids.current.has(nid)) {
        readNids.current.add(nid);
        window.setTimeout(() => {
          readOne(nid).catch(() => readNids.current.delete(nid)); // allow retry
        }, READ_ON_DISCOVERY_MS);
      }
    }
  }, [devices, connected]);

  const readNow = async () => {
    if (currentId == null) return;
    setBusy(true);
    try {
      await readOne(currentId);
    } catch (e) {
      message.error(`${t("readFailed")}: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (currentId == null) return;
    setBusy(true);
    try {
      await api.setPositionPreset(currentId, presetPos);
      message.success(`${t("zeroDone")} ${nid2hex(currentId)} → ${presetPos.toFixed(4)} rev`);
      // fixed single read 20 ms later to confirm
      window.setTimeout(() => {
        readOne(currentId).catch(() => {});
      }, READ_AFTER_SAVE_MS);
    } catch (e) {
      message.error(`${t("zeroFailed")}: ${errMsg(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const curPos = currentId != null ? positions[currentId] : undefined;

  return (
    <Space direction="vertical" size={12} style={{ width: "100%", maxWidth: 640 }}>
      <Card size="small" title={t("zeroTitle")}>
        {!connected && <Alert type="info" showIcon message={t("connectFirst")} style={{ marginBottom: 12 }} />}
        <Typography.Paragraph type="secondary" style={{ marginBottom: 12 }}>
          {t("zeroPick")}
        </Typography.Paragraph>

        <Space align="end" wrap style={{ marginBottom: 12 }}>
          <Field label={t("motorId")}>
            <InputNumber min={1} max={127} value={currentId} onChange={setCurrentId} style={{ width: 110 }} />
          </Field>
          <Button disabled={!connected || currentId == null} loading={busy} onClick={readNow}>
            {t("readPos")}
          </Button>
          <Typography.Text>
            {t("currentPos")}: <b>{curPos != null ? `${curPos.toFixed(4)} rev` : "—"}</b>
          </Typography.Text>
        </Space>

        <Space align="end" wrap>
          <Field label={t("presetPos")}>
            <InputNumber
              min={-0.5}
              max={0.5}
              step={0.01}
              value={presetPos}
              onChange={(v) => setPresetPos(v ?? 0)}
              style={{ width: 140 }}
            />
          </Field>
          <Button
            type="primary"
            disabled={!connected || currentId == null}
            loading={busy}
            onClick={save}
          >
            {t("savePos")}
          </Button>
        </Space>
      </Card>

      <Card size="small" title={t("discovered")}>
        <Table
          size="small"
          pagination={false}
          rowKey={(d) => d.node_id}
          dataSource={devices}
          columns={[
            { title: "ID", dataIndex: "node_id", render: (n: number) => <Typography.Text code>{nid2hex(n)}</Typography.Text> },
            { title: "", dataIndex: "online", render: (o: boolean) => (o ? <Tag color="green">online</Tag> : <Tag color="red">offline</Tag>) },
            { title: t("currentPos"), render: (_: unknown, d: MotorInfo) => fmtNum(positions[d.node_id], 4) },
            {
              title: "",
              render: (_: unknown, d: MotorInfo) => (
                <Button size="small" disabled={!d.online} onClick={() => readOne(d.node_id).catch(() => {})}>
                  {t("readPos")}
                </Button>
              ),
            },
          ]}
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
