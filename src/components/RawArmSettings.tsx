import { Button, Collapse, InputNumber, Select } from "antd";
import { useEffect, useState, type ReactNode } from "react";
import { useI18n } from "../i18n";
import type { ArmGains, FactoryReading, GoalTolerance, Installation, RawArmPreferences, ToolKind } from "../rawArm";

function isTorqueLimit(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 1000;
}

function isTolerance(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

export function replaceAt<T>(values: T[], index: number, value: T): T[] {
  return values.map((current, i) => i === index ? value : current);
}

function GoalToleranceControls({ value, disabled, onApply }: {
  value: GoalTolerance; disabled: boolean; onApply: (value: GoalTolerance) => void;
}) {
  const zh = useI18n().lang === "zh";
  const [position, setPosition] = useState<number | null>(value.position_rad);
  const [velocity, setVelocity] = useState<number | null>(value.velocity_rad_s);
  useEffect(() => {
    setPosition(value.position_rad);
    setVelocity(value.velocity_rad_s);
  }, [value.position_rad, value.velocity_rad_s]);
  const valid = isTolerance(position) && isTolerance(velocity);
  const changed = position !== value.position_rad || velocity !== value.velocity_rad_s;
  return <>
    <div className="raw-arm__fields">
      <label>{zh ? "关节位置误差上限" : "Joint position error limit"} · rad
        <InputNumber value={position} disabled={disabled} step={0.001}
          status={isTolerance(position) ? undefined : "error"} onChange={setPosition} /></label>
      <label>{zh ? "到位速度阈值" : "Joint speed at arrival"} · rad/s
        <InputNumber value={velocity} disabled={disabled} step={0.01}
          status={isTolerance(velocity) ? undefined : "error"} onChange={setVelocity} /></label>
      <Button size="small" disabled={disabled || !valid || !changed}
        onClick={() => { if (valid) onApply({ position_rad: position, velocity_rad_s: velocity }); }}>
        {zh ? "应用阈值" : "Apply thresholds"}
      </Button>
    </div>
    <p>{zh ? "轨迹结束后，六个关节的实测位置误差和速度绝对值都在各自阈值内时，显示已到位。" :
      "After the trajectory ends, the arm is at its goal when every joint’s measured position error and absolute speed are within these limits."}</p>
  </>;
}

function factoryValue(reading: FactoryReading | undefined, zh: boolean) {
  if (!reading) return "—";
  if (reading.state === "valid") return String(reading.factor);
  if (reading.state === "missing") return zh ? "缺失（默认 1.0）" : "Missing (default 1.0)";
  return <>
    <span>{zh ? "读取失败" : "Read failed"} · {reading.error}</span>
    {reading.last_valid !== null && <span>{zh ? "上次成功值" : "Last valid"} · {reading.last_valid}</span>}
  </>;
}

export function RawArmSettings({ prefs, change, connected, toolKind, factory, canReread, readingNode, reread, canAlign, align, applyOffsets, gravityControls, canEditGoalTolerance, applyGoalTolerance }: {
  prefs: RawArmPreferences; change: (p: RawArmPreferences) => void; connected: boolean; toolKind: ToolKind | null;
  factory: FactoryReading[]; canReread: boolean; readingNode: number | null; reread: (nodeId: number) => void;
  canAlign: boolean; align: () => void; applyOffsets: (offset: number[]) => void;
  gravityControls: ReactNode; canEditGoalTolerance: boolean; applyGoalTolerance: (value: GoalTolerance) => void;
}) {
  const { lang } = useI18n();
  const zh = lang === "zh";
  const { limits, gains, installation } = prefs;
  const [offsets, setOffsets] = useState(installation.offset);
  useEffect(() => { setOffsets(installation.offset); }, [installation.offset, connected]);
  const setInstall = (patch: Partial<Installation>) => change({ ...prefs, installation: { ...installation, ...patch } });
  const setGains = (patch: Partial<ArmGains>) => change({ ...prefs, gains: { ...gains, ...patch } });
  const fields = [
    ["velocity", zh ? "速度" : "Speed", "rad/s", 0.1],
    ["acceleration", zh ? "加速度" : "Acceleration", "rad/s²", 0.1],
    ["jerk", zh ? "加加速度" : "Jerk", "rad/s³", 0.5],
  ] as const;
  return <section className="raw-arm__settings">
    <h3>{zh ? "运动参数" : "Motion parameters"}</h3>
    <div className="raw-arm__fields">
      {fields.map(([key, label, unit, step]) => <label key={key}>{label} · {unit}
        <InputNumber aria-label={label} value={limits[key]} step={step}
          onChange={v => { if (v !== null) change({ ...prefs, limits: { ...limits, [key]: v } }); }} />
      </label>)}
      <label>{zh ? "拖动阻尼" : "Hand-guiding damping"} · N·m·s/rad
        <InputNumber value={gains.damping} min={0} step={0.01} onChange={v => { if (v !== null) setGains({ damping: v }); }} />
      </label>
    </div>
    <Collapse size="small" items={[{
      key: "goalTolerance", label: zh ? "到位判定阈值" : "Arrival thresholds",
      children: <GoalToleranceControls value={prefs.goalTolerance} disabled={!canEditGoalTolerance} onApply={applyGoalTolerance} />,
    }, {
      key: "torque", label: zh ? "各轴电机输出力矩上限" : "Motor output torque limits",
      children: <>
        <div className="raw-arm__fields">
          {installation.max_torque_permille.map((value, i) => <label key={i}>J{i + 1} · ‰
            <InputNumber aria-label={`J${i + 1} ${zh ? "电机输出力矩上限" : "motor output torque limit"} (‰)`}
              disabled={connected} size="small" min={0} max={1000} step={1} precision={0}
              placeholder={zh ? "沿用当前值" : "Keep current"} value={value}
              onChange={v => { if (v === null || isTorqueLimit(v)) setInstall({ max_torque_permille: replaceAt(installation.max_torque_permille, i, v) }); }} />
          </label>)}
        </div>
        <p>{zh ? "输入 0–1000 的整数，1000 ‰ 表示该轴电机的峰值力矩。连接并初始化时写入，留空的轴沿用电机当前上限。" :
          "Enter a whole number from 0 to 1000. A value of 1000 ‰ is the motor’s peak torque. Connect and initialize to apply these limits. Leave an axis blank to keep its current limit."}</p>
      </>,
    }, {
      key: "axes", label: zh ? "关节增益与安装标定" : "Joint gains and installation calibration",
      children: <>
        <div className="raw-arm__table-scroll"><table><thead><tr>
          <th>{zh ? "轴" : "Axis"}</th><th>Kp · N·m/rad</th><th>Kd · N·m·s/rad</th>
          <th>{zh ? "方向" : "Sign"}</th><th>{zh ? "零偏" : "Offset"} · rad</th>
          <th>{zh ? "力矩系数覆盖" : "Torque factor override"}</th>
          <th>{zh ? "出厂力矩系数" : "Factory torque factor"}</th>
        </tr></thead><tbody>{installation.direction.map((d, i) => <tr key={i}>
          <th>J{i + 1}</th>
          {(["kp", "kd"] as const).map(k => <td key={k}><InputNumber aria-label={`J${i + 1} ${k}`} size="small" min={0} step={k === "kp" ? 1 : 0.1}
            value={gains[k][i]} onChange={v => { if (v !== null) setGains({ [k]: replaceAt(gains[k], i, v) }); }} /></td>)}
          <td><Select aria-label={`J${i + 1} sign`} disabled={connected} size="small" value={d}
            options={[{ value: 1, label: "+1" }, { value: -1, label: "−1" }]}
            onChange={v => setInstall({ direction: replaceAt(installation.direction, i, v) })} /></td>
          <td><InputNumber aria-label={`J${i + 1} offset`} disabled={connected && !canAlign} size="small" step={0.001} value={(connected ? offsets : installation.offset)[i]}
            onChange={v => { if (v !== null) {
              if (connected) setOffsets(replaceAt(offsets, i, v));
              else setInstall({ offset: replaceAt(installation.offset, i, v) });
            } }} /></td>
          <td><InputNumber aria-label={`J${i + 1} torque factor`} disabled={connected} size="small" step={0.01} value={installation.factor[i]}
            placeholder={zh ? "采用出厂值" : "Use factory"}
            onChange={v => setInstall({ factor: replaceAt(installation.factor, i, v) })} /></td>
          <td><div className="raw-arm__factory-reading">
            {factoryValue(factory[i], zh)}
            <Button size="small" aria-label={`J${i + 1} ${zh ? "重新读取" : "reread calibration"}`}
              disabled={!canReread} loading={readingNode === i + 1} onClick={() => reread(i + 1)}>
              {zh ? "重新读取" : "Reread"}
            </Button>
          </div></td>
        </tr>)}</tbody></table></div>
        <p>{zh ? "启用位置保持或拖动时应用增益。未填写覆盖值的轴使用出厂力矩系数，电机确认缺少标定时使用 1.0。零偏按本机安装标定。" :
          "Gains take effect when entering hold or hand-guiding. An axis with no override uses its factory torque factor, or 1.0 if the motor reports missing calibration. Offsets are installation-specific."}</p>
        <div className="raw-arm__actions"><Button size="small" disabled={!canAlign} onClick={align}>{zh ? "以当前折叠姿态计算六轴零偏" : "Set joint offsets from the current folded pose"}</Button>
          {connected && offsets.some((v, i) => v !== installation.offset[i]) && <Button size="small" disabled={!canAlign} onClick={() => applyOffsets(offsets)}>
            {zh ? "应用编辑的零偏" : "Apply edited offsets"}
          </Button>}
        </div>
        <p>{zh ? "仅在实际姿态为 [0, −1.57, 3.14, 0, 0, 0] rad 时使用。此操作只更新上位机坐标。" :
          "Use at the physical pose [0, −1.57, 3.14, 0, 0, 0] rad. This updates the host coordinates."}</p>
        {toolKind === "gp80" && <div className="raw-arm__fields">
          <label>{zh ? "GP80 单指位移" : "GP80 single-finger travel"} · mm/rev
            <InputNumber disabled={connected} value={installation.finger_m_per_rev * 1000} step={0.1}
              onChange={v => { if (v !== null) setInstall({ finger_m_per_rev: v / 1000 }); }} /></label>
          <label>{zh ? "单指零偏" : "Finger offset"} · mm
            <InputNumber disabled={connected} value={installation.finger_offset_m * 1000} step={0.1}
              onChange={v => { if (v !== null) setInstall({ finger_offset_m: v / 1000 }); }} /></label>
        </div>}
        {toolKind === "gr80" && <div className="raw-arm__fields">
          <label>{zh ? "GR80 单爪转角" : "GR80 single-jaw travel"} · rad/rev
            <InputNumber disabled={connected} value={installation.jaw_rad_per_rev} step={0.01}
              onChange={v => { if (v !== null) setInstall({ jaw_rad_per_rev: v }); }} /></label>
          <label>{zh ? "单爪零偏" : "Jaw offset"} · rad
            <InputNumber disabled={connected} value={installation.jaw_offset_rad} step={0.001}
              onChange={v => { if (v !== null) setInstall({ jaw_offset_rad: v }); }} /></label>
        </div>}
      </>,
    }, {
      key: "gravity", label: zh ? "重力前馈补偿系数" : "Gravity feedforward scales",
      children: gravityControls,
    }]} />
  </section>;
}
