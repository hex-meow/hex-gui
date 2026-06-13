# hex-motor GUI (Tauri)

## todo

Add LICENSE file.

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

## Run

### Dev (hot-reload, recommended)

Uses `tauri-cli`, which runs `npm run dev` (Vite at `:1420`) and the Rust app
together:

```bash
cargo install tauri-cli --version "^2" --locked   # once
cd tauri-test/src-tauri
cargo tauri dev
```

### Quick run (no tauri-cli)

Build the frontend, then run the Rust binary directly (it embeds `dist/`):

```bash
cd tauri-test
npm run build
cd src-tauri && cargo run
```

(Repeat `npm run build` after frontend changes, since `cargo run` embeds the
built `dist/` rather than talking to the Vite dev server.)

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

`.github/workflows/release.yml` builds on `ubuntu-22.04`: it uploads the
`.deb` + `.AppImage` as run artifacts on pushes/PRs, and on a `v*` tag creates
a **draft** GitHub Release with them attached. (It will fail until the
`hex-motor` path dependency in `src-tauri/Cargo.toml` is switched to a
git/crates.io source — see the note at the top of the workflow.)

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
- **Change ID** — batch-friendly Node-ID changer. Connect, pick a motor (or
  type its current ID), enter a new ID, **Write & Save** (writes `0x2001:01`
  then `0x1010:01 = "save"`). The change takes effect **only after a
  power-cycle**. The sidebar shows all heartbeat-discovered nodes live, so you
  can power-cycle a motor and watch its new ID appear (old one goes offline;
  **Forget offline** prunes stale entries). No app restart needed between
  motors. **This tool does NOT broadcast our heartbeat** — otherwise powering
  the (only) motor off would leave our frames unACKed and flood the bus with
  CAN errors.

- **Set Zero** — user-position-preset (zero-point) tool. Also RX-only (no
  heartbeat). Pick a motor (or type its ID), optionally **Read position**
  (one-shot `0x6064` read), enter the desired position (rev, −0.5..0.5) and
  **Save as preset** — writes `0x3001:01` then `0x3001:02 = "pres"`, which sets
  the motor's *current* rotor position to that value (motor must be in Switch
  On Disabled, i.e. freshly powered). Position is read only on demand: once per
  discovery, once 20 ms after a save, and on button click — never polled (to
  avoid TX-without-ACK when motors get powered off).

Use **Switch tool** in the header to go back to the picker (it disconnects
first).

## CAN backend extension point

The GUI ships two backends: `socketcan` (Linux) and `gs_usb` (candleLight
over USB, CAN-FD, cross-platform), selected by the interface string. Adding
another backend is contained to `src-tauri/src/backend.rs` — add an arm to
`open_bus` returning an `Arc<dyn CanBus>`; nothing else in the GUI changes.
