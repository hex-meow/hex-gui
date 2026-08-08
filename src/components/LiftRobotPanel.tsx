// Lift(Zenoh)工具:发现/取控 + 回零 + 高度控制(位置目标 / 点动)+ 状态徽标。
// 镜像 ArmPanel 的连接/取控流;控制部分换成升降特有。
// 设计对应 robot-overall-design/12-lift-api.md。
//
// 与直连 CAN 的 LiftPanel(catRawCanApp)是两个工具:那个直接说 CANopen(调试/产线),
// 这个只说公共 robot API,因此对升降是"底盘进程托管"还是"独占总线的 lift_controller"
// 完全无感 —— 键空间一样。
//
// 三个必须让操作者一眼看到的事实(都来自 12-lift-api,不是 UI 偏好):
//   1. **未回零就动不了**:设备复位后 homed=false,set_mode(ACTIVE) 会被控制器拒绝。
//      所以未回零时把运动控件全禁掉,并把"回零"做成显眼的主按钮,而不是等用户撞一鼻子灰。
//   2. **位置是自主目标**:发完就撒手,完成与否只能看 target_reached(自主 goal 无回执)。
//   3. **点动有下限**:|dq| < vel_min 时设备脱力滑行,发再小的速度也不会动 —— 滑条下限取 vel_min。

import { useCallback, useEffect, useRef, useState } from "react";
import {
  App as AntdApp, Alert, Button, Card, Descriptions, Input, InputNumber, Select,
  Slider, Space, Tag, Typography,
} from "antd";
import { api, errMsg } from "../api";
import type { LiftRobotInfo, ZenohLiftState } from "../types";
import { FaultAlert, RobotModeTag } from "./DiagnosticsPanel";
import { useI18n } from "../i18n";

const POLL_MS = 100; // 升降是慢轴,10Hz 足够;状态本身也只有 ~10Hz

/** 状态徽标。只显示"有意义"的位,避免一排常绿的噪声。 */
function StatusTags({ st }: { st: ZenohLiftState }) {
  return (
    <>
      {st.homed ? <Tag color="green">已回零</Tag> : <Tag color="orange">未回零</Tag>}
      {/* CONFIG_VALID=0 是 fail-closed 硬状态:铭牌/型号/CRC 没过,设备拒绝一切运动 */}
      {!st.config_valid && <Tag color="red">配置无效</Tag>}
      {st.moving && <Tag color="processing">运动中</Tag>}
      {st.target_reached && <Tag color="success">已到位</Tag>}
      {st.output_limited && <Tag color="warning">输出受限</Tag>}
      {st.at_lower_limit && <Tag color="warning">下限位</Tag>}
      {st.at_upper_limit && <Tag color="warning">上限位</Tag>}
      {st.estop && <Tag color="red">急停</Tag>}
      {st.fault_code !== 0 && (
        <Tag color="error">
          故障 0x{st.fault_code.toString(16).toUpperCase().padStart(4, "0")}
          {st.fault_text ? ` ${st.fault_text}` : ""}
        </Tag>
      )}
    </>
  );
}

export function LiftRobotPanel() {
  const { message } = AntdApp.useApp();
  const { t } = useI18n();
  const [endpoint, setEndpoint] = useState("");
  const [connected, setConnected] = useState(false);
  const [lifts, setLifts] = useState<LiftRobotInfo[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [st, setSt] = useState<ZenohLiftState | null>(null);
  const [busy, setBusy] = useState(false);
  const [target, setTarget] = useState(0); // 高度滑条本地值
  const [jogSpeed, setJogSpeed] = useState(0.01);
  const dragging = useRef(false);

  useEffect(() => {
    if (!connected) { setSt(null); return; }
    let alive = true;
    const tick = async () => {
      try {
        const s = await api.zliftGetState();
        if (!alive) return;
        setSt(s);
        // 未拖动时滑条跟随实测高度,拖动时不抢用户的手。
        if (!dragging.current) setTarget(s.height);
      } catch { /* transient */ }
    };
    tick();
    const h = window.setInterval(tick, POLL_MS);
    return () => { alive = false; window.clearInterval(h); };
  }, [connected]);

  // 独立工具:卸载即整体断开(会话随之释放,设备 coast 后自锁保持高度)。
  useEffect(() => () => { api.zliftDisconnect().catch(() => {}); }, []);

  const connect = useCallback(async () => {
    setBusy(true);
    try {
      await api.zliftConnect(endpoint.trim());
      setConnected(true);
      let list = await api.zliftDiscover();
      if (!list.length) { await new Promise((r) => setTimeout(r, 900)); list = await api.zliftDiscover(); }
      setLifts(list);
      const first = list[0]?.prefix ?? null;
      setSelected(first);
      if (first) await api.zliftSetFocus(first).catch(() => {});
      if (!list.length) message.warning(t("liftNoneFound"));
    } catch (e) { message.error(errMsg(e)); }
    finally { setBusy(false); }
  }, [endpoint, message, t]);

  const disconnect = useCallback(async () => {
    try { await api.zliftDisconnect(); } catch { /* ignore */ }
    setConnected(false); setLifts([]); setSelected(null); setSt(null);
  }, []);

  const onSelect = useCallback(async (prefix: string) => {
    setSelected(prefix);
    await api.zliftSetFocus(prefix).catch(() => {});
  }, []);

  const acquire = useCallback(async () => {
    const l = lifts.find((x) => x.prefix === selected);
    if (!l) return;
    try { await api.zliftAcquire(l.prefix, l.model); message.success(t("liftAcquired")); }
    catch (e) { message.error(errMsg(e)); }
  }, [lifts, selected, message, t]);

  const release = useCallback(async () => {
    try { await api.zliftRelease(); } catch (e) { message.error(errMsg(e)); }
  }, [message]);

  const home = useCallback(async () => {
    try { await api.zliftHome(); message.info(t("liftHomingStarted")); }
    catch (e) { message.error(errMsg(e)); }
  }, [message, t]);

  const goto_ = useCallback(async (h: number) => {
    try { await api.zliftGoto(h); } catch (e) { message.error(errMsg(e)); }
  }, [message]);

  // 点动:按住发速度、松开发停车。松手一定要停 —— 控制器 250ms 不刷新虽然也会停,
  // 但那是兜底,不是正常路径。
  const jogStart = useCallback(async (dir: 1 | -1) => {
    try { await api.zliftJog(dir * jogSpeed); } catch (e) { message.error(errMsg(e)); }
  }, [jogSpeed, message]);
  const jogStop = useCallback(async () => {
    try { await api.zliftJog(null); } catch { /* 松手停车尽力而为 */ }
  }, []);

  const controlling = !!st?.controlling;
  const homed = !!st?.homed;
  const posMin = st?.pos_min ?? 0;
  const posMax = st?.pos_max ?? 0.7;
  // 未回零 / 配置无效 / 急停 ⇒ 设备一定拒绝运动,控件直接禁用而不是让用户撞错误。
  const canMove = controlling && homed && !!st?.config_valid && !st?.estop && !st?.fatal;

  return (
    <Space direction="vertical" size={16} style={{ width: "100%", maxWidth: 1000 }}>
      <Card size="small" className="app-command-card">
        <Space wrap>
          <Typography.Text>Endpoint</Typography.Text>
          <Input
            style={{ width: 240 }} value={endpoint} disabled={connected}
            placeholder={t("liftEndpointHint")}
            onChange={(e) => setEndpoint(e.target.value)}
          />
          {connected
            ? <Button onClick={disconnect}>{t("liftDisconnect")}</Button>
            : <Button type="primary" loading={busy} onClick={connect}>{t("liftConnect")}</Button>}
          {connected && (
            // 取控期间锁定选择:否则观察焦点会漂到另一台,而命令仍发往原机器。
            <Select
              style={{ width: 320 }} value={selected ?? undefined} onChange={onSelect}
              disabled={controlling}
              options={lifts.map((l) => ({ value: l.prefix, label: `${l.model} — ${l.prefix}` }))}
            />
          )}
          {connected && (controlling
            ? <Button danger onClick={release}>{t("liftRelease")}</Button>
            : <Button type="primary" disabled={!selected || (!!st && st.holder !== 0)} onClick={acquire}>
                {st && st.holder !== 0 ? `${t("liftBusy")} #${st.holder}` : t("liftAcquire")}
              </Button>)}
        </Space>
      </Card>

      {connected && <FaultAlert fatal={!!st?.fatal} controlling={controlling} onClear={api.zliftClearFault} />}

      {/* 未回零是最常见的"为什么动不了",给一条明确指引而不是让用户去点被禁用的滑条。 */}
      {connected && controlling && !homed && (
        <Alert
          type="warning" showIcon
          message={t("liftNotHomedTitle")}
          description={t("liftNotHomedDesc")}
          action={<Button type="primary" loading={!!st?.homing} onClick={home}>{t("liftHome")}</Button>}
        />
      )}

      {connected && (
        <Card size="small">
          <Space direction="vertical" size={12} style={{ width: "100%" }}>
            <Space wrap>
              {controlling ? <Tag color="green">{t("liftControlling")}</Tag>
                : st && st.holder !== 0 ? <Tag color="orange">{t("liftBusy")} #{st.holder}</Tag>
                : <Tag>{t("liftReadOnly")}</Tag>}
              <RobotModeTag mode={st?.robot_mode} />
              {st && <StatusTags st={st} />}
              {st?.homing && <Tag color="processing">{t("liftHoming")}</Tag>}
            </Space>

            <Descriptions size="small" column={4} bordered>
              <Descriptions.Item label={t("liftHeight")}>
                <b>{(st?.height ?? 0).toFixed(4)} m</b>
              </Descriptions.Item>
              <Descriptions.Item label={t("liftSoftLimits")}>
                {posMin.toFixed(3)} ~ {posMax.toFixed(3)} m
              </Descriptions.Item>
              <Descriptions.Item label={t("liftVelMax")}>
                {(st?.vel_max ?? 0).toFixed(3)} m/s
              </Descriptions.Item>
              <Descriptions.Item label={t("liftPayload")}>
                {st?.payload_max_kg != null ? `${st.payload_max_kg} kg` : "—"}
              </Descriptions.Item>
            </Descriptions>

            <Space wrap>
              <Button onClick={home} loading={!!st?.homing} disabled={!controlling}>
                {t("liftHome")}
              </Button>
              <Button onClick={() => api.zliftSetMode(1).catch((e) => message.error(errMsg(e)))}
                      disabled={!controlling}>
                {t("liftApiDisable")}
              </Button>
              {/* 守卫接触在本型号物理上做不到(拿不到可信力估计),如实说明而不是给个没用的开关 */}
              {st && !st.guarded_contact_supported && (
                <Typography.Text type="secondary">{t("liftNoGuardedContact")}</Typography.Text>
              )}
            </Space>
          </Space>
        </Card>
      )}

      {connected && st?.can_position && (
        <Card size="small" title={t("liftGotoTitle")}>
          <Slider
            min={posMin} max={posMax} step={0.001} value={target}
            disabled={!canMove}
            tooltip={{ formatter: (v) => `${(v ?? 0).toFixed(3)} m` }}
            onChange={(v: number) => { dragging.current = true; setTarget(v); }}
            // 位置是自主 goal:只在松手时下发一次,拖动过程中连发会不断打断设备自己的规划。
            onChangeComplete={(v: number) => { dragging.current = false; goto_(v); }}
          />
          <Space wrap>
            <InputNumber
              min={posMin} max={posMax} step={0.005} value={target} disabled={!canMove}
              onChange={(v) => setTarget(v ?? 0)} addonAfter="m"
            />
            <Button type="primary" disabled={!canMove} onClick={() => goto_(target)}>
              {t("liftGoto")}
            </Button>
            <Typography.Text type="secondary">{t("liftGotoHint")}</Typography.Text>
          </Space>
        </Card>
      )}

      {connected && st?.can_velocity && (
        <Card size="small" title={t("liftJogTitle")}>
          <Space wrap>
            <Typography.Text>{t("liftJogSpeed")}</Typography.Text>
            <InputNumber
              // 下限取设备的释放死区:更小的值设备只会脱力滑行,发了也不动。
              min={st.vel_min} max={st.vel_max} step={0.001}
              value={jogSpeed} disabled={!canMove}
              onChange={(v) => setJogSpeed(v ?? st.vel_min)} addonAfter="m/s"
            />
            <Button
              disabled={!canMove}
              onMouseDown={() => jogStart(1)} onMouseUp={jogStop} onMouseLeave={jogStop}
              onTouchStart={() => jogStart(1)} onTouchEnd={jogStop}
            >
              ▲ {t("liftJogUp")}
            </Button>
            <Button
              disabled={!canMove}
              onMouseDown={() => jogStart(-1)} onMouseUp={jogStop} onMouseLeave={jogStop}
              onTouchStart={() => jogStart(-1)} onTouchEnd={jogStop}
            >
              ▼ {t("liftJogDown")}
            </Button>
            <Typography.Text type="secondary">{t("liftApiJogHint")}</Typography.Text>
          </Space>
        </Card>
      )}
    </Space>
  );
}

export default LiftRobotPanel;
