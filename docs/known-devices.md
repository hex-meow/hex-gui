# Known devices catalog (CANopen `0x1018` identity)

A **human-readable** list of every device the GUI knows about, keyed by its
`0x1018` identity `(vendor_id, product_code)`. Use it to proofread the two
machine-readable sources of truth:

| Device class | Source of truth (code) | Used for |
| --- | --- | --- |
| **Motors** | `hex-motor` → `KNOWN_DEVICES`; GUI → `device_registry::MOTOR_IDENTITIES` | friendly name + safe panel/settings routing |
| **Non-motor** (IMU, …) | this repo → [`src-tauri/src/device_registry.rs`](../src-tauri/src/device_registry.rs) | panel/settings routing (`device_type`) |

> Keep this file in sync by hand when you edit either source. Classification
> always requires an exact `(vendor_id, product_code)` tuple. Vendor-wide
> wildcard routing is deliberately forbidden.

ASCII note: identities are often ASCII-as-hex — `0x00686578` = `\0 h e x` =
**"hex"** (HEXFELLOW), `0x6865786D` = `h e x m` = **"hexm"** (hex-meow),
`0x00696D75` = `\0 i m u`, `0x61696D75` = `a i m u`, and
`0x006C6674` = `\0 l f t`.

---

## Vendors

| Vendor ID | ASCII | Who |
| --- | --- | --- |
| `0x00686578` | `"hex"` | **HEXFELLOW** — CiA402 motors, GELLO, … |
| `0x6865786D` | `"hexm"` | **hex-meow** — IMU, lift, … |
| `0x4859444C` | — | HEX CiA402 motor-series vendor |

---

## Motors  — routes to the **Motor** panel (`MotorDetail`)

Source: `hex-motor` `KNOWN_DEVICES`.

| Vendor ID | Product code | ASCII (product) | Name | Settings schema | Zero |
| --- | --- | --- | --- | --- | --- |
| `0x4859444C` | `0xAAAA0001` | — | CiA402 HEX-4310 | `0x2001` | `0x3001` |
| `0x4859444C` | `0xAAAA0002` | — | CiA402 HEX-4342P | `0x2001` | `0x3001` |
| `0x4859444C` | `0xAAAA0005` | — | CiA402 HEX-4360P | `0x2001` | `0x3001` |

Unknown products under either motor vendor remain `unknown` until their exact
tuple is registered. This prevents an unrelated product sharing a vendor ID
from being routed into CiA402 controls.

The custom Meow Motor protocol uses its own exact identities and `MeowMotorPanel`:

| Vendor ID | Product code | Name | Gear ratio | PV target ceiling (Rev/s) |
| --- | --- | --- | --- | --- |
| `0x00686578` | `0x6C64AE8D` | hexmeow 3525 | 25:1 | 10 (provisional) |
| `0x00686578` | `0x6C64BC78` | hexmeow 4310 | 10:1 | 8 |
| `0x00686578` | `0x6C64BCAA` | hexmeow 4342 | 42:1 | 3 |

Commands and feedback already use output-shaft units; the gear ratio is display
metadata. TODO: replace the provisional 3525 ceiling in `src/meowMotorUi.ts`
after measurement with the updated GUI. The 2026-10-14 `can0` check confirmed
the 3525 identity and low-speed PV motion at 0.05 Rev/s with a 20-permille torque
cap. The updated driver's non-motion initialization also confirmed 500 Hz TPDO1
and 50 Hz TPDO2 feedback, then returned the motor to Disabled/Pre-operational.
It did not qualify the maximum speed or firmware updating. Its updater
profile remains disabled until the exact IAP device/firmware policy is known.

> SmartKnob and HopeA3 are **applications** that run *on* these motors, not
> separate identities. SmartKnob lets the user choose one motor; the raw
> HopeA3 application uses the fixed base Node-IDs `21`, `22`, and `23`.

---

## Non-motor devices — routed by `device_type` to a dedicated panel

Source: this repo's `device_registry.rs`.

| Vendor ID | Product code | ASCII (product) | Kind | Panel | Settings schema | Heartbeat | Name |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `0x6865786D` | `0x00696D75` | `"imu"` | `imu` | `ImuPanel` (2D + 3D) | `0x2100/0x2101` | 500 ms | hex-meow IMU G4 |
| `0x6865786D` | `0x61696D75` | `"aimu"` | `imu` | `ImuPanel` (2D + 3D) | `0x2100/0x2101` | 500 ms | hex-meow arm IMU |
| `0x6865786D` | `0x006C6674` | `"lft"` | `lift` | redirects to the Lift tool | `0x2100/0x2101` | device-defined | hex-meow lift controller |

> Multiple IMU product codes may be added here; they all share the one `ImuPanel`.
> `0x2100:00` remains the runtime capability source: a known product reporting
> `1` is Classic-only and must not be offered data-rate or BRS writes; only
> `3` enables those fields.

---

## Other known product codes (informational — not in a routing registry)

Observed in firmware sources under the HEXFELLOW vendor `0x00686578`. They
remain `unknown` until given explicit entries. Listed so a human can decide
whether they need dedicated handling.

| Vendor ID | Product code | Device | Source |
| --- | --- | --- | --- |
| `0x00686578` | `0x00001145` | GELLO joint | `gello/joint` firmware `GELLO_JOINT_V1_PRODUCT_CODE` |
| `0x00686578` | `0x00001146` | GELLO end controller | `gello/end_controller` firmware `GELLO_END_CONTROLLER_V1_PRODUCT_CODE` |

---

## A note on heartbeats & discovery

Discovery (in `hex-motor`'s `Cia402Manager`) finds a node when it sees that
node's **heartbeat** (`0x700 + node-id`) and then reads its `0x1018` identity.

**Heartbeat period is per-device, not universal** — it's the device's `0x1017`
producer-heartbeat-time:

- hex-meow **IMU**: 500 ms (firmware `0x1017` default)
- **motors**: ~1 s (typical)

Discovery only needs the node to heartbeat *at all*; the exact period only
affects how fast it appears and how quickly an offline node ages out.
