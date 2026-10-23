import { useEffect, useMemo, useRef, useState } from "react";
import { Alert, Button, Card, Descriptions, Empty, Input, Progress, Segmented, Select, Space, Table, Tag, Typography } from "antd";
import { api, errMsg } from "../api";
import type { DamiaoRegisterDefinition, DamiaoRegisterReading, DamiaoRegisterWriteResult } from "../types";

type RegisterSample = { reading?: DamiaoRegisterReading; time?: string; error?: string };
const GROUPS: Record<string, [string, string]> = {
  protection: ["保护参数", "Protection"], motion: ["运动参数", "Motion"],
  communication: ["通信配置", "Communication"], mapping: ["协议映射", "Protocol mapping"],
  tuning: ["控制环参数", "Loop tuning"], motor: ["电机参数", "Motor"],
  version: ["版本与保留项", "Versions / reserved"], telemetry: ["实时数据", "Live data"],
};
const BITRATES = ["125 kbps", "200 kbps", "250 kbps", "500 kbps", "1 Mbps", "2 Mbps", "2.5 Mbps", "3.2 Mbps", "4 Mbps", "5 Mbps"];
const hex = (value: number, width = 2) => `0x${value.toString(16).toUpperCase().padStart(width, "0")}`;

export function DamiaoRegisterPanel({ motorId, ready, busy, zh, runOperation, onWrite }: {
  motorId: number | null;
  ready: boolean;
  busy: boolean;
  zh: boolean;
  runOperation: (action: () => Promise<void>) => Promise<unknown>;
  onWrite: (address: number, value: number) => Promise<DamiaoRegisterWriteResult>;
}) {
  const [catalog, setCatalog] = useState<DamiaoRegisterDefinition[]>([]);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [samples, setSamples] = useState<Record<number, RegisterSample>>({});
  const [query, setQuery] = useState("");
  const [group, setGroup] = useState("all");
  const [access, setAccess] = useState("all");
  const [selected, setSelected] = useState<number | null>(null);
  const [draft, setDraft] = useState("");
  const [draftError, setDraftError] = useState<string | null>(null);
  const [progress, setProgress] = useState<{ done: number; total: number; failures: number } | null>(null);
  const [reading, setReading] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const cancelRead = useRef(false);
  const editorRef = useRef<HTMLDivElement>(null);
  const mounted = useRef(true);
  const epoch = useRef(0);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; cancelRead.current = true; epoch.current += 1; };
  }, []);

  const loadCatalog = async () => {
    setCatalogLoading(true);
    try {
      const entries = await api.damiaoListRegisters();
      if (mounted.current) { setCatalog(entries); setCatalogError(null); }
    } catch (error) {
      if (mounted.current) setCatalogError(errMsg(error));
    } finally { if (mounted.current) setCatalogLoading(false); }
  };
  useEffect(() => { void loadCatalog(); }, []);
  useEffect(() => {
    if (selected != null) editorRef.current?.scrollIntoView({ behavior: "smooth", block: "nearest" });
  }, [selected]);
  useEffect(() => {
    epoch.current += 1;
    cancelRead.current = true;
    setSamples({}); setSelected(null); setDraft(""); setDraftError(null);
    setNotice(null); setProgress(null); setReading(false);
  }, [motorId]);

  const visible = useMemo(() => catalog.filter((entry) => {
    const search = `${entry.name} ${entry.label_zh} ${entry.label_en} ${hex(entry.address)} ${entry.address}`.toLowerCase();
    return (group === "all" || entry.group === group)
      && (access === "all" || entry.writable === (access === "rw"))
      && search.includes(query.trim().toLowerCase());
  }), [catalog, query, group, access]);
  const definition = catalog.find((entry) => entry.address === selected);
  const current = selected == null ? undefined : samples[selected];
  const readCount = Object.values(samples).filter((sample) => sample.reading != null).length;

  const selectEntry = (entry: DamiaoRegisterDefinition) => {
    setSelected(entry.address);
    const value = samples[entry.address]?.reading?.value;
    setDraft(value == null ? "" : String(value));
    setDraftError(null);
  };

  const readRegisters = async (entries: DamiaoRegisterDefinition[]) => {
    if (!ready || busy || motorId == null || !entries.length) return;
    try {
      await runOperation(async () => {
        const generation = epoch.current;
        cancelRead.current = false;
        setReading(true); setNotice(null);
        let done = 0, failures = 0, consecutiveFailures = 0;
        setProgress({ done, total: entries.length, failures });
        try {
          for (const entry of entries) {
            if (cancelRead.current || generation !== epoch.current) break;
            try {
              const value = await api.damiaoReadRegister(motorId, entry.address);
              if (!mounted.current || generation !== epoch.current) break;
              setSamples((old) => ({ ...old, [entry.address]: { reading: value, time: new Date().toLocaleTimeString() } }));
              consecutiveFailures = 0;
            } catch (error) {
              if (!mounted.current || generation !== epoch.current) break;
              failures += 1; consecutiveFailures += 1;
              setSamples((old) => ({ ...old, [entry.address]: { error: errMsg(error) } }));
            }
            done += 1;
            setProgress({ done, total: entries.length, failures });
            if (consecutiveFailures >= 3) {
              setNotice(zh ? "连续 3 项读取失败，已停止批量读取。检查通信后可单独重读失败项。" : "Stopped after 3 consecutive failures. Check communication, then retry individual registers.");
              break;
            }
          }
        } finally {
          if (mounted.current && generation === epoch.current) setReading(false);
        }
      });
    } catch { /* The parent reports operation errors. */ }
  };

  const writeRegister = async () => {
    if (!definition?.writable || !ready || busy) return;
    const validation = validateDraft(definition, draft, zh);
    if (typeof validation === "string") { setDraftError(validation); return; }
    setDraftError(null);
    try {
      await runOperation(async () => {
        const generation = epoch.current;
        // The old sample is no longer authoritative once a write is attempted.
        setSamples((old) => ({ ...old, [definition.address]: {} }));
        try {
          const result = await onWrite(definition.address, validation);
          if (!mounted.current || generation !== epoch.current) return;
          if (result.reading) {
            setSamples((old) => ({ ...old, [definition.address]: { reading: result.reading!, time: new Date().toLocaleTimeString() } }));
            setDraft(result.reading.value == null ? "" : String(result.reading.value));
            setNotice(zh ? "已写入并回读确认。当前值尚未保存到 Flash；需断电保留时使用下方“保存配置”。" : "Written and verified by readback. Use Save configuration below to retain settings after power loss.");
          } else {
            setSamples((old) => ({ ...old, [definition.address]: { error: zh ? "通信参数已写入或结果未确认，请重新连接后读取。" : "Communication settings written or uncertain; reconnect and read back." } }));
            setNotice(zh ? "请查看通信配置结果，重新连接后读取确认，再保存到 Flash。" : "Review the communication result, reconnect, verify and then save to Flash.");
          }
        } catch (error) {
          if (mounted.current && generation === epoch.current) {
            setSamples((old) => ({ ...old, [definition.address]: { error: errMsg(error) } }));
          }
          throw error;
        }
      });
    } catch { /* The parent reports operation errors. */ }
  };

  return <Card size="small" title={zh ? "全部寄存器" : "All registers"} className="damiao-register-card"
    extra={<Space size={4}><Tag color="blue">{catalog.filter((entry) => entry.writable).length} RW</Tag><Tag>{catalog.filter((entry) => !entry.writable).length} RO</Tag></Space>}>
    <Space direction="vertical" size={12} style={{ width: "100%" }}>
      <Typography.Text type="secondary">{zh
        ? "依据手册第 17–19 页。读取不修改参数；写入会停止定时目标并确认失能，成功后回读校验。只读项无法写入。"
        : "Manual pp. 17–19. Reads do not change parameters. Writes stop periodic targets, confirm disabled status and verify by readback. RO entries cannot be written."}</Typography.Text>
      {catalogError && <Alert type="error" showIcon message={catalogError} action={<Button onClick={() => void loadCatalog()}>{zh ? "重试" : "Retry"}</Button>} />}
      <div className="damiao-register-toolbar">
        <Input allowClear value={query} onChange={(event) => setQuery(event.target.value)} placeholder={zh ? "搜索名称、地址或说明" : "Search name, address or description"} />
        <Select value={group} onChange={setGroup} options={[{ value: "all", label: zh ? "全部分组" : "All groups" }, ...Object.entries(GROUPS).map(([value, labels]) => ({ value, label: labels[zh ? 0 : 1] }))]} />
        <Segmented value={access} onChange={(value) => setAccess(String(value))} options={[{ value: "all", label: zh ? "全部" : "All" }, { value: "rw", label: "RW" }, { value: "ro", label: "RO" }]} />
      </div>
      <Space wrap>
        <Button type="primary" disabled={!ready || busy || !catalog.length} onClick={() => void readRegisters(catalog)}>{zh ? "读取全部" : "Read all"}</Button>
        <Button disabled={!ready || busy || !visible.length} onClick={() => void readRegisters(visible)}>{zh ? `读取筛选项 (${visible.length})` : `Read filtered (${visible.length})`}</Button>
        {reading && <Button onClick={() => { cancelRead.current = true; }}>{zh ? "停止读取" : "Stop reading"}</Button>}
        <Typography.Text type="secondary">{zh ? `已读取 ${readCount} / ${catalog.length} 项 · 显示上次读取值` : `${readCount} / ${catalog.length} read · Last read values`}</Typography.Text>
      </Space>
      {progress && <Progress percent={Math.round(progress.done * 100 / progress.total)} status={progress.failures ? "exception" : reading ? "active" : "normal"}
        format={() => `${progress.done}/${progress.total}${progress.failures ? ` · ${progress.failures} ${zh ? "失败" : "failed"}` : ""}`} />}
      {notice && <Alert type="info" showIcon message={notice} closable onClose={() => setNotice(null)} />}
      <Table<DamiaoRegisterDefinition> size="small" rowKey="address" loading={catalogLoading} dataSource={visible}
        scroll={{ x: 760 }} pagination={{ defaultPageSize: 10, showSizeChanger: true, pageSizeOptions: [10, 20, 50], hideOnSinglePage: true }}
        locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={zh ? "没有匹配的寄存器" : "No matching registers"} /> }}
        rowClassName={(entry) => entry.address === selected ? "damiao-register-selected" : ""}
        columns={[
          { title: zh ? "寄存器" : "Register", key: "name", width: 225, render: (_, entry) => <div><Typography.Text code>{hex(entry.address)}</Typography.Text><Typography.Text strong>{entry.name}</Typography.Text><div className="damiao-register-secondary">{zh ? entry.label_zh : entry.label_en}</div></div> },
          { title: zh ? "权限 / 类型" : "Access / type", key: "access", width: 105, render: (_, entry) => <div><Tag color={entry.writable ? "blue" : undefined}>{entry.writable ? "RW" : "RO"}</Tag><div className="damiao-register-secondary">{entry.data_type === "Float32" ? "float32" : "uint32"}</div></div> },
          { title: zh ? "当前值" : "Read value", key: "value", width: 160, render: (_, entry) => {
            const sample = samples[entry.address];
            return <div>{sample?.error ? <Typography.Text type="danger">{zh ? "读取 / 校验失败" : "Read / verification failed"}</Typography.Text>
              : sample?.reading ? <><Typography.Text className="damiao-tabular">{formatValue(entry, sample.reading)}</Typography.Text><div className="damiao-register-secondary">{sample.time}</div></> : "—"}</div>;
          } },
          { title: zh ? "手册范围" : "Manual range", key: "range", width: 140, render: (_, entry) => <div>{entry.range}<div className="damiao-register-secondary">{entry.unit}</div></div> },
          { title: zh ? "操作" : "Actions", key: "actions", width: 130, render: (_, entry) => <Space size={0}>
            <Button size="small" type="link" disabled={!ready || busy} onClick={() => { selectEntry(entry); void readRegisters([entry]); }}>{zh ? "读取" : "Read"}</Button>
            <Button size="small" type="link" disabled={busy} onClick={() => selectEntry(entry)}>{entry.writable ? zh ? "编辑" : "Edit" : zh ? "详情" : "Details"}</Button>
          </Space> },
        ]} />
      {definition && <div className="damiao-register-editor" ref={editorRef}>
        <Space wrap><Typography.Text strong>{hex(definition.address)} · {definition.name} · {zh ? definition.label_zh : definition.label_en}</Typography.Text><Tag color={definition.writable ? "blue" : undefined}>{definition.writable ? "RW" : "RO"}</Tag></Space>
        <Descriptions size="small" column={{ xs: 1, sm: 2 }}>
          <Descriptions.Item label={zh ? "上次读取" : "Last read"}>{current?.reading ? formatValue(definition, current.reading) : "—"}</Descriptions.Item>
          <Descriptions.Item label={zh ? "原始 32 位值" : "Raw 32 bits"}>{current?.reading ? hex(current.reading.raw, 8) : "—"}</Descriptions.Item>
          <Descriptions.Item label={zh ? "范围" : "Range"}>{definition.range} {definition.unit}</Descriptions.Item>
          <Descriptions.Item label={zh ? "类型" : "Type"}>{definition.data_type === "Float32" ? "float32 (IEEE 754)" : "uint32"} · {zh ? "小端" : "little endian"}</Descriptions.Item>
        </Descriptions>
        {(zh ? definition.note_zh : definition.note_en) && <Typography.Text type="secondary">{zh ? definition.note_zh : definition.note_en}</Typography.Text>}
        {current?.error && <Alert type="error" showIcon message={current.error} />}
        {current?.reading?.value === null && <Alert type="warning" showIcon message={zh ? "设备返回非有限浮点值，请检查原始值与固件。" : "The device returned a non-finite float. Check raw bits and firmware."} />}
        {definition.writable ? <>
          <Space wrap>
            {definition.address === 0x0A || definition.address === 0x23
              ? <Select className="damiao-register-value-input" value={draft === "" ? undefined : Number(draft)} disabled={busy}
                placeholder={zh ? "选择新值" : "Select a value"} onChange={(value) => { setDraft(String(value)); setDraftError(null); }}
                options={definition.address === 0x23 ? BITRATES.map((label, value) => ({ value, label: `${value} · ${label}${value > 4 ? " · CAN FD" : ""}` }))
                  : [zh ? "未定义（禁止运动）" : "Undefined (motion blocked)", "MIT", zh ? "位置速度" : "Position + velocity", zh ? "速度" : "Velocity", zh ? "力位混控" : "Force + position"].map((label, value) => ({ value, label: `${value} · ${label}` }))} />
              : <Input className="damiao-register-value-input" disabled={busy} value={draft} status={draftError ? "error" : undefined} onChange={(event) => { setDraft(event.target.value); setDraftError(null); }}
                placeholder={definition.data_type === "Uint32" ? zh ? "整数，支持 0x 十六进制" : "Integer or 0x hex" : zh ? "浮点数，支持科学计数法" : "Float or scientific notation"} />}
            <Button disabled={busy || current?.reading?.value == null} onClick={() => { setDraft(String(current!.reading!.value)); setDraftError(null); }}>{zh ? "填入当前值" : "Use read value"}</Button>
            <Button type="primary" disabled={!ready || busy || !draft.trim()} onClick={() => void writeRegister()}>{zh ? "失能并写入" : "Disable & write"}</Button>
          </Space>
          {draftError && <Typography.Text type="danger">{draftError}</Typography.Text>}
          <Typography.Text type="secondary">{zh ? "写入即时生效，电机保持失能。批量读取不会写入或自动保存参数。fmax = 最大有限 float32；圆括号表示不含端点。" : "Writes apply immediately and leave the motor disabled. Bulk reads never write or save parameters. fmax = largest finite float32; parentheses exclude endpoints."}</Typography.Text>
        </> : <Typography.Text type="secondary">{zh ? "此项在手册中为只读，不提供写入操作。" : "This register is read-only in the manual."}</Typography.Text>}
      </div>}
    </Space>
  </Card>;
}

function formatValue(entry: DamiaoRegisterDefinition, reading: DamiaoRegisterReading): string {
  if (reading.value == null) return "NaN / Infinity";
  if (entry.address === 0x07 || entry.address === 0x08) return `${hex(reading.value)} (${reading.value})`;
  if (entry.address === 0x23) return `${reading.value} · ${BITRATES[reading.value] ?? "?"}`;
  if (entry.address === 0x09) return `${reading.value} (${Number((reading.value * 0.05).toPrecision(10))} ms)`;
  return `${entry.data_type === "Float32" ? Number(reading.value.toPrecision(8)) : reading.value}${entry.unit ? ` ${entry.unit}` : ""}`;
}

function validateDraft(entry: DamiaoRegisterDefinition, draft: string, zh: boolean): number | string {
  const value = Number(draft.trim());
  const parsed = entry.data_type === "Uint32" ? value : Math.fround(value);
  if (!draft.trim() || !Number.isFinite(value) || !Number.isFinite(parsed)
    || (entry.data_type === "Uint32" && (!Number.isInteger(value) || value < 0 || value > 0xFFFFFFFF))) {
    return zh ? `请输入有效的 ${entry.data_type} 值。` : `Enter a valid ${entry.data_type} value.`;
  }
  for (const candidate of [value, parsed]) {
    if ((entry.min != null && (candidate < entry.min || (!entry.min_inclusive && candidate === entry.min)))
      || (entry.max != null && (candidate > entry.max || (!entry.max_inclusive && candidate === entry.max)))) {
      return zh ? `数值及其 float32 编码必须在 ${entry.range} 内。` : `The value and float32 encoding must be within ${entry.range}.`;
    }
  }
  return value;
}
