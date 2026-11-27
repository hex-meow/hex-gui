import { Button, InputNumber, Slider, Space } from "antd";
import { useI18n } from "../i18n";
import { formatMeowModeDisplay } from "../meowMotorUi";
import { gripperUnits, type GripperMotion, type MountedTool, type ToolFeedback } from "../rawArm";

export interface GripperSettings {
  speed: number | null;
  acceleration: number | null;
  torque: number | null;
}

interface Props {
  tool: MountedTool;
  feedback: ToolFeedback | null;
  target: number | null;
  setTarget: (position: number | null) => void;
  settings: GripperSettings;
  setSettings: (settings: GripperSettings) => void;
  canMove: boolean;
  canDisable: boolean;
  move: (motion: GripperMotion) => void;
  disable: () => void;
}

export function RawArmGripper({ tool, feedback, target, setTarget, settings, setSettings, canMove, canDisable, move, disable }: Props) {
  const { lang } = useI18n();
  const zh = lang === "zh";
  const rotary = tool.kind === "gr80";
  const { displayPerJoint, siPerDisplay, unit, step } = gripperUnits[tool.kind];
  const { lower, upper } = tool.joint;
  const { speed, acceleration, torque } = settings;
  const label = rotary ? (zh ? "单爪角" : "jaw angle") : (zh ? "开口全宽" : "full opening");
  const valid = target !== null && target >= lower && target <= upper
    && speed !== null && Number.isFinite(speed) && speed > 0
    && acceleration !== null && Number.isFinite(acceleration) && acceleration > 0
    && torque !== null && Number.isInteger(torque) && torque >= 0 && torque <= 1000;

  function execute() {
    if (!canMove || !valid) return;
    move({
      kind: tool.kind,
      target: target === lower ? "closed" : target === upper ? "open" : { position: target * displayPerJoint * siPerDisplay },
      speed: speed * siPerDisplay,
      acceleration: acceleration * siPerDisplay,
      max_torque_permille: torque,
    });
  }

  const format = (v: number) => (v * displayPerJoint).toFixed(rotary ? 3 : 1);
  return <section>
    <h3>{tool.kind.toUpperCase()} · {zh ? "夹爪控制" : "Gripper control"}</h3>
    <div className="raw-arm__detail">
      {zh ? "实测" : "Measured "}{label}　{feedback ? `${format(feedback.position)} ${unit}` : "—"}
      {feedback?.velocity != null && <>　{format(feedback.velocity)} {unit}/s</>}
      {feedback?.torque_permille != null && <>　{zh ? "电机转矩" : "Motor torque"} {feedback.torque_permille} ‰</>}
      {feedback?.mode_display != null && <>　{formatMeowModeDisplay(feedback.mode_display, lang)}</>}
    </div>
    <div className="raw-arm__fields">
      <label>{zh ? "目标" : "Target "}{label} · {unit}
        <InputNumber value={target === null ? null : target * displayPerJoint} step={step}
          min={lower * displayPerJoint} max={upper * displayPerJoint}
          onChange={v => setTarget(v === null ? null : v / displayPerJoint)} /></label>
      <Button size="small" onClick={() => setTarget(lower)}>{zh ? "全闭" : "Closed"}</Button>
      <Button size="small" onClick={() => setTarget(upper)}>{zh ? "全开" : "Open"}</Button>
    </div>
    <Slider ariaLabelForHandle={label} min={lower * displayPerJoint} max={upper * displayPerJoint} step={step}
      value={(target ?? feedback?.position ?? lower) * displayPerJoint} onChange={v => setTarget(v / displayPerJoint)} />
    <div className="raw-arm__fields">
      <label>{zh ? "速度" : "Speed"} · {unit}/s
        <InputNumber value={speed} step={step} min={0} onChange={v => setSettings({ ...settings, speed: v })} /></label>
      <label>{zh ? "加速度" : "Acceleration"} · {unit}/s²
        <InputNumber value={acceleration} step={step} min={0} onChange={v => setSettings({ ...settings, acceleration: v })} /></label>
      <label>{zh ? "电机转矩上限" : "Motor torque limit"} · ‰
        <InputNumber value={torque} step={1} precision={0} min={0} max={1000} onChange={v => setSettings({ ...settings, torque: v })} /></label>
    </div>
    <Space wrap>
      <Button type="primary" disabled={!canMove || !valid} onClick={execute}>
        {zh ? "执行夹爪目标" : "Execute gripper target"}
      </Button>
      <Button disabled={!canDisable} onClick={disable}>{zh ? "夹爪失能" : "Disable gripper"}</Button>
    </Space>
  </section>;
}
