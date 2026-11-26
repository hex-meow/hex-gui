import { useEffect, useMemo, useState } from "react";
import { Button, Slider } from "antd";
import { ArmViewer } from "./ArmViewer";
import { createRawArmViewGuides } from "./rawArmViewGuides";
import { useI18n } from "../i18n";
import { samplePreview, type ArmFeedback, type ArmPreview, type RawModel } from "../rawArm";

const gravity: [number, number, number] = [0, 0, -9.80665];
export function RawArmPreview({ model, xml, preview, feedback, toolTarget, onReady }: {
  model: RawModel; xml: string; preview: ArmPreview | null; feedback: ArmFeedback | null;
  toolTarget: number | null; onReady: (ready: boolean) => void;
}) {
  const { lang } = useI18n();
  const zh = lang === "zh";
  const [fraction, setFraction] = useState(0);
  const [play, setPlay] = useState(0);
  const [scrubbing, setScrubbing] = useState(false);
  useEffect(() => { setScrubbing(false); }, [preview]);
  useEffect(() => {
    if (!preview || scrubbing) return;
    const start = performance.now();
    let timer = 0;
    const frame = () => {
      const p = preview.duration > 0 ? Math.min(1, (performance.now() - start) / (1000 * preview.duration)) : 1;
      setFraction(p);
      if (p < 1) timer = window.setTimeout(frame, 32);
    };
    frame();
    return () => window.clearTimeout(timer);
  }, [preview, play, scrubbing]);
  const names = useMemo(() => [...model.joints.map(j => j.name), ...(model.tool ? [`ee_${model.tool.joint.name}`] : [])], [model]);
  const measured = feedback?.tool?.position ?? 0;
  const ghost = preview ? [...samplePreview(preview, fraction), ...(model.tool ? [toolTarget ?? measured] : [])] : null;
  return <>
    <div className="raw-arm__viewport">
      <ArmViewer jointNames={names} q={feedback ? [...feedback.q, ...(model.tool ? [measured] : [])] : []}
        showActual={!!feedback} measuredJoints gravity={gravity} urdfXml={xml}
        previewQ={ghost} tcpPath={preview?.path} createOverlay={createRawArmViewGuides} onModelReady={onReady} />
      <span className="raw-arm__axes-caption">{zh ? "基座坐标系" : "Base frame"}</span>
      {feedback && <div className="raw-arm__readout">
        <div>{zh ? "实测关节角" : "Measured joints"} · rad</div>
        <dl>{model.joints.map((joint, i) => (
          <div key={joint.name}><dt>J{i + 1}</dt><dd>{feedback.q[i]?.toFixed(3) ?? "—"}</dd></div>
        ))}</dl>
      </div>}
    </div>
    <div className="raw-arm__playback">
      <Button size="small" disabled={!preview} onClick={() => { setScrubbing(false); setPlay(n => n + 1); }}>
        {zh ? "重播虚影" : "Replay ghost"}
      </Button>
      <Slider min={0} max={1} step={0.002} value={preview ? fraction : 0} disabled={!preview}
        tooltip={{ formatter: v => `${((v ?? 0) * (preview?.duration ?? 0)).toFixed(2)} s` }}
        ariaLabelForHandle={zh ? "预览进度" : "Preview time"}
        onChange={v => { setScrubbing(true); setFraction(v); }} />
      <span>{preview ? `${(preview.duration * fraction).toFixed(2)} / ${preview.duration.toFixed(2)} s` : "—"}</span>
    </div>
  </>;
}
