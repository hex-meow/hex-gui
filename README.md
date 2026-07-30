# hex-motor GUI (Tauri)

A Tauri 2.x desktop GUI on top of the local [`hex-motor`](../hex-motor)
crate. Connect to a CAN bus, browse discovered CiA402 motors in a sidebar,
watch each motor's PDO feedback (position / host-filtered velocity / torque /
status word / temps / motor timestamp) as a numeric panel or rolling 2-D
chart, record any motor's full-rate stream to CSV, and drive its CiA402 state
machine (enable / disable / mode switch / targets / max-torque limit).

Frontend: **Vite + React + TypeScript + Ant Design + ECharts**.
Backend: **pure Rust** (Tauri commands over `hex-motor`).

## Layout

```
tauri-test/
├── index.html              # Vite entry
├── package.json            # frontend deps + scripts
├── vite.config.ts
├── src/                    # React frontend (TypeScript)
│   ├── main.tsx / App.tsx
│   ├── api.ts              # typed invoke() wrappers
│   ├── types.ts            # TS mirrors of the Rust DTOs
│   ├── useTelemetry.ts     # 20 Hz get_status poll + rolling buffer
│   └── components/         # ConnectBar / Sidebar / MotorDetail / LivePanel / LiveChart / ControlPanel
└── src-tauri/
    ├── tauri.conf.json
    └── src/
        ├── main.rs / lib.rs
        ├── backend.rs      # CanBus factory (per-OS / per-backend)
        ├── state.rs        # AppState: Cia402Manager + CSV log handles
        ├── dto.rs          # serde DTOs mirroring hex-motor
        ├── commands.rs     # #[tauri::command]s
        └── logging.rs      # full-rate CSV recorder task
```

## Prerequisites

### 1. System libraries (Linux)

Tauri 2.x on Linux links WebKit2GTK + libsoup-3. On Debian/Ubuntu:

```bash
sudo apt install -y \
    libwebkit2gtk-4.1-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
    build-essential pkg-config libssl-dev \
    libayatana-appindicator3-dev librsvg2-dev
```

### 2. Node.js (for the frontend)

The frontend needs Node 18+ (developed on Node 24). Easiest is
[nvm](https://github.com/nvm-sh/nvm):

```bash
curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash
# reopen the shell, then:
nvm install 24
```

Install JS dependencies once (and after any `package.json` change):

```bash
cd tauri-test
npm install
```

### 3. A CAN interface

Three options, selected by the **interface** string in the Connect bar:

- **SocketCAN** (Linux): real hardware on `can0`, or a virtual bus to
  smoke-test without it:
  ```bash
  sudo modprobe vcan
  sudo ip link add dev vcan0 type vcan
  sudo ip link set up vcan0
  ```
- **gs_usb / candleLight** (Linux / macOS / Windows): type `gs_usb`
  (or `gs_usb0`, `gs_usb1` for a specific channel) — a CAN-FD adapter
  driven directly over USB. On Linux this needs usbfs access; add a udev
  rule so the GUI can open it without running as root:
  ```bash
  # adjust idVendor/idProduct for your adapter (here: candleLight 1209:2323)
  echo 'SUBSYSTEM=="usb", ATTR{idVendor}=="1209", ATTR{idProduct}=="2323", MODE="0660", GROUP="plugdev"' \
    | sudo tee /etc/udev/rules.d/70-gs-usb.rules
  sudo udevadm control --reload-rules && sudo udevadm trigger
  ```
  On macOS no setup is needed (no sudo, no driver install).

### 4. HPM USB Bootloader access (Firmware Update tool)

The Firmware Update app currently enables the hardware-tested HPM USB v2
transport for the exact legacy `gs_can` Bootloader profile. On Linux, install
a udev rule for `34b7:beef` so the GUI can claim interface 0 without root:

```bash
sudo install -m 0644 /dev/stdin /etc/udev/rules.d/99-hpm-bl.rules <<'EOF'
SUBSYSTEM=="usb", ATTR{idVendor}=="34b7", ATTR{idProduct}=="beef", MODE="0660", GROUP="plugdev", TAG+="uaccess"
EOF
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Windows uses the Bootloader's WinUSB descriptor; macOS needs no driver setup.
For the initial MVP, connect exactly one matching Bootloader at a time.

## Run

### Dev (hot-reload, recommended)

Uses `tauri-cli`, which runs `npm run dev` (Vite at `:1420`) and the Rust app
together:

```bash
cargo install tauri-cli --version "^2" --locked   # once
cd hex-motor-gui/src-tauri
cargo tauri dev
```

### Quick run (no tauri-cli)

Build the frontend, then run the Rust binary directly (it embeds `dist/`):

```bash
cd hex-motor-gui
npm run build
cd src-tauri && cargo run
```

(Repeat `npm run build` after frontend changes, since `cargo run` embeds the
built `dist/` rather than talking to the Vite dev server.)

### Laggy on Linux? Use a Wayland session

If the UI feels sluggish on Linux — especially when the window is large, with
the lag getting worse the bigger the window — **log into a Wayland session**
(GDM login screen → gear icon → **"Ubuntu on Wayland"** → log in). This is by
far the biggest fix and needs no change to the app.

The cause is **WebKitGTK** — the webview Tauri uses on Linux — not this app.
On the **NVIDIA proprietary driver under X11** (and worse with fractional
display scaling), WebKitGTK's per-frame window presentation is slow, so cost
scales with window pixel area. Chromium-based apps (Chrome, VS Code) don't hit
this; WebKitGTK does. It reproduces in `cargo tauri dev` and in the prebuilt
binary alike, and none of the usual `WEBKIT_DISABLE_DMABUF_RENDERER` /
`WEBKIT_DISABLE_COMPOSITING_MODE` / Skia-CPU env toggles help — but Wayland
does.

**Quick confirmation:** install GNOME Web (`sudo apt install epiphany-browser`),
maximize it, and scroll a long page. If Epiphany is *also* laggy when large,
it's this WebKitGTK/X11 limitation (not hex-motor-gui), and switching to Wayland
is the fix.

## Packaging (Ubuntu x64)

Prebuilt packages target **Ubuntu 22.04+ / x86-64**. Other distros: build from
source (see prerequisites above). `cargo tauri build` produces both a `.deb`
and an `.AppImage`:

```bash
cd tauri-test/src-tauri
cargo tauri build                      # both deb + appimage (see bundle.targets)
# or just one:
cargo tauri build --bundles deb
cargo tauri build --bundles appimage
```

Outputs land in `src-tauri/target/release/bundle/{deb,appimage}/`.

- **`.deb`** (~5 MB) — `sudo apt install ./hex-motor-gui_*.deb`. It declares
  `libwebkit2gtk-4.1-0` + `libgtk-3-0` as dependencies, so apt pulls the
  **WebKitGTK 4.1** runtime automatically. Recommended for Ubuntu.
- **`.AppImage`** (~77 MB) — bundles WebKitGTK, so it runs without installing
  anything: `chmod +x hex-motor-gui_*.AppImage && ./hex-motor-gui_*.AppImage`.
  On Ubuntu 22.04+ you may need FUSE: `sudo apt install libfuse2` (or run with
  `--appimage-extract-and-run`).

> **glibc / build host:** an AppImage links against the build machine's glibc
> and is **not** forward-compatible. Build releases on the **oldest** target
> (Ubuntu 22.04) — e.g. a CI job in an `ubuntu:22.04` Docker image — so they run
> on 22.04 and up. (The `.deb` has the same constraint via its dependencies.)
>
> **Runtime dependency:** all builds need **WebKitGTK 4.1**
> (`libwebkit2gtk-4.1-0`). The `.deb` installs it for you; for the bare binary
> or other distros, install it manually (Ubuntu/Debian:
> `sudo apt install libwebkit2gtk-4.1-0`).

### CI

`.github/workflows/release.yml` builds all three desktop platforms in a matrix:

| Runner          | Bundles                                    |
| --------------- | ------------------------------------------ |
| `ubuntu-22.04`  | `.deb` + `.AppImage` (x86-64)              |
| `windows-latest`| `.msi` + NSIS `.exe` installer (x86-64)    |
| `macos-latest`  | `.dmg` + `.app`, universal (Intel + ARM)   |

The workflow runs on pushes to `main`, PRs, `v*` tags, and manual dispatch. What
it does depends on the trigger:

- Every run first reads the application version from
  `src-tauri/Cargo.toml`, verifies the lock file and runs the frontend/Rust
  test gate. Tag builds additionally require an exact `v<app-version>` match.
- **push / PR / manual** — build every platform and upload the bundles as
  **run artifacts** (Actions → the run → Artifacts). Nothing is released.
- **`v*` tag** — build every platform and create a **draft GitHub Release**
  named from the verified Cargo application version, with every bundle attached.

#### Cutting a draft release

The draft Release is driven entirely by pushing a tag that matches `v*`
(handled by [`tauri-action`](https://github.com/tauri-apps/tauri-action) with
`releaseDraft: true`). There is no button to click — just tag and push:

```bash
# src-tauri/Cargo.toml is the only manually maintained application version.
# Bump it, refresh src-tauri/Cargo.lock with cargo metadata, commit, then:
git tag v1.2.0
git push origin v1.2.0
```

Each platform's job appends its bundles to the same Release. When all three
finish, open **Releases** on GitHub — the draft is waiting there. Review it, then
**Publish** manually (drafts are never public until you publish). Do not move or
reuse a published tag. If a draft and tag have never become an external release,
a maintainer may explicitly delete and recreate both.

> Bundles are **unsigned**: macOS users right-click → Open past Gatekeeper,
> Windows users click through SmartScreen. Add signing later via `tauri-action`
> env vars.
>
> **Green-build prerequisites** (see the header comment in the workflow):
> `hex-arm-dynamics` must be available from crates.io. CI checks out the verified
> shared contract from `hex-meow/hex-robot-proto/master` via `ROBOT_PROTO_DIR`.

## Usage

1. Top bar: pick the CAN interface (default `can0`; also accepts
   `socketcan:vcan0`-style prefixed specs) and your own NID (1..127, must
   differ from every motor), then **连接 (Connect)**.
2. Discovered motors appear in the left **sidebar**. Click one to open its
   detail view.
3. Click **初始化 (Initialize)** in the control card (runs
   `NMT PreOp → TPDO → fault-clear → NMT Op`). The init also brute-forces the
   firmware's flaky heartbeat-fault clear, so a freshly power-cycled or
   reconnected motor comes up clean.
4. **显示面板**: toggle between **数值** (numeric) and **图表** (a rolling
   2-D chart of position / velocity / torque; window defaults to 10 s, 1–60 s
   adjustable).
5. **记录 CSV**: flip the switch to record this motor's *full TPDO-rate*
   stream to `logs/motor_0xNN_<localtime>.csv`. Each toggle-on opens a fresh
   file; the path is shown and copyable.
6. **控制**: pick a mode (locked once enabled), **使能 (Enable)**, then send a
   mode-specific target (**发送目标**). Adjust the `0x6072` **最大力矩** limit
   (permille, with the ≈Nm equivalent shown) in any mode. After init, faults
   are **not** auto-cleared — the panel surfaces them so you can decide
   (清除错误 + 重新初始化).

The numeric panel / chart poll `get_status` at ~20 Hz (velocity is already
filtered in Rust); CSV logging subscribes to the full TPDO stream separately.

> **MIT mode units are SI** (`pos` rad, `vel` rad/s, `kp` Nm/rad, `kd`
> Nm·s/rad, `tor` Nm). The GUI converts to the motor's native Rev internally
> (±2π); `kp`/`kd` are then mapped to integers via the cached `0x2003:07`
> factor by `hex-motor`.

## Tools

On launch you pick a tool (extensible for future utilities like zero-point
setting). The choice is made *before* connecting, which lets each tool open the
bus with the right settings:

- **Motor Control** — everything above. Broadcasts our heartbeat (the motor's
  `0x1016` consumer needs it).
- **Firmware Update** — an independent DFU workspace with a CAN/USB selector.
  The current USB path recognizes only exact Bootloader version `0x0100`,
  mapped locally to product code `0x6763616E` (ASCII `gcan`). Protected devices
  accept only strict `.hpmota` v2; development devices accept only structural
  plaintext APP0 `.bin`. Local selection does not bypass validation. HPM CAN is
  visible but disabled until hardware validation exists. A matching JUMP ACK
  means transfer completion, not confirmed application health; check the
  device's actual function after upgrading. The STM32 CAN page is currently a
  read-only safety preview: it passively discovers valid CANopen heartbeat
  nodes and strictly reads/classifies complete `0x1018` identities. Every known
  product profile remains disabled, so no proprietary object or CAN update
  write is sent. The common streaming engine is present behind the final
  same-transport identity gate, ready for a separately qualified product
  profile without duplicating the CLI protocol logic.
- **Lift (Raw CAN)** — direct CANopen commissioning for one `lift-driver`
  node (default `0x14`) on the already-open bus. Attach is observation-only:
  it reads identity, nameplate/CRC, effective limits, heartbeat, TPDOs and SDO
  diagnostics, including `0x4601:08..0B` sensor status, INA `DIAG_ALRT`, sample
  age and failure count, without changing NMT or sending motion. TPDO2 frame
  freshness and INA sample freshness are displayed and gated independently:
  stale V/I remain visible only as explicitly marked last-successful values.
  QEI readiness and the separately bench-qualified encoder direction are also
  distinct status bits; an initialized QEI never implies that “up counts
  positive” has been verified on the mechanism.
  A separate low-duty commissioning card is shown **only** for the exact pair
  0x1008:00 = "hexmeow-lift-commission" and 0x4700:01 U16 = 2. ABI1 and
  production images never expose these controls. ABI2 uses the frozen 0x4700
  record and exact 8-byte 0x4701:00 RPDO3
  (active_session:u32 + pulse_id:u16 + signed duty:i16).

  The device owns the anti-replay boot epoch and one-shot challenge. ARM echoes
  the currently displayed non-zero challenge with kind=Arm; that echoed value
  becomes the active session only after ArmedIdle + flags.ARMED confirmation.
  The active session is always an echoed device challenge. Clear-fault is a
  separate kind=ClearFault challenge path, enabled only while NMT is Operational
  and FaultLatched; after CAN E-stop the operator must explicitly return the
  node to Operational before clearing. The GUI displays boot epoch,
  challenge/kind, expected and active pulse IDs, qualified encoder sign, and
  the INA238 configuration-fingerprint mismatch bitmap.

  Stage A epoch establishment is offered only for MissingOrUnreadable or
  Corrupt continuity. The write is enabled only in NMT Pre-operational while
  the commissioning state is Disarmed, active_session=0, ARMED/OUTPUT flags
  are clear, boot_epoch=0, and the operator separately confirms that the motor
  is physically disconnected from the driver PCB. The backend rechecks that
  boolean, obtains a fresh non-zero u32 provisioning salt from the operating
  system random source, and writes that value to EPOCH_SERVICE; there is no
  fixed service magic. The salt is anti-stale framing rather than a secret or
  CAN authentication credential. Exhausted and WriteFailed are warning-only
  terminal states: the GUI deliberately exposes no service or retry button
  for them.

  Stage A may be performed only with the motor physically disconnected.
  Connecting the motor for Stage B remains blocked until the shared `hstd`
  persistence paths (`0x1010`, `0x1011`, and write-through Flash mutations)
  reject or defer work while the lift is Operational, armed, or output-capable;
  otherwise blocking Flash work can pause the cooperative supervisor.

  Rust owns the 20 ms RPDO3 stream. ArmedIdle sends zero keepalives; A/B
  hold-to-run repeats only the device-issued expected pulse ID and duty—the
  WebView never predicts a sequence number. Pointer release, window blur, or
  loss of the operator lease sends zero. The host mirrors the firmware-reported
  100 ms lease from 0x4700:08 instead of maintaining a longer timeout.
  Repeated frames cannot extend the firmware absolute pulse deadline.
  Firmware hard-cap, lease, and maximum-pulse values remain read-only in the
  UI; no host sensor gate assumes SAMPLE_VALID.

  TPDO3 (0x380 + node) and TPDO4 (0x480 + node) are paired by their u16
  firmware tick before display/recording. The latest 2,000 paired samples are
  kept in a bounded backend buffer (cleared by the next ARM) and can be copied
  as CSV; this is intentionally not durable file logging yet. Commissioning
  E-stop sends directed NMT Stop before waiting for SDO, then enters Pre-op
  and confirms active_session=0, state=Disarmed or FaultLatched, and clear
  ARMED/OUTPUT flags. It is a CAN
  software stop, not a safety-rated substitute for physical power removal.
  Generic Homing/Velocity/Position and the production Clear Fault command are
  disabled for every commissioning image; a latched commissioning fault can
  only use the dedicated kind=ClearFault challenge path above.
  Homing, velocity and position remain locked until heartbeat and both TPDOs
  are fresh, the encoder/INA sample is healthy, `CONFIG_VALID` is set, NMT is
  Operational, no fault is latched, and Homing has completed where required.
  Velocity is hold-to-jog: Rust owns the RPDO timing while the WebView renews a
  250 ms operator lease. Lease loss sends a
  directed NMT Stop. Detach/Disconnect and normal window close report success
  only after a Pre-operational heartbeat and Disabled-command readback; a
  failed close keeps the window open with `STOP UNCONFIRMED`. Position is an
  autonomous goal: confirmed shutdown cancels it, but a process crash cannot,
  so commissioning still requires a physical power-removal path. This tool
  does not broadcast a host heartbeat.
- **Device Settings** — the merged identity-gated Node-ID, CAN profile and
  motor-zero workspace. The sidebar shows every heartbeat-discovered node, but
  unknown `(vendor_id, product_code)` tuples have no actions. A known motor can
  change Node-ID, 1/2/4/5 Mbit/s data rate, TPDO BRS and its `0x3001` position
  preset; known non-motor products expose only the communication fields their
  actual object shape supports. Classic-only `0x2100:00 == 1` images never get
  data-rate/BRS controls. Communication Apply is accepted only while the last
  device heartbeat reports Pre-operational or Stopped; the tool does not issue
  NMT just to force that state.

  Every write button force-reads `0x1018` again and verifies the exact expected
  tuple in the same per-node exclusive SDO transaction. Motor `0x2001` changes
  are stored with one final `0x1010:01 = "save"` and take effect after a
  physical power cycle. `0x2100/0x2101` changes are write-through, are not
  followed by `0x1010`, and are never auto-reset; wait for persistence before
  restarting and verify after the next heartbeat. Position preset is a
  separate button transaction that confirms Switch On Disabled before writing
  `0x3001`.

  This workspace does **not** broadcast a host heartbeat. It sends CAN traffic
  only after receiving a device heartbeat or after a user click. Position is
  read once per online edge, on explicit Read, and once after preset; failed
  edge reads are not retried by the UI refresh. This avoids accumulating TX
  errors while a device is unplugged or the bus is empty.

Use **Switch tool** in the header to go back to the picker (it disconnects
first).

## CAN backend extension point

The GUI ships two backends: `socketcan` (Linux) and `gs_usb` (candleLight
over USB, CAN-FD, cross-platform), selected by the interface string. Adding
another backend is contained to `src-tauri/src/backend.rs` — add an arm to
`open_bus` returning an `Arc<dyn CanBus>`; nothing else in the GUI changes.
