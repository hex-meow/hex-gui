# gs_usb USB 通讯说明

这个项目不是通过 USB CDC 串口、HID report，或者仓库里自定义的一套 USB
裸字节流来和 MCU 通讯。它使用的是 `gs_usb` / candleLight 风格的 USB-CAN
协议，并通过 Rust 的 `can-transport` crate 访问适配器。应用层构造的是 CAN
或 CAN-FD 帧，`can-transport` 再把这些帧序列化成 `gs_host_frame`，通过 USB
bulk 传输发给 USB-CAN 适配器固件。

## 整体调用链

发送路径是：

```text
React/Tauri command
  -> Rust backend command
  -> can_transport::CanBus::send(CanFrame)
  -> can_transport::gs_usb::encode_host_frame(...)
  -> USB bulk OUT endpoint 0x01
  -> gs_usb/candleLight firmware
  -> CAN/CAN-FD bus
```

接收路径反过来：

```text
CAN/CAN-FD bus
  -> gs_usb/candleLight firmware
  -> USB bulk IN endpoint 0x81
  -> can_transport::gs_usb::parse_host_frame(...)
  -> CanRx subscribers in this application
```

相关项目文件：

- `src-tauri/Cargo.toml` 启用了 `can-transport` 的 `gs_usb` feature。
- `src-tauri/src/backend.rs` 选择 CAN backend 和 bit rate 模式。
- `src-tauri/src/smartknob.rs`、`src-tauri/src/hopea3.rs`、
  `src-tauri/src/rollercan.rs`、`src-tauri/src/analyzer.rs` 构造实际 CAN 帧。

本项目当前使用的 `gs_usb` 实现来自：

```text
.\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\can-transport-0.1.2\src\gs_usb.rs
```

## USB 协议类型

USB 侧使用的是 candleLight 兼容 USB-CAN 适配器常见的 `gs_usb` vendor
protocol。

这意味着：

- 它不是 COM 口。项目层没有 line coding、串口 baud rate、串口帧格式。
- 它不是 HID。代码路径里没有 HID report descriptor 或固定 HID report。
- 它用 USB vendor control request 做设备配置。
- 它用 USB bulk endpoint 传输 CAN frame 数据。
- Windows 下适配器 interface 需要绑定 WinUSB。依赖库注释里提到，如果固件可控，
  可以提供 Microsoft OS 2.0 descriptor 自动绑定；否则可用 Zadig 绑定一次。
- Linux 下 userspace backend 可能会在程序占用 USB interface 时 detach 内核
  `gs_usb` driver。

依赖库识别的已知 VID/PID：

| VID | PID | 含义 |
| --- | --- | --- |
| `0x1209` | `0x2323` | Generic candleLight / bytewerk.org |
| `0x1d50` | `0x606f` | Geschwister Schneider / candleLight |
| `0x1d50` | `0x600f` | Older Geschwister Schneider device |

## 项目中的 Backend 选择

`src-tauri/src/backend.rs` 里有两条打开 CAN bus 的路径。

### 默认 CAN-FD 路径

大多数工具使用 `open_bus(spec, hw_timestamp)`。`spec` 可以是 `gs_usb`、
`gs_usb0`、`gs_usb1`、`gs_usb:1`、`gsusb2` 这类形式，用来选择 `gs_usb`
通道。

对于 `gs_usb`，代码打开的是：

```rust
GsUsbConfig::fd_1m_5m()
    .with_channel(channel)
    .with_hw_timestamp(hw_timestamp)
```

含义是：

- 启用 CAN-FD。
- Nominal/arbitration bitrate：1 Mbit/s。
- Data phase bitrate：5 Mbit/s。
- 设备 CAN clock 假设为 80 MHz。
- 如果请求了硬件时间戳，并且固件支持，则启用硬件时间戳。

依赖库里的 nominal timing 值：

| 字段 | 值 |
| --- | ---: |
| `prop_seg` | 31 |
| `phase_seg1` | 32 |
| `phase_seg2` | 16 |
| `sjw` | 5 |
| `brp` | 1 |

CAN-FD data phase timing 值：

| 字段 | 值 |
| --- | ---: |
| `prop_seg` | 5 |
| `phase_seg1` | 6 |
| `phase_seg2` | 4 |
| `sjw` | 3 |
| `brp` | 1 |

### 共享 CAN-FD 通道上的帧类型

适配器通道只使用 `fd_1m_5m()` 打开一次。与旧版原厂固件通信时，应用仍可在
支持 FD 的通道上发送 Classic CAN 帧；但 RollerCAN SmartKnob 专用固件的双向
通信固定使用开启 BRS 的 8 字节 CAN-FD 帧。

RollerCAN SmartKnob 自动发现受窗口生命周期约束：应用连接总线时只建立接收订阅，
不会发送 FD 探测帧；SmartKnob 窗口挂载后才启用 FD 扫描，卸载时会等待扫描完全
停稳。旧版原厂 RollerCAN Control 窗口启动时也会在第一次 Classic CAN 探测前
再次强制停用 SmartKnob FD 扫描。

## USB 初始化流程

设备打开并 claim interface 0 之后，依赖库会通过 vendor control transfer 配置适配器。

request 类型是 USB vendor control。代码使用面向 interface 0 的 control request。

Vendor request code：

| `bRequest` | 名称 | 方向 | 用途 |
| ---: | --- | --- | --- |
| `0` | `BREQ_HOST_FORMAT` | OUT | 告诉设备 host 字节序。payload 是 little-endian `0x0000beef`。 |
| `4` | `BREQ_BT_CONST` | IN | 读取设备 bit-timing 常量和 feature bits。 |
| `1` | `BREQ_BITTIMING` | OUT | 设置 nominal/arbitration CAN bit timing。 |
| `10` | `BREQ_DATA_BITTIMING` | OUT | 设置 CAN-FD data phase bit timing。仅 FD 模式发送。 |
| `2` | `BREQ_MODE` | OUT | 启动选中的 CAN channel，并启用 mode flags。 |
| `14` | `BREQ_GET_STATE` | IN | 如果固件支持，读取 controller state 和 RX/TX error counter。 |

打开顺序：

1. Claim USB interface 0。
2. 发送 `BREQ_HOST_FORMAT`，payload 为 `ef be 00 00`。
3. 读取 `BREQ_BT_CONST`；返回数据的第一个 little-endian `u32` 是 feature word。
4. 判断硬件时间戳和 `GET_STATE` 是否可用。
5. 发送 `BREQ_BITTIMING`，设置 nominal 1 Mbit/s timing。
6. 如果启用 CAN-FD，发送 `BREQ_DATA_BITTIMING`，设置 5 Mbit/s data timing。
7. 发送 `BREQ_MODE`，payload 为 `{ mode = START, flags = ... }`。
8. 打开 bulk IN endpoint `0x81`。
9. 打开 bulk OUT endpoint `0x01`。
10. 启动异步 USB reader task。

依赖库使用的 mode flags：

| Flag | 值 | 含义 |
| --- | ---: | --- |
| `MODE_LISTEN_ONLY` | `1 << 0` | Listen-only 模式，本项目默认不用。 |
| `MODE_LOOP_BACK` | `1 << 1` | Loopback 模式，本项目默认不用。 |
| `MODE_HW_TIMESTAMP` | `1 << 4` | 要求固件追加硬件时间戳。 |
| `MODE_FD` | `1 << 8` | 启用 CAN-FD 模式。 |

`BREQ_MODE` OUT payload 是 8 字节：

```text
offset 0..4  mode  little-endian u32，当前为 1，表示 START
offset 4..8  flags little-endian u32
```

## Bulk Endpoint

依赖库使用固定 endpoint 地址：

| Endpoint | 方向 | 用途 |
| --- | --- | --- |
| `0x01` | OUT | Host 每次提交一个序列化后的 `gs_host_frame`。 |
| `0x81` | IN | 设备返回收到的 CAN frame，以及 TX echo/completion frame。 |

读取 buffer 大小是 512 字节。依赖库会保持多个 IN transfer in flight，避免两次读取之间丢帧。

## gs_host_frame 布局

每个通过 USB bulk OUT 发送的 CAN frame，都会被序列化为一个 `gs_host_frame`。
header 固定 12 字节。data arm 在 classic 模式下是 8 字节，在 FD 模式下是 64 字节。

```text
offset  size  field       endian / meaning
------  ----  ----------  -----------------------------------------------
0       4     echo_id     little-endian u32
4       4     can_id      little-endian u32，带 SocketCAN 风格 flag bits
8       1     can_dlc     classic 长度，或 CAN-FD DLC code
9       1     channel     gs_usb channel number，例如 gs_usb0 对应 0
10      1     flags       per-frame flags，例如 FD 和 BRS
11      1     reserved    zero
12      N     data        CAN payload，从 data arm 起始处复制
```

其中 `N` 取决于适配器模式：

| 适配器模式 | USB frame 中 data arm 大小 | 最大 CAN payload |
| --- | ---: | ---: |
| Classic CAN mode | 8 bytes | 8 bytes |
| CAN-FD mode | 64 bytes | 64 bytes |

所以 bulk OUT transfer 总长度是：

| 适配器模式 | Transfer 长度 |
| --- | ---: |
| Classic CAN mode | `12 + 8 = 20` bytes |
| CAN-FD mode | `12 + 64 = 76` bytes |

在 CAN-FD 模式下，即使发送的是 classic CAN data frame，USB 里的 data arm 也仍然是
64 字节，因为整个 adapter channel 是以 FD mode 打开的。

## echo_id

发送 frame 时，host 会分配递增的 `echo_id`：

```text
echo_id = atomic_counter & 0x7fff_ffff
```

这个值写入 bytes `0..4`。

接收时，依赖库把 `echo_id == 0xffff_ffff` 当作真正从 CAN bus 收到的 frame。
其他 `echo_id` 都被认为是 host 自己发送 frame 的 TX echo/completion，普通 subscriber
会忽略它们。

这对 analyzer 很重要：默认 receive path 不会把普通 TX echo 当作“总线上确认收到”的 RX traffic。

## can_id 编码

`can_id` 字段是 little-endian `u32`，并且会把 SocketCAN 风格的 flag OR 到 raw CAN ID 上。

依赖库使用的 flags：

| Flag | 值 | 含义 |
| --- | ---: | --- |
| `CAN_EFF_FLAG` | `0x8000_0000` | Extended 29-bit CAN ID。 |
| `CAN_RTR_FLAG` | `0x4000_0000` | Remote transmission request。 |
| `CAN_ERR_FLAG` | `0x2000_0000` | CAN error frame；依赖库接收路径会忽略。 |

Masks：

| Mask | 值 | 含义 |
| --- | ---: | --- |
| `CAN_SFF_MASK` | `0x0000_07ff` | Standard 11-bit ID mask。 |
| `CAN_EFF_MASK` | `0x1fff_ffff` | Extended 29-bit ID mask。 |

例子：

```text
Standard ID 0x210:
  can_id = 0x00000210
  bytes  = 10 02 00 00

Extended ID 0x1200007f:
  can_id = 0x1200007f | 0x80000000 = 0x9200007f
  bytes  = 7f 00 00 92

Remote standard ID 0x701:
  can_id = 0x00000701 | 0x40000000 = 0x40000701
  bytes  = 01 07 00 40
```

## DLC 和 FD Flags

Classic CAN data frame：

```text
can_dlc = payload length, 0..8
flags   = 0
```

Remote frame：

```text
can_dlc = requested length, capped to 8
flags   = 0
can_id  includes CAN_RTR_FLAG
```

CAN-FD frame：

```text
can_dlc = CAN-FD DLC code for the payload length
flags   = FLAG_FD, plus FLAG_BRS when bit-rate switching is requested
```

FD frame flags：

| Flag | 值 | 含义 |
| --- | ---: | --- |
| `FLAG_FD` | `1 << 1` | Frame 是 CAN-FD。 |
| `FLAG_BRS` | `1 << 2` | CAN-FD bit-rate switching 已启用。 |

依赖库使用的 CAN-FD DLC 映射：

| Payload length | DLC |
| ---: | ---: |
| 0..8 | 和 length 相同 |
| 12 | 9 |
| 16 | 10 |
| 20 | 11 |
| 24 | 12 |
| 32 | 13 |
| 48 | 14 |
| 64 | 15 |

编码时，依赖库会选择能容纳 payload 的最小 CAN-FD DLC。例如 9 字节 payload 会变成
DLC 9，也就是 CAN-FD 总线上的 12-byte data length。

## 发送编码算法

依赖库的 `encode_host_frame(frame, echo_id, fd_mode, channel)` 会做这些事：

1. 从 raw CAN ID 开始。
2. 如果 ID 是 extended，OR `CAN_EFF_FLAG`。
3. 如果是 remote frame，OR `CAN_RTR_FLAG`。
4. 根据 `CanFrame` kind 选择 `can_dlc`、payload slice 和 frame flags：
   - `Data`：DLC 是 payload length，flags 为 0。
   - `Fd`：DLC 是 CAN-FD DLC，flags 包含 `FLAG_FD`，可能还包含 `FLAG_BRS`。
   - `Remote`：DLC 是 requested length，payload 为空。
5. 分配一个清零 buffer：
   - classic mode 下是 `12 + 8` 字节。
   - FD mode 下是 `12 + 64` 字节。
6. 写入：
   - `echo_id` 到 bytes `0..4`。
   - 编码后的 `can_id` 到 bytes `4..8`。
   - `can_dlc` 到 byte `8`。
   - channel 到 byte `9`。
   - flags 到 byte `10`。
   - payload 到 bytes `12..12 + payload.len()`。
7. 把 buffer 提交到 USB bulk OUT endpoint `0x01`。

伪代码：

```rust
let mut raw_id = frame.id().raw();
if frame.id().is_extended() {
    raw_id |= CAN_EFF_FLAG;
}
if frame.kind().is_remote() {
    raw_id |= CAN_RTR_FLAG;
}

let data_field = if fd_mode { 64 } else { 8 };
let mut buf = vec![0u8; 12 + data_field];
buf[0..4].copy_from_slice(&echo_id.to_le_bytes());
buf[4..8].copy_from_slice(&raw_id.to_le_bytes());
buf[8] = dlc;
buf[9] = channel as u8;
buf[10] = flags;
buf[12..12 + payload.len()].copy_from_slice(payload);
```

## 接收解析

接收 parser 只接受真正从 CAN bus 收到的 frame：

1. buffer 至少 12 字节。
2. `echo_id` 必须是 `0xffff_ffff`；否则认为是 TX echo。
3. channel byte 必须和打开的 adapter channel 匹配。
4. CAN error frame 会被忽略。
5. raw ID 根据 `CAN_EFF_FLAG` 解码为 standard 或 extended。
6. 如果设置了 `CAN_RTR_FLAG`，则解析成 remote CAN frame。
7. 如果设置了 `FLAG_FD`，DLC 通过 CAN-FD DLC 表转换为 payload length。
8. 否则 payload length 是 `min(dlc, 8)`。
9. 如果启用了硬件时间戳，会在固定 data arm 之后读取一个 trailing little-endian
   `u32` timestamp：
   - classic mode 下 offset 是 `12 + 8`。
   - FD mode 下 offset 是 `12 + 64`。

timestamp 是固件支持时提供的设备侧微秒计数器。

## 项目层 CAN Frame

项目通常不会自己构造 raw `gs_host_frame` buffer。它构造 `CanFrame`，然后调用
`bus.send(frame).await`。

### CAN analyzer

Analyzer 可以根据 UI 输入发送任意 frame：

- Remote frame：`CanFrame::new_remote(id, dlc.min(8))`
- CAN-FD frame：`CanFrame::new_fd(id, data, brs)`
- Classic data frame：`CanFrame::new_data(id, data)`

这适合手动验证 adapter 或目标设备。

### SmartKnob

SmartKnob 打开默认 CAN-FD 路径，并持续发送 RPDO frame。

CAN frame：

```text
kind:       CAN-FD, BRS enabled
CAN ID:     0x200 + nid
payload:    8 bytes
```

Payload 布局：

```text
offset  size  field
------  ----  -------------------------------------------
0       4     TFF torque command, little-endian f32, Nm
4       2     KD, little-endian u16, stream 中当前为 0
6       2     max torque, little-endian u16, permille
```

因为 adapter 是 CAN-FD mode，这个 8-byte CAN-FD frame 对应的 USB bulk OUT
transfer 仍然是 76-byte `gs_host_frame`：

```text
12-byte header + 64-byte data arm
```

例如 `nid = 1` 时，CAN ID 是 `0x201`：

```text
echo_id:   generated by gs_usb backend
can_id:    0x00000201 -> 01 02 00 00
can_dlc:   8
channel:   0 by default
flags:     FLAG_FD | FLAG_BRS = 0x06
payload:   TFF f32 LE, 00 00, max_torque u16 LE
```

### HopeA3

HopeA3 也使用默认 CAN-FD 路径，并为三个电机发送一个 shared RPDO frame。

CAN frame：

```text
kind:       CAN-FD, BRS enabled
CAN ID:     0x210
payload:    24 bytes
```

Payload 是 3 个 8-byte slice：

```text
per motor slice:
offset  size  field
------  ----  -------------------------------------------
0       4     VDES target, little-endian f32, rev/s
4       2     KD, little-endian u16
6       2     max torque, little-endian u16
```

因为 payload length 是 24 bytes，CAN-FD DLC 是 12。USB transfer 是：

```text
12-byte header + 64-byte data arm = 76 bytes
```

CAN 部分的 header 字段：

```text
can_id:    0x00000210 -> 10 02 00 00
can_dlc:   12
flags:     FLAG_FD | FLAG_BRS = 0x06
```

### RollerCAN SmartKnob 专用固件

RollerCAN SmartKnob 专用固件使用与另一条 SmartKnob 路径相同的固定配置：
标称段 1 Mbit/s、数据段 5 Mbit/s。命令、响应和遥测全部使用开启 BRS 的
extended CAN-FD 帧。

CAN frame：

```text
kind:       CAN-FD data frame, BRS enabled
CAN ID:     extended 29-bit
payload:    8 bytes
```

Extended CAN ID 布局：

```text
bits      field
--------  -----------------------------------------
28..24    cmd, 5 bits
23..16    param
15..8     host_id
7..0      target_id
```

构造方式：

```rust
raw_id =
    ((cmd as u32) << 24)
  | ((param as u32) << 16)
  | ((host_id as u32) << 8)
  | target_id as u32;
```

随后 `gs_usb` encoder 会设置 `CAN_EFF_FLAG`，所以 USB `can_id` 字段是：

```text
raw_id | 0x80000000
```

参数写入 payload 布局：

```text
offset  size  field
------  ----  -------------------------------------------
0       2     object/index, little-endian u16
2       2     zero padding
4       4     value, little-endian i32
```

参数读取 payload 布局：

```text
offset  size  field
------  ----  -------------------------------------------
0       2     object/index, little-endian u16
2       6     zero padding
```

本项目里重要的 RollerCAN object index：

| Index | 含义 |
| ---: | --- |
| `0x7004` | Enable |
| `0x7005` | Run mode |
| `0x7006` | Current command |
| `0x7030` | Speed readback |
| `0x7031` | Position readback |
| `0x7032` | Current readback |

enable sequence 写入：

1. `0x7005 = CURRENT_MODE`
2. `0x7006 = 0`
3. `0x7004 = 1`

因为 adapter 处于 CAN-FD mode，每个 8 字节 RollerCAN SmartKnob frame 对应的
USB bulk OUT transfer 是：

```text
12-byte header + 64-byte data arm = 76 bytes
```

示例：`cmd = 0x12`，`param = 0`，`host_id = 0`，`target_id = 0x7f`，
payload 为 `index = 0x7006`、`value = 1234`：

```text
raw extended CAN ID:
  (0x12 << 24) | (0 << 16) | (0 << 8) | 0x7f
  = 0x1200007f

USB can_id field:
  0x1200007f | CAN_EFF_FLAG
  = 0x9200007f
  bytes = 7f 00 00 92

payload:
  index 0x7006 -> 06 70
  padding      -> 00 00
  value 1234   -> d2 04 00 00

gs_host_frame:
  echo_id      -> generated, little-endian u32
  can_id       -> 7f 00 00 92
  can_dlc      -> 08
  channel      -> 00 by default
  flags        -> 06 (FLAG_FD | FLAG_BRS)
  reserved     -> 00
  data         -> 06 70 00 00 d2 04 00 00
```

## USB 抓包时看什么

使用 USBPcap/Wireshark 或类似工具抓 USB 时，可以重点看：

1. 设备打开后，bulk traffic 之前应该先出现 vendor control transfer。
2. `BREQ_HOST_FORMAT` 应该包含 `ef be 00 00`。
3. `BREQ_BITTIMING`，以及 FD 模式下的 `BREQ_DATA_BITTIMING`，应该出现在
   `BREQ_MODE` 之前。
4. frame traffic 应该发往 bulk OUT endpoint `0x01`。
5. 接收 traffic 应该来自 bulk IN endpoint `0x81`。
6. RollerCAN SmartKnob、CANopen SmartKnob 和 HopeA3 write 都应该是 76-byte
   bulk OUT payload。
7. RollerCAN SmartKnob 应该是开启 BRS 的 extended CAN-FD frame。
8. bulk OUT payload 的 byte offsets `4..8` 应该能解码出 CAN ID 和 SocketCAN
   风格 flags。
9. 所有 SmartKnob CAN-FD+BRS frame 的 byte `10` 都应该是 `0x06`。

## 常见误区

- 不要把这段 USB traffic 当作串口数据解析。frame 的前 4 字节是 `echo_id`，不是应用命令。
- 不要在 USB offset 0 直接找 MCU 命令。MCU/设备协议命令在 `gs_host_frame`
  封装之后的 CAN ID 和 CAN payload 里。
- 在 FD adapter mode 下，即使实际 CAN payload 只有 8 字节，USB data arm 也是 64 字节。
- 适配器返回的 TX echo/completion frame 不等于从 CAN bus 收到的 frame。依赖库只有在
  `echo_id == 0xffff_ffff` 时才把它当作真实 RX frame。
- 不要混淆旧版原厂 RollerCAN 固件与 SmartKnob 专用固件：前者可能使用 Classic
  CAN 帧，后者固定要求 1 Mbit/s / 5 Mbit/s CAN-FD+BRS。

