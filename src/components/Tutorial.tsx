import { useState } from "react";
import { Carousel, Modal, Typography, theme } from "antd";
import { useI18n } from "../i18n";

// A swipe-through getting-started guide shown from the tool picker.
//
// To customize: drop screenshots or short screen-recordings into
// `public/tutorial/` and point each slide's `media` at them, e.g.
//   media: { type: "image", src: "/tutorial/01-connect.png" }
//   media: { type: "video", src: "/tutorial/02-drive.mp4" }
// Files in `public/` are served from the site root, so the leading "/" is the
// project's `public/` folder. Slides with no `media` just render their text.
// Edit, reorder, or add to SLIDES freely — both languages live inline.
interface Slide {
  media?: { type: "image" | "video"; src: string };
  title: { en: string; zh: string };
  body: { en: string; zh: string };
}

const HOME_SLIDES: Slide[] = [
  {
    media: { type: "image", src: "/tutorial/01-connect.png" },
    title: { en: "1 · Connect", zh: "1 · 连接" },
    body: {
      en: "Pick your CAN interface in the top bar, then press Connect. Linux defaults to can0; macOS / Windows default to gs_usb0 (a USB candleLight adapter).",
      zh: "在顶部选择 CAN 接口，然后点击「连接」。Linux 默认 can0；macOS / Windows 默认 gs_usb0（USB candleLight 适配器）。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/02-select.png" },
    title: { en: "2 · Pick a motor", zh: "2 · 选择电机" },
    body: {
      en: "Discovered motors appear in the left sidebar. Click one to open its live panel, charts, and controls.",
      zh: "发现的电机会出现在左侧栏。点击任意一个即可打开它的实时面板、图表和控制区。",
    },
  },
  {
    media: { type: "video", src: "/tutorial/03-drive.mp4" },
    title: { en: "3 · Drive & chart", zh: "3 · 控制与绘图" },
    body: {
      en: "Set a mode and target to drive the motor. Switch the display to Chart to watch position, velocity and torque. Use the Refresh High/Low toggle if the chart feels heavy.",
      zh: "设置模式和目标值来驱动电机。把显示切到「图表」即可观察位置、速度和力矩。如果图表卡顿，用「刷新率 高/低」开关调节。",
    },
  },
  {
    title: { en: "4 · Record full-rate data", zh: "4 · 记录全速率数据" },
    body: {
      en: "The on-screen chart is downsampled for smoothness. For the full ~1000 Hz stream, flip the Record CSV switch — it logs every frame to a file untouched by the UI.",
      zh: "屏幕上的图表为了流畅做了降采样。需要完整的 ~1000 Hz 数据流时，打开「记录 CSV」开关——它会把每一帧原样写入文件，不受界面影响。",
    },
  },
];

// Placeholder slides for a per-app tutorial that hasn't been written yet.
// Each slide's media already points at `public/tutorial/<tool>/0N.png`, so
// dropping a screenshot (or renaming to .mp4 and adjusting the type) there is
// all it takes to fill a step in — then replace the step's body text. Add or
// remove steps by changing `count`.
function placeholderSlides(tool: string, count = 3): Slide[] {
  return Array.from({ length: count }, (_, i) => {
    const n = i + 1;
    return {
      media: { type: "image", src: `/tutorial/${tool}/0${n}.png` },
      title: { en: `Step ${n}`, zh: `步骤 ${n}` },
      body: {
        en: "(Describe this step, then drop a screenshot into public/tutorial/ to replace this placeholder.)",
        zh: "（在此描述该步骤，并把截图放到 public/tutorial/ 目录以替换此占位。）",
      },
    };
  });
}

// Slide sets keyed by tool id (matching App's Tool union, plus "home" for the
// landing-page guide). Each app starts with blank placeholder steps.
export const TUTORIALS: Record<string, Slide[]> = {
  home: HOME_SLIDES,
  control: placeholderSlides("control"),
  changeId: placeholderSlides("changeId"),
  zero: placeholderSlides("zero"),
  hopea3: placeholderSlides("hopea3"),
  smartknob: placeholderSlides("smartknob"),
  zenoh: placeholderSlides("zenoh"),
  arm: placeholderSlides("arm"),
  canalyzer: placeholderSlides("canalyzer"),
};

// Renders the slide's image/video, falling back to the placeholder caption if
// the file is missing (so it looks intentional before real media is dropped in).
function SlideMedia({ media }: { media?: Slide["media"] }) {
  const { t } = useI18n();
  const [failed, setFailed] = useState(false);

  if (media && !failed) {
    if (media.type === "image") {
      return (
        <img
          src={media.src}
          alt=""
          onError={() => setFailed(true)}
          style={{ maxWidth: "100%", maxHeight: "100%", objectFit: "contain" }}
        />
      );
    }
    return (
      <video
        src={media.src}
        controls
        onError={() => setFailed(true)}
        style={{ maxWidth: "100%", maxHeight: "100%" }}
      />
    );
  }
  return (
    <Typography.Text type="secondary">{t("tutorialMediaPlaceholder")}</Typography.Text>
  );
}

export function TutorialModal({
  open,
  onClose,
  title,
  slides,
}: {
  open: boolean;
  onClose: () => void;
  // Defaults to the landing-page "Getting started" guide when omitted.
  title?: string;
  slides?: Slide[];
}) {
  const { t, lang } = useI18n();
  const { token } = theme.useToken();
  const list = slides ?? HOME_SLIDES;

  return (
    <Modal
      open={open}
      onCancel={onClose}
      footer={null}
      width={640}
      centered
      title={title ?? t("tutorialTitle")}
    >
      <Carousel arrows draggable adaptiveHeight style={{ paddingBottom: 24 }}>
        {list.map((s, i) => (
          <div key={i}>
            <div style={{ padding: "8px 32px 0" }}>
              <div
                style={{
                  height: 280,
                  borderRadius: token.borderRadiusLG,
                  overflow: "hidden",
                  background: token.colorFillTertiary,
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  marginBottom: 16,
                }}
              >
                <SlideMedia media={s.media} />
              </div>
              <Typography.Title level={5} style={{ marginTop: 0 }}>
                {s.title[lang]}
              </Typography.Title>
              <Typography.Paragraph type="secondary" style={{ marginBottom: 0 }}>
                {s.body[lang]}
              </Typography.Paragraph>
            </div>
          </div>
        ))}
      </Carousel>
    </Modal>
  );
}
