# Known devices catalog (CANopen `0x1018` identity)

A **human-readable** list of every device the GUI knows about, keyed by its
`0x1018` identity `(vendor_id, product_code)`. Use it to proofread the two
machine-readable sources of truth:

| Device class | Source of truth (code) | Used for |
| --- | --- | --- |
| **Motors** | `hex-motor` → `KNOWN_DEVICES`; GUI → `device_registry::MOTOR_IDENTITIES` | friendly name + safe panel routing |
| **Non-motor** (IMU, …) | this repo → [`src-tauri/src/device_registry.rs`](../src-tauri/src/device_registry.rs) | panel routing (`device_type`) |

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

| Vendor ID | Product code | ASCII (product) | Name |
| --- | --- | --- | --- |
| `0x4859444C` | `0xAAAA0001` | — | CiA402 HEX-4310 |
| `0x4859444C` | `0xAAAA0002` | — | CiA402 HEX-4342P |
| `0x4859444C` | `0xAAAA0005` | — | CiA402 HEX-4360P |

Unknown products under either motor vendor remain `unknown` until their exact
tuple is registered. This prevents an unrelated product sharing a vendor ID
from being routed into CiA402 controls.

> SmartKnob and HopeA3 are **applications** that run *on* these motors, not
> separate identities. SmartKnob lets the user choose one motor; the raw
> HopeA3 application uses the fixed base Node-IDs `21`, `22`, and `23`.

---

## Non-motor devices — routed by `device_type` to a dedicated panel

Source: this repo's `device_registry.rs`.

| Vendor ID | Product code | ASCII (product) | Kind | Panel | Heartbeat | Name |
| --- | --- | --- | --- | --- | --- | --- |
| `0x6865786D` | `0x00696D75` | `"imu"` | `imu` | `ImuPanel` (2D + 3D) | 500 ms | hex-meow IMU G4 |
| `0x6865786D` | `0x61696D75` | `"aimu"` | `imu` | `ImuPanel` (2D + 3D) | 500 ms | hex-meow arm IMU |
| `0x6865786D` | `0x006C6674` | `"lft"` | `lift` | redirects to the Lift tool | device-defined | hex-meow lift controller |

> Multiple IMU product codes may be added here; they all share the one `ImuPanel`.

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
