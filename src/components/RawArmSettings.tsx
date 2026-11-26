import { Button, Collapse, InputNumber, Select } from "antd";
import { useEffect, useState, type ReactNode } from "react";
import { useI18n } from "../i18n";
import { defaultGains, defaultLimits, referenceInstallation, type ArmGains, type Installation, type MotionLimits, type ToolKind } from "../rawArm";

export interface Preferences { installation: Installation; gains: ArmGains; limits: MotionLimits }
export const preferenceKey = "hex-gui.firefly-raw-can.v1";

export function readPreferences(): Preferences {
  try {
    const p: Preferences = JSON.parse(localStorage.getItem(preferenceKey) ?? "null");
    const six = (a: unknown) => Array.isArray(a) && a.length === 6 && a.every(Number.isFinite);
    // Keep the installation and gains saved by the GP80-only panel.
    const i = { ...referenceInstallation, ...p.installation };
    if (six(i.direction) && i.direction.every(d => d === 1 || d === -1) && six(i.offset)
      && i.factor.length === 6 && i.factor.every(f => f === null || (Number.isFinite(f) && f > 0))
      // The selected encoder is validated on connect. A zero scale entered
      // for one tool must not discard the other tool's or the arm's calibration.
      && [i.finger_m_per_rev, i.finger_offset_m, i.jaw_rad_per_rev, i.jaw_offset_rad].every(Number.isFinite)
      && six(p.gains.kp) && six(p.gains.kd) && Number.isFinite(p.gains.damping)
      && [p.limits.velocity, p.limits.acceleration, p.limits.jerk].every(v => Number.isFinite(v) && v > 0)) return { ...p, installation: i };
  } catch { /* A first run or an older settings layout uses the product defaults. */ }
  return { installation: referenceInstallation, gains: defaultGains, limits: defaultLimits };
}

export function replaceAt<T>(values: T[], index: number, value: T): T[] {
  return values.map((current, i) => i === index ? value : current);
}

export function RawArmSettings({ prefs, change, connected, toolKind, factory, canAlign, align, applyOffsets, gravityControls }: {
  prefs: Preferences; change: (p: Preferences) => void; connected: boolean; toolKind: ToolKind | null;
  factory: (number | null)[]; canAlign: boolean; align: () => void; applyOffsets: (offset: number[]) => void;
  gravityControls: ReactNode;
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
        <InputNumber value={gains.damping} min={0} step={0.1} onChange={v => { if (v !== null) setGains({ damping: v }); }} />
      </label>
    </div>
    <Collapse size="small" items={[{
      key: "axes", label: zh ? "关节增益与安装标定" : "Joint gains and installation calibration",
      children: <>
        <div className="raw-arm__table-scroll"><table><thead><tr>
          <th>{zh ? "轴" : "Axis"}</th><th>Kp · N·m/rad</th><th>Kd · N·m·s/rad</th>
          <th>{zh ? "方向" : "Sign"}</th><th>{zh ? "零偏" : "Offset"} · rad</th>
          <th>{zh ? "力矩系数覆盖" : "Torque factor override"}</th>
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
            placeholder={factory[i]?.toFixed(3) ?? (connected ? "1.000" : (zh ? "出厂" : "Factory"))}
            onChange={v => setInstall({ factor: replaceAt(installation.factor, i, v) })} /></td>
        </tr>)}</tbody></table></div>
        <p>{zh ? "启用位置保持或拖动时应用增益。力矩默认采用电机出厂系数，缺失时使用 1.0。零偏按本机安装标定。" :
          "Gains take effect when entering hold or hand-guiding. Torque uses the factory factor when available and 1.0 otherwise. Offsets are installation-specific."}</p>
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
