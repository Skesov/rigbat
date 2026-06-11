# Architecture

How rigbat is put together: the layers, the ports between them, the concurrency model, and how
data flows through each of the three surfaces. For build/test/lint and conventions see
[`../CLAUDE.md`](../CLAUDE.md); for adding a device see [`../CONTRIBUTING.md`](../CONTRIBUTING.md).

## Overview

rigbat reads battery levels from peripherals and presents them. One binary, three surfaces over
a shared headless core:

- **CLI** — `rigbat list` / `--json` / `--wide`: a one-shot poll printed and exit.
- **Tray** — `rigbat tray`: a long-running StatusNotifierItem daemon.
- **Settings** — `rigbat settings`: a small GUI window, launched as a separate process.

The core (`domain` + `sources` + `discovery` + `app`) has no knowledge of CLI tables, tray
icons, or GUI widgets. The surfaces are adapters on top of it.

## Layering and dependency direction

Dependencies point inward. `domain` is pure and imports no infrastructure crate
(`zbus`/`tiny-skia`/`nix`/`eframe`). Each outer layer may depend on inner layers, never the
reverse.

```text
        domain            pure types + logic (DeviceInfo, BatteryReading, PrimaryStatus,
          ▲               classify, guess_kind, freedesktop_icon_name, DeviceId)
          │
        sources           BatterySource / BatteryBackend traits + sysfs/bluez/steelseries impls
          ▲
        discovery         registry of backends + discover_all()
          ▲
          app             poll_once (CLI) and Supervisor (tray): orchestration
          ▲
   cli / tray / settings  output + input adapters (table/json, ksni icons, egui window)
   appearance / session / notifications / config / autostart   side services
```

New infrastructure (a D-Bus client, a HID transport, a renderer) goes behind a **port** — a
trait in an inner layer — with the concrete dependency living in the implementation. This keeps
`domain` testable without a bus or a display and lets implementations be swapped.

## Ports (contracts)

- **`BatterySource`** (`sources`): `async fn poll(&mut self) -> Result<BatteryReading>` and
  `fn device(&self) -> &DeviceInfo`. One source = one device. A source opens its handle once and
  holds it for its whole lifetime — it does not reopen per poll (reopening per poll deadlocked
  the original Python tool).
- **`BatteryBackend`** (`sources`): `async fn discover(&self) -> Vec<Box<dyn BatterySource>>`.
  Finds devices and constructs sources. Backends are listed in `discovery::registry::backends()`.
- **`IconRenderer`** (`tray`): `render(status, kind, theme, mode) -> Vec<ksni::Icon>`. The tray
  depends on this trait, not on the renderer. The implementation is `tiny-skia`; an SVG/resvg
  renderer would be a new implementation behind the same port.

## Concurrency model (tray)

`tokio`, message-passing, **no `Mutex` on shared data**. The `Supervisor` owns all state; data
flows out through channels.

- `Supervisor::spawn(config)` starts a **manager task** that owns the device set
  (`order`/`infos`/`readings`) and a map of per-device task handles. It owns discovery: it runs
  `discover_all()` at start, every 30 s, and on every `refresh` notification.
- Each discovered device gets its own **source task** that polls on that device's effective
  interval and sends `(DeviceId, reading)` to the manager over an `mpsc` channel. One failing
  source never affects the others.
- The manager publishes a `TrayState { devices, primary, primary_status }` snapshot through a
  `watch` channel after each reading or discovery change.
- **Re-discovery / reconcile**: on each discovery sweep the manager diffs the live set against
  running tasks by `DeviceId = (name, transport, locator)`. New devices get a task; vanished
  devices have their task aborted; stable devices keep running untouched (their just-opened
  transient discovery handle is dropped). This is how hotplugged devices appear without a
  restart, and why the tray menu's **Refresh** re-discovers.

## Data flow per surface

```text
CLI:       main → discover_all() → poll_once() (poll all in parallel) → cli::print_*  → exit

Tray:      main → Supervisor::spawn(config) ──watch<TrayState>──▶ tray::manager::run
                                                                    │ reconciles ksni items,
                                                                    │ renders icons (IconRenderer)
           appearance (xdg portal) ──watch<ColorScheme>────────────┘
           session (logind PrepareForSleep) ──Arc<Notify> refresh──▶ Supervisor (re-poll + re-discover)
           config file watch ──watch<Config>──▶ manager + notifications (live)
           notifications task ◀── TrayState + Config (edge-triggered low-battery)

Settings:  tray menu "Settings…" → spawn `rigbat settings` (separate process)
           egui window edits config.json (atomic temp+rename)
           tray's config file watch reloads ──watch<Config>──▶ live update
```

The settings window is a separate process on purpose: it owns the winit event loop and runs with
**no tokio runtime** (eframe with `default-features = false`, glow backend, no accesskit — an
AT-SPI/zbus bridge would panic without a runtime). It communicates with the tray only through
`config.json`; there is no IPC.

## Configuration and persistence

- `config` module, XDG `~/.config/rigbat/config.json`, `serde`. Every field is
  `#[serde(default)]` at the struct level, so older config files load and missing keys fall back
  to `Config::default()`.
- Saves are atomic: write a temp file, then rename.
- A `notify` file watcher pushes reloads into a `watch<Config>` channel. It reacts only to
  content-changing events (create / data write / rename), never to `Access` events — reacting to
  reads would make the watcher's own `load()` feed an infinite loop.
- Per-device overrides: `device_overrides: HashMap<name, DeviceSettings>` with optional poll
  interval and low threshold; `Config::effective_*` resolve override → global → built-in default.

## Side services

- **appearance** — reads `org.freedesktop.appearance` color-scheme from xdg-desktop-portal and
  publishes light/dark through a `watch` channel; the tray re-renders on change.
- **session** — listens to logind `PrepareForSleep`; on resume it fires the shared `refresh`
  notify so all sources re-poll and the manager re-discovers.
- **notifications** — a hand-rolled `zbus` `org.freedesktop.Notifications` proxy; an
  edge-triggered tracker fires once per low-battery crossing, using each device's effective
  threshold, gated by `notifications_enabled`.
- **autostart** — writes/removes `~/.config/autostart/rigbat.desktop`.

## Compatibility and platform constraints

rigbat is desktop-environment-agnostic. The tray is a standard StatusNotifierItem and runs on any
SNI host — KDE Plasma (native), GNOME (AppIndicator extension), Waybar/wlroots, XFCE (via
`snixembed`), COSMIC. Theme, notifications, and resume use xdg-desktop-portal,
`org.freedesktop.Notifications`, and logind; battery data is read from BlueZ/sysfs/hidraw. There
are no DE-specific dependencies.

Hosts differ in capability. COSMIC — the primary development/test environment — has the youngest,
strictest SNI host (no hover tooltip, squares the icon, drops some menu-item clicks), and those
constraints shape a few rendering and menu choices. The agent-facing list of "things that will
break if you don't know them" lives in [`../CLAUDE.md`](../CLAUDE.md) under **Platform gotchas**.
