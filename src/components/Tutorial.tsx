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

const SETTINGS_SLIDES: Slide[] = [
  {
    title: { en: "1 · Connect and select", zh: "1 · 连接并选择设备" },
    body: {
      en: "Open Device Settings, connect the CAN bus, and select a device from the sidebar. All discovered nodes are shown, but write operations are available only for an exact known Vendor-ID + Product-code identity.",
      zh: "打开「设备设置」，连接 CAN 总线，再从侧栏选择设备。这里会显示所有发现的节点，但只有 Vendor-ID + Product-code 精确匹配的已知设备才开放写操作。",
    },
  },
  {
    title: { en: "2 · Configure communication", zh: "2 · 配置通信参数" },
    body: {
      en: "The current Node-ID comes from the selected device and cannot be typed manually. Nominal timing is fixed at 1 Mbit/s, SP 0.80. CAN-FD devices offer 1/2/4 Mbit/s at SP 0.80 or 5 Mbit/s at SP 0.75, plus TPDO BRS. Classic-CAN devices hide the FD fields.",
      zh: "当前 Node-ID 来自所选设备，不能手动填写。仲裁段固定为 1 Mbit/s、SP 0.80。CAN-FD 设备可选 1/2/4 Mbit/s（SP 0.80）或 5 Mbit/s（SP 0.75），并可设置 TPDO BRS；Classic CAN 设备会隐藏 FD 字段。",
    },
  },
  {
    title: { en: "3 · Apply and follow the result", zh: "3 · 应用并按结果处理" },
    body: {
      en: "Communication settings can be applied only while the device is Pre-Operational or Stopped. A 0x2001 change requires a later power cycle. A pending 0x2100 change must be allowed to persist; do not reset the device automatically. A BRS-only change may take effect immediately.",
      zh: "只有设备处于 Pre-Operational 或 Stopped 时才能应用通信设置。修改 0x2001 后需要稍后重新上电；0x2100 显示等待持久化时，应等待完成，不要自动复位设备；仅修改 BRS 时可以立即生效。",
    },
  },
  {
    title: { en: "4 · Motor position preset", zh: "4 · 电机位置预设" },
    body: {
      en: "Known motors have a separate Position Preset card. Position is read once after each online edge and can always be refreshed manually. Saving a preset is its own transaction and performs one confirmation read; a failed read is never retried in a loop.",
      zh: "已知电机另有独立的「位置预设」卡片。每次设备上线后只自动读取一次位置，也可以随时手动刷新。保存预设是独立事务，之后只确认读取一次；读取失败不会进入自动重试循环。",
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

const DFU_SLIDES: Slide[] = [
  {
    title: { en: "1 · Choose transport", zh: "1 · 选择传输方式" },
    body: {
      en: "For the current HPM product, enter USB Bootloader mode and select USB. The STM32 CAN page currently provides read-only Classic-CAN discovery; all known product profiles remain write-locked.",
      zh: "当前 HPM 产品请进入 USB Bootloader 并选择 USB。STM32 CAN 页面目前只提供 Classic CAN 只读发现；所有已知产品 profile 仍保持写锁定。",
    },
  },
  {
    title: { en: "2 · Validate firmware", zh: "2 · 校验固件" },
    body: {
      en: "USB accepts a local artifact after strict validation. STM32 .meowpkg selection appears only after an exact product profile is qualified; manual selection never bypasses validation.",
      zh: "USB 可在严格校验后选择本地制品。STM32 只有在精确产品 profile 完成资格确认后才开放 .meowpkg；手动选择不会绕过校验。",
    },
  },
  {
    title: { en: "3 · Upgrade and check", zh: "3 · 升级并检查" },
    body: {
      en: "Keep power connected during USB update. USB cannot prove application health, so check the HPM device's actual function afterward. This build does not start an STM32 CAN write.",
      zh: "USB 升级期间请保持供电。USB 无法证明 APP 健康，因此结束后还需检查 HPM 设备的实际功能。此构建不会启动 STM32 CAN 写入。",
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
      en: "Motion stays locked until heartbeat and both TPDOs are fresh, the encoder/INA sample is independently healthy, NMT is Operational, CONFIG_VALID is set, no fault is latched, and Homing has completed where required. Never bypass a red or amber blocker.",
      zh: "只有 heartbeat 与两路 TPDO 都新鲜、编码器/INA 联合样本也独立确认健康、NMT 为 Operational、CONFIG_VALID 有效、无锁存故障，并在需要时完成 Homing，运动才会解锁。不要绕过任何红色或黄色阻塞提示。",
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
  settings: SETTINGS_SLIDES,
  hopea3: placeholderSlides("hopea3"),
  lift: LIFT_SLIDES,
  smartknob: SMARTKNOB_SLIDES,
  zenoh: placeholderSlides("zenoh"),
  arm: placeholderSlides("arm"),
  config: placeholderSlides("config"),
  canalyzer: CANALYZER_SLIDES,
  dfu: DFU_SLIDES,
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
