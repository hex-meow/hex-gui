# Device identity & auto-discovery (CANopen `0x1018`)

How the GUI figures out **what** is on the CAN bus and **which panel** to open
for it. This is the "who is who" convention for hex-meow devices.

> Looking for the **list of all known devices** (motors, IMU, …)? See the
> human-readable catalog: [known-devices.md](known-devices.md).

## TL;DR

1. Every device emits a **heartbeat** (`0x700+nid`). The period is per-device
   (its `0x1017`) — e.g. the IMU 500 ms, motors ~1 s — discovery only needs the
   node to heartbeat *at all*.
2. `hex-motor`'s `Cia402Manager` sweeps heartbeats and, for each new node, reads
   its **identity object `0x1018`** over SDO (vendor id, product code, …).
3. The backend maps the exact `(vendor_id, product_code)` → a **device kind**
   (`"motor"`, `"imu"`, `"lift"`, or `"unknown"`) and ships it as `device_type` on each entry
   of `list_devices()`.
4. The sidebar lists every discovered device. **Clicking one routes by
   `device_type`**: an IMU opens `ImuPanel`, a motor opens `MotorDetail`.

No per-device-type discovery code — IMUs show up in the same list as motors,
exactly "like motors".

## The `0x1018` identity convention

| Sub | Field            | convention |
| --- | ---------------- | ------------------- |
| 01  | Vendor ID        | ASCII: `0x00686578`="hex" (**HEXFELLOW**), `0x6865786D`="hexm" (**hex-meow**) |
| 02  | Product Code     | ASCII model tag, e.g. `0x00696D75` = `"imu"` |
| 03  | Revision Number  | firmware date/letter code |
| 04  | Serial Number    | per-unit |

ASCII-as-hex makes identities human-readable in a bus sniffer: `0x00686578`
reads `h e x` (HEXFELLOW), `0x6865786D` reads `h e x m` (hex-meow), and the
IMU product `0x00696D75` reads `i m u`.

A device **kind** may map to **several** product codes (e.g. future IMU variants
`IMU2`, a different sensor) — they all share **one** frontend panel.

## Where the registry lives

- **Motors** are routed only from exact tuples in
  `device_registry::MOTOR_IDENTITIES`, mirrored from the motor driver's known
  product table.
- **Non-motor** devices (IMU, …) are registered in **this repo**:
  [`src-tauri/src/device_registry.rs`](../src-tauri/src/device_registry.rs).

`device_registry::classify(vendor_id, product_code) -> DeviceKind` returns the
kind; anything not listed is treated as `Unknown` and cannot open motor
controls.

### Add a new IMU (or other non-motor device)

1. Add a row to `NON_MOTOR_DEVICES` in `device_registry.rs`:
   ```rust
   KnownDevice {
       vendor_id: VENDOR_HEXM,
       product_code: 0x........,     // exact ASCII model tag
       kind: DeviceKind::Imu,        // same kind → same panel
       name: "hex-meow IMU mk2",
   },
   ```
2. Nothing else for IMUs — every exact `DeviceKind::Imu` product tuple routes
   to the single `ImuPanel`. Do not add vendor-wide wildcard entries.
3. New **kind** (not IMU)? Add a `DeviceKind` variant, give it a panel, and add a
   routing arm in `App.tsx` (see below).

## The click-to-route flow

```
heartbeat 0x700+nid ─► Cia402Manager discovery ─► read 0x1018 (SDO)
        │
        ▼
list_devices() → MotorInfo { …, device_type }   (device_registry::classify)
        │
        ▼  (frontend polls every 700 ms)
Sidebar lists devices; an IMU row shows an "IMU" badge
        │  user clicks a row → setSelectedNid(nid)
        ▼
App.tsx panel switch:
   selected.device_type === "imu"     → <ImuPanel info connected />
   selected.device_type === "motor"   → <MotorDetail … />
   selected.device_type === "lift"    → prompt to open Lift tool
   selected.device_type === "unknown" → safe unsupported-device notice
```

Relevant files:

- Backend: `src-tauri/src/device_registry.rs` (registry), `dto.rs`
  (`MotorInfoDto.device_type`), `imu.rs` (IMU session), `commands.rs`
  (`imu_*`), `state.rs`, `lib.rs`.
- Frontend: `src/App.tsx` (routing), `src/components/Sidebar.tsx` (badge),
  `src/components/ImuPanel.tsx` + `ImuViewer.tsx`, `src/useImuTelemetry.ts`,
  `src/types.ts`, `src/api.ts`.

## IMU data path (for reference)

The IMU defaults to **Pre-Operational** (it heartbeats but does not stream).
`ImuPanel` calls `imu_start(nid)` on mount, which:

1. subscribes to the IMU's **TPDO1** (`0x180+nid`),
2. sends an **NMT Start** (`0x000`, `[0x01, nid]`) → device goes Operational and
   streams TPDO1,
3. parses each 26-byte CAN-FD frame into a snapshot the UI polls.

On unmount it sends **NMT Pre-Operational** (`[0x80, nid]`) to stop the stream.

### TPDO1 layout (`0x180+nid`, CAN-FD, little-endian)

| offset | field        | type | unit         |
| ------ | ------------ | ---- | ------------ |
| 0      | q0 (w)       | i16  | ×10000       |
| 2      | q1 (x)       | i16  | ×10000       |
| 4      | q2 (y)       | i16  | ×10000       |
| 6      | q3 (z)       | i16  | ×10000       |
| 8      | accel X      | i16  | mg           |
| 10     | accel Y      | i16  | mg           |
| 12     | accel Z      | i16  | mg           |
| 14     | gyro X       | i16  | 0.1 °/s      |
| 16     | gyro Y       | i16  | 0.1 °/s      |
| 18     | gyro Z       | i16  | 0.1 °/s      |
| 20     | temperature  | i16  | 0.01 °C      |
| 22     | sample count | u32  | —            |

### Commands (SDO download to the IMU node)

| Object       | Action                  |
| ------------ | ----------------------- |
| `0x3200:01`  | write `1` → gyro-bias trim (hold still) |
| `0x3200:02`  | write `1` → yaw reset (re-level)        |
