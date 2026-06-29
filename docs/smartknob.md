# SmartKnob — 智能触觉旋钮模块

## 概述

SmartKnob 是一个**机器人应用程序（Robot Application）**，它将单个 HEX 4310/4342 无刷云台电机转变为一个软件可配置的触觉旋钮。其核心理念来自 [scottbez1/smartknob](https://github.com/scottbez1/smartknob) 开源固件项目——将固件级别的力矩控制算法**移植到上位机**，通过 CAN-FD 总线以 1 kHz 频率实时下发力矩指令。

旋钮提供多种触觉模式：虚拟档位（detents）、机械限位（endstops）、自动回中（return-to-center）、精细/粗调数值拨盘，以及自适应零重力（Zero-G）无摩擦旋转体验。

---

## 架构

```
┌─────────────────────────────────────────────────────────┐
│                    上位机 (Host)                         │
│  ┌─────────────┐  ┌──────────────────────────────────┐  │
│  │ SmartKnobPanel │  │  haptic_loop (1 kHz)            │  │
│  │ (React/TS)    │  │  ┌──────────┐ ┌──────────────┐ │  │
│  │               │◄─┤  │  detent   │ │ Zero-G       │ │  │
│  │  mode buttons │  │  │  state    │ │ velocity-hold│ │  │
│  │  tuning sliders│  │  │  machine  │ │ + friction   │ │  │
│  │  dial (SVG)   │  │  └────┬─────┘ │   observer    │ │  │
│  └──────┬────────┘  │       │       └──────┬───────┘ │  │
│         │ Tauri cmds │       │   torque_cmd │          │  │
│         │ (async)    │       ▼              │          │  │
│         │            │  ┌──────────────────┐│          │  │
│         │            │  │ clamp + RPDO1    ││          │  │
│         │            │  │ CAN-FD frame     ││          │  │
│         │            │  └────────┬─────────┘│          │  │
│         │            │           │           │          │  │
└─────────┼────────────┼───────────┼───────────┼──────────┘  │
          │            │           │ CAN-FD    │             │
          │            │           ▼           │             │
          │            │  ┌──────────────────┐ │             │
          │            │  │ HEX Motor        │ │             │
          │            │  │ (MIT mode,       │◄┘             │
          │            │  │  0x2003:03 TFF)  │               │
          │            │  └──────────────────┘               │
          │            └─────────────────────────────────────┘
```

### 与电机控制器的交互方式

HEX 电机运行在**非压缩 MIT 模式**（object `0x2003`），其力矩控制律为：

```
τ = TFF + KD · (VDES − v)
```

- `KP = 0`，`PDES = 0`，`VDES = 0` —— 全部在上位机侧计算
- 仅通过 **RPDO1** 以 1 kHz 频率下发 `TFF`（力矩前馈，`0x2003:03`）和 `KD`（速度阻尼增益，`0x2003:05`）
- 所有阻尼由软件 PID 的 D 项完成，保持与原始固件一致

电机反馈通过 **TPDO** 以相同速率读取位置和速度。

---

## 触觉算法

### 档位状态机（Detent State Machine）

核心逻辑直接移植自 SmartKnob 固件的 `motor_task.cpp`：

1. **档位中心（detent center）**：旋钮当前"卡入"的参考角度位置
2. **当前位置与档位中心的偏差** `angle_to_detent_center` 计算弹簧回正力矩
3. 当偏差超过 `snap_point × position_width`，自动跳转到相邻档位（档位中心 ± 宽度，逻辑位置 ±1）
4. **死区**（`DEAD_ZONE_DETENT_PERCENT = 20%` 档位宽度，上限 1°）：档位中心附近的平坦区域，避免微小抖动

### PID 力矩计算

```
input = −angle_to_detent_center + dead_zone_adjustment
pid = clamp(P_gain × input − D_gain × shaft_velocity, −10, 10)
torque = strength_scale × pid
```

- **P 增益**：`detent_strength_unit × 4`（档位内）或 `endstop_strength_unit × 4`（限位处）
- **D 增益**：与档位宽度相关的分段函数 —— 细档位（窄间距）阻尼大产生清脆"咔嗒"感，粗档位阻尼小
- **磁性档位**（`detent_positions` 非空）：仅指定位置有弹簧力，其他位置可自由旋转
- **速度保护**：轴速度超过 60 rad/s 时力矩归零，防止正反馈失控

### 空闲回中（Idle Re-centering）

当旋钮静止时，系统缓慢将档位中心漂移到当前轴角度，补偿长期漂移。对单档位（回中模式）禁用，因为回中模式需要锚定在绝对零点。

---

## 触觉模式（Presets）

模式定义在 [preset_configs()](../src-tauri/src/smartknob.rs) 中，共 11 种：

| # | 名称 | 范围 | 档位宽度 | 特点 |
|---|------|------|----------|------|
| 0 | **Zero-G** | 无界 | 10° | 自适应摩擦抵消 + 速度保持 PI，完全无重量感 |
| 1 | Unbounded / No detents | 无界 | 10° | 自由旋转，无档位 |
| 2 | Bounded 0-10 / No detents | 0..10 | 10° | 有限位，无档位 |
| 3 | Multi-rev / No detents | 0..72 | 10° | 多圈旋转，无档位 |
| 4 | On/off / Strong detent | 0..1 | 60° | 强档位开关 |
| 5 | Return-to-center | 0..0 | 60° | 单档位自动回中 |
| 6 | Fine values / No detents | 0..255 | 1° | 精细调节，无档位 |
| 7 | Fine values / With detents | 0..255 | 1° | 每个值都有档位 |
| 8 | Coarse values / Strong detents | 0..31 | ~8.2° | 强档位粗调 |
| 9 | Coarse values / Weak detents | 0..31 | ~8.2° | 弱档位粗调 |
| 10 | Magnetic detents | 0..31 | 7° | 仅位置 [2,10,21,22] 有磁性档位 |
| 11 | Return-to-center with detents | -6..6 | 60° | 回中 + 档位 |

`max_position < min_position` 表示无界模式（`num_positions = 0`），旋钮可无限旋转。

---

## 各模式原理详解

每种模式的触觉体验由以下核心因素共同决定：

- **档位弹簧强度**（`detent_strength_unit`）：偏离档位中心时的回中力矩大小，决定"咔嗒"的力度
- **限位弹簧强度**（`endstop_strength_unit`）：触碰边界时的反弹力矩，模拟硬限位
- **档位间距**（`position_width_radians`）：相邻档位间的角度间隔，窄间距 → 密集档位 → 阻尼大 → 清脆手感
- **跳档点**（`snap_point`）：偏离档位中心超过此比例时自动跳到相邻档位
- **磁性档位**（`detent_positions`）：仅在指定位置存在弹簧力，其余位置自由旋转
- **摩擦补偿方式**：Zero-G 用自适应观测器，其余模式用固定库伦摩擦补偿

---

### 0 — Zero-G（零重力无摩擦旋转）

**原理：自适应摩擦力抵消 + 速度保持 PI 控制**

这是整个系统中最复杂的模式。传统旋钮受电机机械摩擦（轴承阻力、齿槽效应）影响，旋转时会有"粘滞感"。Zero-G 模式通过**两阶段策略**消除这种感觉：

**阶段 1 — 用户操作中**（加速度变化 > `0.05 rad/s²/tick`）：
- 系统检测到用户正在主动施加力矩，暂停自适应学习（避免将用户施加的力误学为"摩擦力"）
- 使用当前摩擦力估计值 × 速度方向进行纯前馈补偿
- 每次用户重新加速时，目标速度被刷新为当前速度

**阶段 2 — 惯性滑行**（连续 50 ms 低加速度）：
- **速度保持 PI** 介入：以松手时的速度为"目标速度"，P 项（Kp=0.5）抵抗速度衰减，I 项（Ki=0.3，限幅 0.08 Nm）消除稳态误差 → 模拟牛顿第一定律（无摩擦 → 恒速）
- **摩擦力观测器**以对称速率 `0.0002 Nm/tick` 自适应：
  - 速度衰减 → 摩擦力补偿不足 → 上调估计值
  - 速度增大 → 过度补偿 → 下调估计值
  - 速度稳定 → 估计准确 → 保持
- **方向滞后**（`±0.08 rad/s` 死区）：避免零速附近摩擦力符号频繁翻转产生抖动

最终效果：旋钮像在真空中旋转，轻轻一拨即可持续滑行数秒。

> 配置细节：无界范围（`max < min`），10° 档位宽度（仅用于内部参考），`strength_scale=0.1`（低强度），最大力矩限 0.2 Nm。

---

### 1 — Unbounded / No detents（无界自由旋转）

**原理：纯摩擦补偿的自由旋转**

这是最接近普通旋钮的模式——但附加了固定库伦摩擦补偿。

- `detent_strength_unit = 0`：没有档位弹簧力，旋钮在任意角度都无回中趋势
- `max_position < min_position`（无界）：可无限圈旋转，不受任何限位约束
- 唯一的力来自库伦摩擦补偿（`friction_compensation = 0.09 Nm`）：一个恒定的、方向跟随速度的力矩，用于抵消电机机械阻力
- D 增益按分段函数计算（`0.08×strength` ~ `0.02×strength`），但因 `strength=0` 实际阻尼也为 0

**适用场景**：需要连续无级调节的场景，如音量旋钮的无级模式、自由浏览长列表。旋钮完全跟随手感，无任何"卡位"或边界。

---

### 2 — Bounded 0-10 / No detents（有界无档位）

**原理：带软限位的自由旋转**

在前一模式基础上增加了位置边界：

- `min_position=0, max_position=10`：共 11 个逻辑位置（0 到 10），旋钮被限制在此范围内
- `detent_strength_unit = 0, endstop_strength_unit = 1.0`：无档位弹簧，但触碰边界时有限位弹簧力——产生被"墙壁"阻挡的触觉反馈
- `friction_compensation = 0.05 Nm`：轻微摩擦补偿，手感顺滑但不完全失重
- 10° 间距提供适中的旋转行程（总计 ~110°）

**适用场景**：需要在有限范围内平滑选择数值，如设定温度（0-10 级）、亮度等级。用户能感觉到边界但不能感知中间值。

---

### 3 — Multi-rev / No detents（多圈无档位）

**原理：多圈范围内自由旋转**

- `min_position=0, max_position=72`：73 个逻辑位置，72° 总行程（以 10°/位置计算，约 2 圈的物理行程）
- 与模式 2 相同：无档位弹簧，仅有限位弹簧和摩擦补偿（`0.08 Nm`）
- 较大的 `strength_scale=0.15` 使得限位处的碰撞感更明显

**适用场景**：需要覆盖较大数值范围但不希望"咔嗒"感的场景，如粗略的时间设定（0-72 小时）、大范围参数扫描。多圈旋转提供高分辨率的同时保持操作直觉。

---

### 4 — On/off / Strong detent（强档位开关）

**原理：双稳态机械开关模拟**

模拟传统机械开关的"开/关"手感：

- `min_position=0, max_position=1`：仅 2 个位置（0=关, 1=开），每次跳档即切换状态
- `position_width_radians = 60°`：极宽的档位间距，两个位置之间需要大幅旋转
- `detent_strength_unit = 1.0`：强档位弹簧，旋钮被强力吸引到最近的档位中心
- `snap_point = 0.55`：偏离档位中心超过 55% 宽度（33°）时自动跳到相邻位置
- `strength_scale = 0.25`：高强度输出，产生明确、有力的"咔嗒"手感

触觉体验：旋钮在 0 和 1 两个稳定位置之间有明显的"势垒"，需要一定力矩才能推动越过中点，越过後自动吸入另一侧。类似老式拨动开关的阻尼感。

---

### 5 — Return-to-center（自动回中）

**原理：单档位弹簧 + 强力限位，锚定于绝对零点**

这是一个特殊的单档位模式（`num_positions = 1`），只有一个档位中心：

- `min_position = max_position = 0`：唯一的逻辑位置，旋钮始终被弹簧拉回此处
- `detent_strength_unit = 0.01, endstop_strength_unit = 0.6`：档位内弹簧极弱（0.01），但限位弹簧较强（0.6）——离开中心越远，回中力越大
- `position_width_radians = 60°`：较宽的"捕获范围"
- **禁用空闲回中漂移**：`num_positions=1` 时跳过 idle re-centering，确保回中目标始终是绝对零点
- `strength_scale = 0.05`：低强度，手感轻柔

触觉体验类似弹簧自动回中的摇杆或方向盘——无论推到哪里，松手后旋钮自动回到中心。死区（±12°）内弹簧力为零，系统还设计了最小回中力矩（当前设为 0），用于突破静摩擦力确保回到真正中心。

---

### 6 — Fine values / No detents（精细无档位）

**原理：高分辨率无级调节**

- `min_position=0, max_position=255`：256 个位置，覆盖 0-255 的完整范围
- `position_width_radians = 1°`：极窄的档位间距（仅 1°），总共约 256° 行程
- `detent_strength_unit = 0`：无档位弹簧力，值之间平滑过渡
- `friction_compensation = 0.02 Nm`：极低的摩擦补偿，手感极轻
- `strength_scale = 0.3`：高强度——但由于 `detent_strength=0`，这个值主要影响限位处的反馈力度

在 1° 间距下，D 增益处于高位（靠近 `w_lower=3°` 时阻尼最大），产生平滑但略带阻尼的手感，适合精细调节。

**适用场景**：需要从 0-255 精确选值的场景，如 RGB 颜色分量调节、MIDI 参数控制。

---

### 7 — Fine values / With detents（精细有档位）

**原理：每个整数值都有触觉"咔嗒"**

- 与模式 6 相同的范围和间距（0-255, 1°）
- **关键区别**：`detent_strength_unit = 1.0`——每个 1° 位置都有档位弹簧力
- `friction_compensation = 0.03 Nm` + `strength_scale = 0.16`
- 1° 间距产生最强的 D 阻尼（接近下界 3°），档位手感清脆、明确

由于 1° 间距极窄，D 增益计算位于分段函数的高位（靠近 3° 时 ~`0.08×strength`），产生比粗调模式更强的速度阻尼。结果是每个刻度都有清晰的"咔嗒"确认，同时旋转速度受控。

**适用场景**：需要精确到每个值的步进调节，如音量（0-255 级 MIDI CC）、像素级参数调整。

---

### 8 — Coarse values / Strong detents（强档位粗调）

**原理：大力档位 + 宽间距 = 明确分段选择**

- `min_position=0, max_position=31`：32 个位置
- `position_width_radians ≈ 8.23°`（255°/31）：宽间距使每个位置之间有足够的物理行程
- `detent_strength_unit = 2.0`：最强的档位弹簧力之一，需要明确力矩才能推动越过档位
- `snap_point = 1.1`：故意设为 >1.0，意味着旋钮不会自动跳档，需要用户主动推到下一位置
- `strength_scale = 0.75`：高强度，放大弹簧力 → 产生非常明确的"段落感"

D 增益方面，8.23° 间距 > 8° 上界，阻尼为 `0.02×strength = 0.04`，相对较低，使得推进档位时速度快但仍有控制。

**适用场景**：需要明确分段选择且不易误触的场景，如模式选择、档位切换。

---

### 9 — Coarse values / Weak detents（弱档位粗调）

**原理：轻触档位 + 宽间距 = 柔和分段**

- 与模式 8 相同的范围和间距（0-31, 8.23°）
- **关键区别 1**：`detent_strength_unit = 0.2`（仅为强档位的 1/10），弹簧力极弱
- **关键区别 2**：`strength_scale = 2.9`——超高的强度缩放，弥补了 detent_strength_unit 的低值
- 弱的 detent_strength 使 D 增益也相应降低（`0.08×0.2=0.016` 到 `0.02×0.2=0.004`），手感轻盈

综合效果：虽然 strength_scale 放大了最终输出，但 detent_strength_unit 低导致弹簧力和阻尼的基数就小。最终手感介于"有档位确认"和"接近自由旋转"之间——有微妙的段落感但不生硬。

**适用场景**：需要分段但手感轻柔的场景，如音量粗调（0-31 级）、菜单选择。

---

### 10 — Magnetic detents（磁性档位）

**原理：仅在特定位置产生弹簧力，其余位置完全自由旋转**

这是最特殊的模式——模拟"磁性吸附"效果：

- `min_position=0, max_position=31, position_width_radians=7°`：32 个位置，7° 间距（总共约 217°）
- `detent_positions = [2, 10, 21, 22]`：**仅在**位置 2、10、21、22 存在档位弹簧力
- `detent_strength_unit = 2.5`：在这些位置有极强的弹簧吸附
- `snap_point = 0.7`：70% 触发跳档
- **D 增益为 0**：磁性档位模式下禁用 D 增益（代码中 `derivative_gain` 对非空 `detent_positions` 直接返回 0），只靠 P 弹簧力产生手感
- `strength_scale = 0.8`：高强度

触觉体验：旋钮在大多数位置可以完全自由旋转（无任何弹簧力），但经过位置 2、10、21、22 时会感受到强烈的"磁性吸附"——像磁铁吸引铁片一样，旋钮被吸入这些特定位置。这模拟了某些高级音响设备上的"磁性定位"旋钮。

> D 增益被禁用是因为磁性档位的弹簧力仅存在于离散位置——在无障碍区域引入速度阻尼会破坏"自由→吸附"的对比效果。

---

### 11 — Return-to-center with detents（回中 + 档位）

**原理：带刻度感的自动回中**

- `min_position=-6, max_position=6`：13 个位置，对称于零点
- `position_width_radians = 60°`：宽间距
- `detent_strength_unit = 1.0`：每个整数值都有档位，经过时产生"咔嗒"
- `snap_point = 0.55, snap_point_bias = 0.4`：55% 跳档 + 偏置——`snap_point_bias` 在正半轴和负半轴施加方向性偏置，使跳档行为不对称（趋向零点时更容易跳档）
- `strength_scale = 0.15`：适中强度

工作原理是单档位回中和多档位刻度的叠加：
1. 基础层是回中弹簧——旋钮总是趋向零点（`min=max` 的特殊情况不适用，这里 num_positions=13>1）
2. 叠加层是 13 个均匀分布的档位——经过每个整数值时产生"咔嗒"

`snap_point_bias = 0.4` 是关键设计：在负半轴（position ≤ 0），`snap_dec` 增加 `0.4×width` 的偏置，使旋钮更容易向零点跳档；在正半轴类似。这造成靠近零点的位置"更易跳回"，远离零点则相对稳定。

触觉体验类似汽车的方向灯拨杆——有明确的分段感，但始终有回到中心的趋势。

---

## Zero-G 模式详细原理（自适应零重力）

Zero-G 模式是第 0 号预设，通过自适应摩擦力观测器 + 速度保持 PI，创造出"无重量"的旋转手感。

### 两阶段工作流程

**阶段 1 — 用户交互中**（加速度超过阈值）：
- 暂停观测器自适应（避免学习用户施加的力）
- 纯前馈摩擦力补偿（当前估计值 × 速度方向）

**阶段 2 — 惯性滑行**（连续 50 ms 低加速度）：
- **速度保持 PI** 介入，维持用户松手时的速度 → 模拟牛顿第一定律
  - P 增益 `ZERO_G_VEL_HOLD_KP = 0.5` Nm/(rad/s)
  - I 增益 `ZERO_G_VEL_HOLD_KI = 0.3` Nm/rad
  - I 限幅 `0.08` Nm
- **摩擦力观测器** 以对称速率 `0.0002` Nm/tick 自适应
  - 速度衰减 → 摩擦力估计偏低 → 增加
  - 速度增大 → 过度补偿 → 减少
  - 速度稳定 → 估计准确 → 保持
- **方向滞后**（`ZERO_G_SIGN_HYSTERESIS = 0.08` rad/s）：防止零速附近的方向抖动

### 关键参数

| 参数 | 值 | 说明 |
|------|-----|------|
| 种子摩擦力 | 0.05 Nm | 观测器初始估计 |
| 最大输出力矩 | 0.2 Nm | Zero-G 回路力矩上限 |
| 用户加速度阈值 | 0.05 rad/s²/tick | 判断用户是否在施力 |
| 惯性等待 | 50 ticks (50 ms) | 松手后 PI 介入前的延迟 |

---

## 可调参数（Tuning）

通过前端 UI 实时调节，按模式独立保存（切换模式后恢复各模式自己的调参）：

| 参数 | 默认值 | 范围 | 说明 |
|------|--------|------|------|
| **Strength Scale** | 0.15（模式依赖） | ≥ 0 | 整体触觉强度，Nm / PID 单位 |
| **Torque Limit** | 2.0 Nm | ≥ 0 | 上位机侧力矩硬限幅 |
| **Max Torque** | 700‰ | 0..1000 | 电机侧安全限幅（`0x6072`） |
| **Friction Comp** | 0.03 Nm（默认） | ≥ 0 | 库伦摩擦补偿（非 Zero-G 模式） |

Zero-G 模式忽略 `Friction Comp` 参数，使用自适应观测器替代。

---

## 前端 UI

前端组件位于 [SmartKnobPanel.tsx](../src/components/SmartKnobPanel.tsx)。

### 组件结构

```
SmartKnobPanel
├── 控制栏 (Card)
│   ├── 电机选择下拉框 (Select)
│   ├── 启动 / 停止按钮
│   ├── 清除错误按钮
│   └── 状态标签 (Tag)
├── 仪表盘 (Dial) —— SVG 渲染
│   ├── 刻度线 (Tick)
│   ├── 指针 (needle)
│   └── 力矩环 (torque ring)
├── 模式选择区 (Card)
│   └── 11 个模式按钮 (ModeButton)
├── 调参区 (Card)
│   └── 4 个滑动输入 (InputNumber)
└── 遥测数据区 (Card, 运行时可见)
    ├── 角度 / 指令力矩 / 实测力矩
    └── 电机状态 / 驱动温度 / 电机温度
```

### 仪表盘（Dial）

- **有界模式**（2 ≤ 位置数 ≤ 49）：300° 弧形刻度盘，指针指示当前值
- **无界/多圈模式**：自由旋转表盘，刻度线随轴角度移动
- **力矩环**：指针外圈弧长正比于 `|扭矩| / 扭矩限幅`
- **限位指示**：触碰限位时指针变为红色

### 轮询

UI 以 25 Hz（40 ms）轮询后端状态。触觉控制回路在 Rust 侧以 1 kHz 独立运行，不受 UI 轮询速率影响。

---

## Tauri 命令 API

所有命令定义在 [commands.rs](../src-tauri/src/commands.rs)（SmartKnob 部分）：

| 命令 | 参数 | 返回值 | 说明 |
|------|------|--------|------|
| `smartknob_configs` | — | `Vec<KnobConfig>` | 获取所有预设模式（无需连接） |
| `smartknob_start` | `nid`, `config_index` | `()` | 初始化电机并启动触觉回路 |
| `smartknob_stop` | — | `()` | 停止触觉回路并禁用电机 |
| `smartknob_set_config` | `index` | `()` | 切换触觉模式 |
| `smartknob_set_tuning` | `strength_scale`, `torque_limit_nm`, `max_torque_permille`, `friction_compensation` | `()` | 更新实时调参 |
| `smartknob_clear_error` | — | `()` | 清除 CiA402 故障（尽力而为） |
| `smartknob_get_state` | — | `SmartKnobState` | 轮询当前旋钮状态 |

---

## 数据类型

### KnobConfig（触觉模式配置）

| 字段 | 类型 | 说明 |
|------|------|------|
| `position` | i32 | 初始逻辑位置 |
| `min_position` | i32 | 最小逻辑位置 |
| `max_position` | i32 | 最大逻辑位置（< min ⇒ 无界） |
| `position_width_radians` | f64 | 档位间距（弧度） |
| `detent_strength_unit` | f64 | 档位弹簧强度 |
| `endstop_strength_unit` | f64 | 限位弹簧强度 |
| `snap_point` | f64 | 触发跳档的百分比（≥0.5） |
| `snap_point_bias` | f64 | 跳档偏置 |
| `detent_positions` | Vec<i32> | 磁性档位列表（空 = 均匀分布） |
| `zero_g` | bool | 是否启用 Zero-G 模式 |
| `friction_compensation` | f64 | 库伦摩擦补偿（Nm），Zero-G 模式下忽略 |
| `strength_scale` | f64 | 整体触觉强度 |
| `text` | String | 模式按钮上的两行标签 |
| `led_hue` | i32 | 表盘色调（0..255） |

### SmartKnobState（运行时状态快照）

| 字段 | 类型 | 说明 |
|------|------|------|
| `running` | bool | 触觉回路是否运行中 |
| `config_index` | usize | 当前模式索引 |
| `config` | Option\<KnobConfig\> | 当前模式的完整配置 |
| `current_position` | i32 | 当前逻辑位置（档位编号） |
| `sub_position_unit` | f64 | 档位间平滑偏移（−snap..+snap） |
| `shaft_angle_rad` | f64 | 连续轴角度（弧度） |
| `shaft_velocity_rev_per_s` | f64 | 轴速度（rev/s） |
| `applied_torque_nm` | f64 | 当前指令力矩（Nm） |
| `measured_torque_nm` | Option\<f32\> | 电机反馈力矩 |
| `at_endstop` | bool | 是否触碰限位 |
| `node_id` | u8 | 电机 CAN 节点 ID |
| `online` / `enabled` | bool | 电机在线 / 使能状态 |
| `error` | Option\<String\> | CiA402 错误信息 |

---

## 关键常量

| 常量 | 值 | 说明 |
|------|-----|------|
| `CONTROL_HZ` | 1000 | 控制回路频率 |
| `DIRECTION` | 1.0 | 旋转方向符号 |
| `DEAD_ZONE_DETENT_PERCENT` | 0.2 | 档位死区比例 |
| `DEAD_ZONE_RAD` | π/180 (1°) | 死区角度下限 |
| `MAX_VEL_RAD_S` | 60.0 | 安全速度上限 |
| `PID_LIMIT` | 10.0 | PID 输出限幅（固件单位） |
| `FRAME_LEN` | 8 | RPDO 帧字节数 |
| `INIT_ATTEMPTS` | 3 | 电机初始化重试次数 |

---

## 初始化流程

1. CiA402 标准初始化（状态机 → Switch On Disabled）
2. 重映射 RPDO1 映射表，指向 `TFF(32bit) + KD(16bit) + MaxTorque(16bit)`
3. 清零静态 MIT 参数（PDES、VDES、KP）
4. 设置 `0x6072` 最大力矩安全限幅
5. 切换到 MIT 模式（同时使能电机）

初始化支持最多 3 次重试，每次失败后先清除故障再重试。

---

## 相关文件

| 文件 | 说明 |
|------|------|
| [src-tauri/src/smartknob.rs](../src-tauri/src/smartknob.rs) | Rust 后端：触觉算法、模式定义、控制回路 |
| [src-tauri/src/commands.rs](../src-tauri/src/commands.rs) | Tauri 命令层：SmartKnob 相关命令（L370-463） |
| [src/components/SmartKnobPanel.tsx](../src/components/SmartKnobPanel.tsx) | React 前端：UI 面板、仪表盘、调参 |
| [src/types.ts](../src/types.ts) | TypeScript 类型定义：`KnobConfig`、`SmartKnobState` |
| [src/api.ts](../src/api.ts) | 前端 API 封装：`smartknob*` 系列调用 |
