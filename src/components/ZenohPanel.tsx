import { useCallback, useEffect, useRef, useState } from "react";
import { App as AntdApp, Button, Card, Col, Input, InputNumber, Row, Select, Space, Statistic, Switch, Tag, Typography } from "antd";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import type { BaseInfo, ZenohBaseState } from "../types";

const POLL_MS = 10;

export function ZenohPanel() {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();

  const [endpoint, setEndpoint] = useState(""); // 留空 = 自动扫描局域网(组播)
  const [connected, setConnected] = useState(false);
  const [bases, setBases] = useState<BaseInfo[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [st, setSt] = useState<ZenohBaseState | null>(null);
  const [lin, setLin] = useState(0.2);
  const [ang, setAng] = useState(0.5);
  const [busy, setBusy] = useState(false);

  // 轮询状态
  useEffect(() => {
    if (!connected) { setSt(null); return; }
    let alive = true;
    const tick = async () => {
      try { const s = await api.zenohGetState(); if (alive) setSt(s); } catch { /* transient */ }
    };
    tick();
    const h = window.setInterval(tick, POLL_MS);
    return () => { alive = false; window.clearInterval(h); };
  }, [connected]);

  // 卸载时释放 + 断开(安全)
  useEffect(() => () => { api.zenohDisconnect().catch(() => {}); }, []);

  const connect = useCallback(async () => {
    setBusy(true);
    try {
      await api.zenohConnect(endpoint.trim());
      setConnected(true);
      message.success(t("zConnected"));
      // 组播发现可能稍慢,自动重试一次。
      let list = await api.zenohDiscover();
      if (list.length === 0) {
        await new Promise((r) => setTimeout(r, 900));
        list = await api.zenohDiscover();
      }
      setBases(list);
      setSelected(list[0]?.prefix ?? null);
      if (list.length === 0) message.warning(t("zNoBase"));
    } catch (e) { message.error(errMsg(e)); }
    finally { setBusy(false); }
  }, [endpoint, message, t]);

  const disconnect = useCallback(async () => {
    try { await api.zenohDisconnect(); } catch { /* ignore */ }
    setConnected(false); setBases([]); setSelected(null); setSt(null);
  }, []);

  const discover = useCallback(async () => {
    try { const list = await api.zenohDiscover(); setBases(list); if (!selected) setSelected(list[0]?.prefix ?? null);
      if (list.length === 0) message.warning(t("zNoBase")); }
    catch (e) { message.error(errMsg(e)); }
  }, [selected, message, t]);

  const acquire = useCallback(async () => {
    const b = bases.find((x) => x.prefix === selected);
    if (!b) return;
    try { await api.zenohAcquire(b.prefix, b.model); message.success(t("zControlling")); }
    catch (e) { message.error(errMsg(e)); }
  }, [bases, selected, message, t]);

  const release = useCallback(async () => {
    try { await api.zenohRelease(); } catch (e) { message.error(errMsg(e)); }
  }, [message]);

  const setActive = useCallback(async (on: boolean) => {
    try { await api.zenohSetActive(on); } catch (e) { message.error(errMsg(e)); }
  }, [message]);

  const cmd = useCallback((vx: number, vy: number, wz: number) => { api.zenohSetCmd(vx, vy, wz).catch(() => {}); }, []);
  const stop = useCallback(() => cmd(0, 0, 0), [cmd]);

  // 按住移动、松开停止
  const hold = (vx: number, vy: number, wz: number) => ({
    onMouseDown: () => cmd(vx, vy, wz),
    onMouseUp: stop,
    onMouseLeave: stop,
  });

  // 固定 3 位小数,避免 antd 对极小值用科学计数法。
  const fmt3 = (v: number | string) => Number(v).toFixed(3);
  const controlling = !!st?.controlling;
  const controlTag = controlling
    ? <Tag color="green">{t("zControlling")}</Tag>
    : st && st.holder !== 0
      ? <Tag color="orange">{t("zBusy")} (#{st.holder})</Tag>
      : <Tag>{t("zNotControlling")}</Tag>;

  return (
    <Space direction="vertical" size={16} style={{ width: "100%", maxWidth: 720 }}>
      <Typography.Title level={4} style={{ margin: 0 }}>{t("toolBaseZenoh")}</Typography.Title>

      {/* 连接 + 发现 */}
      <Card size="small">
        <Space wrap>
          <Typography.Text>{t("zEndpoint")}</Typography.Text>
          <Input style={{ width: 240 }} value={endpoint} disabled={connected} placeholder={t("zEndpointHint")} onChange={(e) => setEndpoint(e.target.value)} />
          {connected
            ? <Button onClick={disconnect}>{t("zDisconnect")}</Button>
            : <Button type="primary" loading={busy} onClick={connect}>{t("zConnect")}</Button>}
          {connected && <Button onClick={discover}>{t("zDiscover")}</Button>}
        </Space>
        {connected && (
          <div style={{ marginTop: 12 }}>
            <Space wrap>
              <Typography.Text type="secondary">{t("zFound")}: {bases.length}</Typography.Text>
              <Select
                style={{ width: 320 }}
                value={selected ?? undefined}
                onChange={setSelected}
                options={bases.map((b) => ({ value: b.prefix, label: `${b.model} — ${b.prefix}` }))}
              />
              {controlling
                ? <Button danger onClick={release}>{t("zRelease")}</Button>
                : <Button type="primary" disabled={!selected} onClick={acquire}>{t("zAcquire")}</Button>}
            </Space>
          </div>
        )}
      </Card>

      {/* 控制 */}
      <Card size="small" title={<Space>{controlTag}{controlling && <>{t("zActive")}: <Switch onChange={setActive} /></>}</Space>}>
        <Row gutter={[24, 16]} align="top" wrap={false} style={{ minWidth: 600 }}>
          <Col flex="260px">
            <Space direction="vertical">
              <Typography.Text strong>{t("zMove")}</Typography.Text>
              <Space>
                <span style={{ width: 36, display: "inline-block" }} />
                <Button disabled={!controlling} {...hold(lin, 0, 0)}>▲</Button>
                <span style={{ width: 36, display: "inline-block" }} />
              </Space>
              <Space>
                <Button disabled={!controlling} {...hold(0, lin, 0)}>◀</Button>
                <Button danger disabled={!controlling} onClick={stop}>{t("zStop")}</Button>
                <Button disabled={!controlling} {...hold(0, -lin, 0)}>▶</Button>
              </Space>
              <Space>
                <Button disabled={!controlling} {...hold(0, 0, ang)}>↺</Button>
                <Button disabled={!controlling} {...hold(-lin, 0, 0)}>▼</Button>
                <Button disabled={!controlling} {...hold(0, 0, -ang)}>↻</Button>
              </Space>
              <Space>
                <span>{t("zSpeedLin")}</span><InputNumber min={0} max={3} step={0.1} value={lin} onChange={(v) => setLin(v ?? 0)} />
              </Space>
              <Space>
                <span>{t("zSpeedAng")}</span><InputNumber min={0} max={3} step={0.1} value={ang} onChange={(v) => setAng(v ?? 0)} />
              </Space>
            </Space>
          </Col>
          <Col flex="auto">
            <Typography.Text strong>{t("zPose")}</Typography.Text>
            <Row>
              <Col style={{ width: 96 }}><Statistic title="x (m)" value={st?.pose_x ?? 0} formatter={fmt3} /></Col>
              <Col style={{ width: 96 }}><Statistic title="y (m)" value={st?.pose_y ?? 0} formatter={fmt3} /></Col>
              <Col style={{ width: 96 }}><Statistic title="θ (rad)" value={st?.pose_theta ?? 0} formatter={fmt3} /></Col>
            </Row>
            <Typography.Text strong>{t("zTwist")}</Typography.Text>
            <Row>
              <Col style={{ width: 96 }}><Statistic title="vx" value={st?.vx ?? 0} formatter={fmt3} /></Col>
              <Col style={{ width: 96 }}><Statistic title="vy" value={st?.vy ?? 0} formatter={fmt3} /></Col>
              <Col style={{ width: 96 }}><Statistic title="ωz" value={st?.wz ?? 0} formatter={fmt3} /></Col>
            </Row>
          </Col>
        </Row>
      </Card>
    </Space>
  );
}
