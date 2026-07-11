# gs_usb USB Communication

This project does not talk to the MCU through USB CDC serial, HID reports, or a
custom byte stream designed in this repository. It uses the `gs_usb`
candleLight-style USB-CAN protocol through the Rust `can-transport` crate. The
application builds CAN or CAN-FD frames, and `can-transport` serializes each
frame into a `gs_host_frame` that is sent over USB bulk transfers.

## Stack Overview

The runtime path is:

```text
React/Tauri command
  -> Rust backend command
  -> can_transport::CanBus::send(CanFrame)
  -> can_transport::gs_usb::encode_host_frame(...)
  -> USB bulk OUT endpoint 0x01
  -> gs_usb/candleLight firmware
  -> CAN/CAN-FD bus
```

Receive is the reverse path:

```text
CAN/CAN-FD bus
  -> gs_usb/candleLight firmware
  -> USB bulk IN endpoint 0x81
  -> can_transport::gs_usb::parse_host_frame(...)
  -> CanRx subscribers in this application
```

Relevant project files:

- `src-tauri/Cargo.toml` enables `can-transport` with the `gs_usb` feature.
- `src-tauri/src/backend.rs` selects the backend and bitrate mode.
- `src-tauri/src/smartknob.rs`, `src-tauri/src/hopea3.rs`,
  `src-tauri/src/rollercan.rs`, and `src-tauri/src/analyzer.rs` construct the
  actual CAN frames.

The `gs_usb` implementation used by this project is from:

```text
C:\Users\hb\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\can-transport-0.1.2\src\gs_usb.rs
```

## USB Protocol Type

The USB side is the `gs_usb` vendor protocol used by candleLight-compatible
USB-CAN adapters.

Important consequences:

- It is not a COM port. There is no line coding, baud rate, or serial framing at
  the project level.
- It is not HID. There are no HID report descriptors or fixed HID reports in
  this code path.
- It uses USB vendor control requests for configuration.
- It uses USB bulk endpoints for frame traffic.
- On Windows, the adapter interface must be bound to WinUSB. The dependency
  comments mention either Microsoft OS 2.0 descriptors in firmware or binding
  once with Zadig.
- On Linux, this userspace backend may detach the kernel `gs_usb` driver while
  the program owns the USB interface.

Known VID/PID pairs recognized by the dependency are:

| VID | PID | Meaning |
| --- | --- | --- |
| `0x1209` | `0x2323` | Generic candleLight / bytewerk.org |
| `0x1d50` | `0x606f` | Geschwister Schneider / candleLight |
| `0x1d50` | `0x600f` | Older Geschwister Schneider device |

## Project Backend Selection

`src-tauri/src/backend.rs` has two open paths.

### Default CAN-FD path

`open_bus(spec, hw_timestamp)` is used by most tools. A spec such as `gs_usb`,
`gs_usb0`, `gs_usb1`, `gs_usb:1`, or `gsusb2` selects a `gs_usb` channel.

For `gs_usb`, it opens:

```rust
GsUsbConfig::fd_1m_5m()
    .with_channel(channel)
    .with_hw_timestamp(hw_timestamp)
```

That means:

- CAN-FD enabled.
- Nominal/arbitration bitrate: 1 Mbit/s.
- Data phase bitrate: 5 Mbit/s.
- Device CAN clock assumption: 80 MHz.
- Optional hardware timestamps if requested and supported by firmware.

The nominal timing values in the dependency are:

| Field | Value |
| --- | ---: |
| `prop_seg` | 31 |
| `phase_seg1` | 32 |
| `phase_seg2` | 16 |
| `sjw` | 5 |
| `brp` | 1 |

The CAN-FD data phase timing values are:

| Field | Value |
| --- | ---: |
| `prop_seg` | 5 |
| `phase_seg1` | 6 |
| `phase_seg2` | 4 |
| `sjw` | 3 |
| `brp` | 1 |

### Classic CAN path

`open_classic_1m_bus(spec)` is used by RollerCAN. For `gs_usb`, it opens:

```rust
GsUsbConfig::classic_1m().with_channel(channel)
```

That means:

- Classic CAN 2.0 only.
- 1 Mbit/s.
- CAN-FD is not enabled.
- Payload arm in the USB frame is 8 bytes instead of 64 bytes.

RollerCAN uses this path because its protocol is extended classic CAN, not
CAN-FD.

## USB Setup Sequence

After the device is opened and interface 0 is claimed, the dependency configures
the adapter using vendor control transfers.

The request type is USB vendor control. The code uses recipient/interface style
requests against interface 0.

Vendor request codes:

| `bRequest` | Name | Direction | Purpose |
| ---: | --- | --- | --- |
| `0` | `BREQ_HOST_FORMAT` | OUT | Tell the device host byte order. Payload is little-endian `0x0000beef`. |
| `4` | `BREQ_BT_CONST` | IN | Read device bit-timing constants and feature bits. |
| `1` | `BREQ_BITTIMING` | OUT | Set nominal/arbitration CAN bit timing. |
| `10` | `BREQ_DATA_BITTIMING` | OUT | Set CAN-FD data phase bit timing. Sent only when FD mode is enabled. |
| `2` | `BREQ_MODE` | OUT | Start the selected CAN channel and enable mode flags. |
| `14` | `BREQ_GET_STATE` | IN | Read controller state and RX/TX error counters, if firmware supports it. |

The open sequence is:

1. Claim USB interface 0.
2. Send `BREQ_HOST_FORMAT` with `ef be 00 00`.
3. Read `BREQ_BT_CONST`; the first little-endian `u32` is the feature word.
4. Decide whether hardware timestamps and `GET_STATE` are available.
5. Send `BREQ_BITTIMING` for nominal 1 Mbit/s timing.
6. If CAN-FD is enabled, send `BREQ_DATA_BITTIMING` for 5 Mbit/s data timing.
7. Send `BREQ_MODE` with `{ mode = START, flags = ... }`.
8. Open bulk IN endpoint `0x81`.
9. Open bulk OUT endpoint `0x01`.
10. Start the async USB reader task.

Mode flags used by the dependency:

| Flag | Value | Meaning |
| --- | ---: | --- |
| `MODE_LISTEN_ONLY` | `1 << 0` | Listen-only mode, not used by this project by default. |
| `MODE_LOOP_BACK` | `1 << 1` | Loopback mode, not used by this project by default. |
| `MODE_HW_TIMESTAMP` | `1 << 4` | Ask firmware to append hardware timestamps. |
| `MODE_FD` | `1 << 8` | Enable CAN-FD mode. |

The `BREQ_MODE` OUT payload is 8 bytes:

```text
offset 0..4  mode  little-endian u32, currently 1 for START
offset 4..8  flags little-endian u32
```

## Bulk Endpoints

The dependency uses fixed endpoint addresses:

| Endpoint | Direction | Purpose |
| --- | --- | --- |
| `0x01` | OUT | Host sends one serialized `gs_host_frame` per submitted transfer. |
| `0x81` | IN | Device returns received frames and TX echo/completion frames. |

The read buffer size is 512 bytes. The dependency keeps multiple IN transfers in
flight so received CAN frames are not lost between reads.

## gs_host_frame Layout

Every CAN frame sent through USB bulk OUT is serialized as a `gs_host_frame`.
The header is always 12 bytes. The data arm is 8 bytes in classic mode and 64
bytes in FD mode.

```text
offset  size  field       endian / meaning
------  ----  ----------  -----------------------------------------------
0       4     echo_id     little-endian u32
4       4     can_id      little-endian u32, SocketCAN-style flag bits
8       1     can_dlc     classic length or CAN-FD DLC code
9       1     channel     gs_usb channel number, e.g. 0 for gs_usb0
10      1     flags       per-frame flags, e.g. FD and BRS
11      1     reserved    zero
12      N     data        CAN payload copied at the start of the data arm
```

Where `N` is:

| Adapter mode | Data arm size in USB frame | Max CAN payload |
| --- | ---: | ---: |
| Classic CAN mode | 8 bytes | 8 bytes |
| CAN-FD mode | 64 bytes | 64 bytes |

The total bulk OUT transfer length is therefore:

| Adapter mode | Transfer length |
| --- | ---: |
| Classic CAN mode | `12 + 8 = 20` bytes |
| CAN-FD mode | `12 + 64 = 76` bytes |

In CAN-FD mode, even a classic CAN data frame is placed into a 64-byte data arm
because the adapter channel was opened in FD mode.

## echo_id

For transmitted frames, the host allocates an incrementing `echo_id`:

```text
echo_id = atomic_counter & 0x7fff_ffff
```

That value is written at bytes `0..4`.

On receive, the dependency treats `echo_id == 0xffff_ffff` as a real received
CAN frame from the bus. Any other `echo_id` is considered a TX echo/completion
for a frame the host sent, and is ignored by normal subscribers.

This matters for the analyzer: by default, the receive path does not interpret
ordinary TX echoes as bus-confirmed RX traffic.

## can_id Encoding

The `can_id` field is little-endian `u32` with SocketCAN-style flags ORed into
the raw CAN identifier.

Flags used by the dependency:

| Flag | Value | Meaning |
| --- | ---: | --- |
| `CAN_EFF_FLAG` | `0x8000_0000` | Extended 29-bit CAN ID. |
| `CAN_RTR_FLAG` | `0x4000_0000` | Remote transmission request. |
| `CAN_ERR_FLAG` | `0x2000_0000` | CAN error frame; ignored by this dependency receive path. |

Masks:

| Mask | Value | Meaning |
| --- | ---: | --- |
| `CAN_SFF_MASK` | `0x0000_07ff` | Standard 11-bit ID mask. |
| `CAN_EFF_MASK` | `0x1fff_ffff` | Extended 29-bit ID mask. |

Examples:

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

## DLC and FD Flags

For classic CAN data frames:

```text
can_dlc = payload length, 0..8
flags   = 0
```

For remote frames:

```text
can_dlc = requested length, capped to 8
flags   = 0
can_id  includes CAN_RTR_FLAG
```

For CAN-FD frames:

```text
can_dlc = CAN-FD DLC code for the payload length
flags   = FLAG_FD, plus FLAG_BRS when bit-rate switching is requested
```

FD frame flags:

| Flag | Value | Meaning |
| --- | ---: | --- |
| `FLAG_FD` | `1 << 1` | Frame is CAN-FD. |
| `FLAG_BRS` | `1 << 2` | CAN-FD bit-rate switching is enabled. |

CAN-FD DLC mapping used by the dependency:

| Payload length | DLC |
| ---: | ---: |
| 0..8 | same as length |
| 12 | 9 |
| 16 | 10 |
| 20 | 11 |
| 24 | 12 |
| 32 | 13 |
| 48 | 14 |
| 64 | 15 |

When encoding, the dependency chooses the smallest CAN-FD DLC that can hold the
payload. For example, a 9-byte payload becomes DLC 9, which is a 12-byte CAN-FD
data length on the wire.

## Transmit Encoding Algorithm

The dependency's `encode_host_frame(frame, echo_id, fd_mode, channel)` does this:

1. Start with the raw CAN ID.
2. If the ID is extended, OR `CAN_EFF_FLAG`.
3. For remote frames, OR `CAN_RTR_FLAG`.
4. Pick `can_dlc`, payload slice, and frame flags from the `CanFrame` kind:
   - `Data`: DLC is payload length, flags zero.
   - `Fd`: DLC is CAN-FD DLC, flags include `FLAG_FD` and maybe `FLAG_BRS`.
   - `Remote`: DLC is requested length, payload is empty.
5. Allocate a zeroed buffer:
   - `12 + 8` bytes when the adapter is in classic mode.
   - `12 + 64` bytes when the adapter is in FD mode.
6. Write:
   - `echo_id` at bytes `0..4`.
   - encoded `can_id` at bytes `4..8`.
   - `can_dlc` at byte `8`.
   - channel at byte `9`.
   - flags at byte `10`.
   - payload at bytes `12..12 + payload.len()`.
7. Submit the buffer to USB bulk OUT endpoint `0x01`.

Pseudo-code:

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

## Receive Parsing

The receive parser accepts only real received frames:

1. The buffer must be at least 12 bytes.
2. `echo_id` must be `0xffff_ffff`; otherwise it is treated as a TX echo.
3. The channel byte must match the opened adapter channel.
4. CAN error frames are ignored.
5. The raw ID is decoded as standard or extended based on `CAN_EFF_FLAG`.
6. If `CAN_RTR_FLAG` is set, the frame becomes a remote CAN frame.
7. If `FLAG_FD` is set, the DLC is converted through the CAN-FD DLC table.
8. Otherwise, the payload length is `min(dlc, 8)`.
9. If hardware timestamps are enabled, a trailing little-endian `u32` timestamp
   is read after the fixed data arm:
   - `12 + 8` in classic mode.
   - `12 + 64` in FD mode.

The timestamp is a device-side microsecond counter when firmware supports it.

## Project-Level CAN Frames

The project generally never builds raw `gs_host_frame` buffers itself. It builds
`CanFrame` values, then calls `bus.send(frame).await`.

### CAN analyzer

The analyzer can send arbitrary frames from UI input:

- Remote frame: `CanFrame::new_remote(id, dlc.min(8))`
- CAN-FD frame: `CanFrame::new_fd(id, data, brs)`
- Classic data frame: `CanFrame::new_data(id, data)`

These are useful when manually verifying the adapter or a target device.

### SmartKnob

SmartKnob opens the default CAN-FD path and streams RPDO frames.

CAN frame:

```text
kind:       CAN-FD, BRS enabled
CAN ID:     0x200 + nid
payload:    8 bytes
```

Payload layout:

```text
offset  size  field
------  ----  -------------------------------------------
0       4     TFF torque command, little-endian f32, Nm
4       2     KD, little-endian u16, currently 0 in stream
6       2     max torque, little-endian u16, permille
```

Because the adapter is in CAN-FD mode, the USB bulk OUT transfer for this
8-byte CAN-FD frame is still a 76-byte `gs_host_frame`:

```text
12-byte header + 64-byte data arm
```

For example, with `nid = 1`, CAN ID is `0x201`:

```text
echo_id:   generated by gs_usb backend
can_id:    0x00000201 -> 01 02 00 00
can_dlc:   8
channel:   0 by default
flags:     FLAG_FD | FLAG_BRS = 0x06
payload:   TFF f32 LE, 00 00, max_torque u16 LE
```

### HopeA3

HopeA3 also uses the default CAN-FD path and sends one shared RPDO frame for
three motors.

CAN frame:

```text
kind:       CAN-FD, BRS enabled
CAN ID:     0x210
payload:    24 bytes
```

Payload layout is three 8-byte slices:

```text
per motor slice:
offset  size  field
------  ----  -------------------------------------------
0       4     VDES target, little-endian f32, rev/s
4       2     KD, little-endian u16
6       2     max torque, little-endian u16
```

Because payload length is 24 bytes, CAN-FD DLC is 12. The USB transfer is:

```text
12-byte header + 64-byte data arm = 76 bytes
```

Header fields for the CAN part:

```text
can_id:    0x00000210 -> 10 02 00 00
can_dlc:   12
flags:     FLAG_FD | FLAG_BRS = 0x06
```

### RollerCAN

RollerCAN uses the classic 1 Mbit path, not CAN-FD. It sends extended classic
CAN data frames.

CAN frame:

```text
kind:       classic CAN data frame
CAN ID:     extended 29-bit
payload:    8 bytes
```

Extended CAN ID layout:

```text
bits      field
--------  -----------------------------------------
28..24    cmd, 5 bits
23..16    param
15..8     host_id
7..0      target_id
```

Constructed as:

```rust
raw_id =
    ((cmd as u32) << 24)
  | ((param as u32) << 16)
  | ((host_id as u32) << 8)
  | target_id as u32;
```

Then the `gs_usb` encoder sets `CAN_EFF_FLAG`, so the USB `can_id` field is:

```text
raw_id | 0x80000000
```

Parameter write payload layout:

```text
offset  size  field
------  ----  -------------------------------------------
0       2     object/index, little-endian u16
2       2     zero padding
4       4     value, little-endian i32
```

Parameter read payload layout:

```text
offset  size  field
------  ----  -------------------------------------------
0       2     object/index, little-endian u16
2       6     zero padding
```

Important RollerCAN object indexes in this project:

| Index | Meaning |
| ---: | --- |
| `0x7004` | Enable |
| `0x7005` | Run mode |
| `0x7006` | Current command |
| `0x7030` | Speed readback |
| `0x7031` | Position readback |
| `0x7032` | Current readback |

The enable sequence writes:

1. `0x7005 = CURRENT_MODE`
2. `0x7006 = 0`
3. `0x7004 = 1`

Because RollerCAN opens the adapter in classic mode, the USB bulk OUT transfer
for each RollerCAN frame is:

```text
12-byte header + 8-byte data arm = 20 bytes
```

Example for `cmd = 0x12`, `param = 0`, `host_id = 0`, `target_id = 0x7f`, and
payload `index = 0x7006`, `value = 1234`:

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
  flags        -> 00
  reserved     -> 00
  data         -> 06 70 00 00 d2 04 00 00
```

## What to Look For in a USB Capture

When capturing USB traffic with USBPcap/Wireshark or similar tools:

1. Device open should show vendor control transfers before bulk traffic.
2. `BREQ_HOST_FORMAT` should contain `ef be 00 00`.
3. `BREQ_BITTIMING` and, for FD mode, `BREQ_DATA_BITTIMING` should appear
   before `BREQ_MODE`.
4. Frame traffic should go to bulk OUT endpoint `0x01`.
5. Received traffic should come from bulk IN endpoint `0x81`.
6. Classic-mode RollerCAN writes should be 20-byte bulk OUT payloads.
7. CAN-FD mode SmartKnob/HopeA3 writes should be 76-byte bulk OUT payloads.
8. Byte offsets `4..8` of a bulk OUT payload should decode to the CAN ID plus
   SocketCAN-style flags.
9. Byte `10` should be `0x06` for CAN-FD+BRS frames and `0x00` for classic
   RollerCAN frames.

## Common Pitfalls

- Do not interpret this USB traffic as serial data. The first four bytes of a
  frame are `echo_id`, not an application command.
- Do not look for MCU commands directly at USB offset zero. MCU/protocol
  commands are inside the CAN ID and CAN payload after `gs_host_frame`
  encapsulation.
- In FD adapter mode, the USB data arm is 64 bytes even when the actual CAN
  payload is only 8 bytes.
- TX echo/completion frames from the adapter are not the same as received CAN
  bus frames. The dependency ignores them unless `echo_id == 0xffff_ffff`.
- RollerCAN is intentionally classic CAN. Opening it through the FD path would
  change adapter mode and does not match that device protocol.

