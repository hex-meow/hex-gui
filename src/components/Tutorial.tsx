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

// Per-app tutorials migrated from the geek-docs "上位机的使用" guide.
// Screenshots live under public/tutorial/<tool>/0N.png.
const CHANGE_ID_SLIDES: Slide[] = [
  {
    media: { type: "image", src: "/tutorial/changeId/01.png" },
    title: { en: "1 · Open Change ID", zh: "1 · 打开 Change ID" },
    body: {
      en: "From the tool picker, open the Change ID app.",
      zh: "在工具选择界面点击 Change ID（修改 ID）方框，进入该工具。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/changeId/02.png" },
    title: { en: "2 · Connect", zh: "2 · 连接" },
    body: {
      en: "Press Connect. The input box to the left of the button is this host's NodeID — any value works as long as it doesn't clash with a motor's NodeID.",
      zh: "点击 Connect 连接。按钮左边的输入框是上位机自身的 NodeID，只要不与电机的 NodeID 冲突即可。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/changeId/03.png" },
    title: { en: "3 · Pick the motor", zh: "3 · 选择电机" },
    body: {
      en: "Detected motors appear in the left list. Click the one whose ID you want to change.",
      zh: "左侧列表会显示识别到的电机，点击你要修改的那台。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/changeId/04.png" },
    title: { en: "4 · Write & Save", zh: "4 · 写入并保存" },
    body: {
      en: "Enter the new ID in the New ID box, then press Write & Save. Power-cycle the motor and it reappears in the list with its new ID.",
      zh: "在 New ID 框中输入新的 ID，点击 Write & Save。写入后给电机重新上电，它会以新 ID 重新出现在下方列表里。",
    },
  },
];

const CONTROL_SLIDES: Slide[] = [
  {
    media: { type: "image", src: "/tutorial/control/01.png" },
    title: { en: "1 · Open Motor Control", zh: "1 · 打开 Motor Control" },
    body: {
      en: "Once wiring is done, open the Motor Control app from the tool picker.",
      zh: "接好线后，在工具选择界面点击 Motor Control 方框进入。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/control/02.png" },
    title: { en: "2 · Connect", zh: "2 · 连接" },
    body: {
      en: "In the Motor Control view, press Connect.",
      zh: "进入 Motor Control 界面后，点击 Connect 连接总线。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/control/03.png" },
    title: { en: "3 · Motors appear", zh: "3 · 显示电机" },
    body: {
      en: "On a successful connection, the detected motors and their info show in the left list.",
      zh: "连接成功后，左侧列表会显示电机及其信息。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/control/04.png" },
    title: { en: "4 · Select & initialize", zh: "4 · 选择并初始化" },
    body: {
      en: "Select the motor you want to drive, then press the Initialize button.",
      zh: "选择你要控制的电机，然后点击初始化按钮。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/control/05.png" },
    title: { en: "5 · Choose a mode & send", zh: "5 · 选择模式并发送" },
    body: {
      en: "Pick a control mode — here velocity mode (0.5 rev/s, 30% max torque). In order: choose the mode, enable the motor, limit peak torque to 30%, set speed to 0.5 rev/s, then send. The motor starts turning.",
      zh: "选择控制方式，这里以速度模式（0.5 rev/s、30% 峰值力矩）为例。按顺序操作：选择控制模式 → 使能电机 → 将峰值力矩限制到 30% → 设置速度 0.5 rev/s → 发送速度。成功后即可看到电机转动。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/control/06.png" },
    title: { en: "6 · Live readout", zh: "6 · 实时数据" },
    body: {
      en: "While running, the Display panel streams live motor data. Press Chart to switch to a graph view.",
      zh: "电机运行时，Display 窗口会实时返回运行数据。点击 Chart 按钮可切换为图表模式。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/control/07.png" },
    title: { en: "7 · Chart view", zh: "7 · 图表模式" },
    body: {
      en: "The chart plots position, velocity and torque over time.",
      zh: "图表模式下可实时观察位置、速度和力矩曲线。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/control/08.png" },
    title: { en: "8 · Record CSV", zh: "8 · 记录 CSV" },
    body: {
      en: "Press Record CSV to save the run. The highlighted field (2) shows the path of the saved data file.",
      zh: "按下 Record CSV 按钮即可保存运行数据。图中标记的 2 号方框就是数据文件的存储路径。",
    },
  },
];

const ZERO_SLIDES: Slide[] = [
  {
    media: { type: "image", src: "/tutorial/zero/01.png" },
    title: { en: "1 · Open Set Zero", zh: "1 · 打开 Set Zero" },
    body: {
      en: "Open the Set Zero (position preset) app from the tool picker.",
      zh: "在工具选择界面选择 Set Zero（设置零点）模式。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/zero/02.png" },
    title: { en: "2 · Read, then Save as preset", zh: "2 · 读取后 Save as preset" },
    body: {
      en: "Connect the bus and select the motor. Read the current position first, then enter the position you want to set and press Save as preset. No error warning means it worked.",
      zh: "连接总线并选择要修改的电机。先读取当前位置，再输入你要设置的位置，按下 Save as preset。无错误警告即为设置成功。",
    },
  },
];

const SMARTKNOB_SLIDES: Slide[] = [
  {
    media: { type: "image", src: "/tutorial/smartknob/01.png" },
    title: { en: "1 · Open SmartKnob", zh: "1 · 打开 SmartKnob" },
    body: {
      en: "Open the SmartKnob app to experience the motor's haptic force-feedback knob.",
      zh: "选择 SmartKnob 模式，体验 hex 电机的智能力反馈旋钮功能。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/smartknob/02.png" },
    title: { en: "2 · Connect & pick a mode", zh: "2 · 连接并选择模式" },
    body: {
      en: "After connecting, select the motor to operate, then pick a feel mode on the right.",
      zh: "连接电机后，选择你想操作的电机，再在右边选择想要的模式。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/smartknob/03.png" },
    title: { en: "3 · Custom mode", zh: "3 · 自定义模式" },
    body: {
      en: "The first option is Custom — tune the haptic feel parameters below the dial.",
      zh: "第一个是自定义模式，你可以在仪表盘下方自定义手感参数。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/smartknob/04.png" },
    title: { en: "4 · Adjust strength", zh: "4 · 调整强度" },
    body: {
      en: "You can also adjust the feel strength of each mode below the mode selector.",
      zh: "你也可以在模式下方调整不同模式的手感强度。",
    },
  },
];

const LIFT_SLIDES: Slide[] = [
  {
    title: { en: "1 · Connect and attach", zh: "1 · 连接并绑定" },
    body: {
      en: "Connect can0 (or the selected adapter), then attach the Lift worker to Node-ID 20 by default. Attach only reads identity, nameplate, configuration and telemetry; it never makes the node Operational.",
      zh: "连接 can0（或所选适配器），再把 Lift worker 绑定到默认 Node-ID 20。绑定只读取身份、铭牌、配置与遥测，不会让节点进入 Operational。",
    },
  },
  {
    title: { en: "2 · Read every safety gate", zh: "2 · 检查全部安全门控" },
    body: {
      en: "Motion stays locked until heartbeat and both TPDOs are fresh, NMT is Operational, CONFIG_VALID is set, no fault is latched, and Homing has completed where required. Never bypass a red or amber blocker.",
      zh: "只有 heartbeat 与两路 TPDO 都新鲜、NMT 为 Operational、CONFIG_VALID 有效、无锁存故障，并在需要时完成 Homing，运动才会解锁。不要绕过任何红色或黄色阻塞提示。",
    },
  },
  {
    title: { en: "3 · Velocity is hold-to-jog", zh: "3 · 速度只允许按住点动" },
    body: {
      en: "Press and hold Up or Down. Rust owns RPDO timing, while the WebView renews a short operator lease; release, blur or stale telemetry stops it. DISABLE OUTPUT is the always-available directed NMT Stop path.",
      zh: "按住“上升”或“下降”才会点动。RPDO 时序由 Rust 管理，WebView 续租短时人机租约；松手、失焦或遥测过期都会停止。DISABLE OUTPUT 是始终可用的定向 NMT Stop 路径。",
    },
  },
  {
    title: { en: "4 · Homing and Position are autonomous", zh: "4 · Homing 与 Position 是自主运动" },
    body: {
      en: "Use a current-limited supply and keep physical power removal ready. A confirmed Detach/Disconnect cancels autonomous motion, but process crash or host power loss cannot guarantee cancellation. Commission free motor and Homing before Position.",
      zh: "使用限流电源并随时准备物理断电。经确认的 Detach/Disconnect 会取消自主运动，但进程崩溃或主机掉电无法保证。应先完成自由电机与 Homing 调试，最后才测试 Position。",
    },
  },
];

const CANALYZER_SLIDES: Slide[] = [
  {
    media: { type: "image", src: "/tutorial/canalyzer/01.png" },
    title: { en: "1 · Open CAN Analyzer", zh: "1 · 打开 CAN 分析仪" },
    body: {
      en: "Open the CAN Analyzer app to inspect the messages on the connected CAN bus.",
      zh: "选择 CAN Analyzer 模式，查看所连接 CAN 总线上的消息。",
    },
  },
  {
    media: { type: "image", src: "/tutorial/canalyzer/02.png" },
    title: { en: "2 · Connect & operate", zh: "2 · 连接与操作" },
    body: {
      en: "Press Connect first, then use the various buttons to filter and interact with the traffic.",
      zh: "同样先点击连接按钮，之后可以点击不同的按钮进行操作。",
    },
  },
];

// Slide sets keyed by tool id (matching App's Tool union, plus "home" for the
// landing-page guide). Tools without a written guide yet fall back to
// placeholder steps.
export const TUTORIALS: Record<string, Slide[]> = {
  home: HOME_SLIDES,
  control: CONTROL_SLIDES,
  changeId: CHANGE_ID_SLIDES,
  zero: ZERO_SLIDES,
  hopea3: placeholderSlides("hopea3"),
  lift: LIFT_SLIDES,
  smartknob: SMARTKNOB_SLIDES,
  zenoh: placeholderSlides("zenoh"),
  arm: placeholderSlides("arm"),
  config: placeholderSlides("config"),
  canalyzer: CANALYZER_SLIDES,
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
