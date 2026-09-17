# Architecture

How rigbat is put together: the layers, the ports between them, the concurrency model, and how
data flows through each of the three surfaces. For build/test/lint and conventions see
[`../CLAUDE.md`](../CLAUDE.md); for adding a device see [`../CONTRIBUTING.md`](../CONTRIBUTING.md).

## Overview

rigbat reads battery levels from peripherals and presents them. One binary, three surfaces over
a shared headless core:

- **CLI** — `rigbat list` / `--json` / `--wide`: a one-shot poll printed and exit. `--waybar` is
  the exception: it holds its own `Supervisor` and runs continuously, printing one line per
  state change, so a retained reading survives a device going unreachable the same way it does
  in the tray.
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
          ▲               classify, guess_kind, freedesktop_icon_name, DeviceId, estimate)
          │
        sources           BatterySource / BatteryBackend traits + sysfs/bluez/steelseries impls
          ▲
        discovery         registry of backends + discover_all()
          ▲
          app             poll_once (list/--json) and Supervisor (tray, --waybar): orchestration
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
  holds it for its whole lifetime — it does not reopen per poll (reopening per poll can
  deadlock the device).
- **`BatteryBackend`** (`sources`): `async fn discover(&self, ctx: &discovery::Context) ->
Vec<Box<dyn BatterySource>>`. Finds devices and constructs sources. Backends are listed in
  `discovery::registry::backends()`. `Context` is the dependency container built at the
  composition root: it holds the process-wide system-bus connection (`discovery/context.rs`),
  opened lazily on first use and re-dialled if it has since closed, so a `dbus-daemon` restart
  does not strand the BlueZ backend for the life of the process. A backend that needs no
  infrastructure ignores the parameter.
- **`IconRenderer`** (`tray`): `render(status, kind, theme, mode) -> Vec<ksni::Icon>`. The tray
  depends on this trait, not on the renderer. The implementation is `tiny-skia`; an SVG/resvg
  renderer would be a new implementation behind the same port.

## Concurrency model (tray)

`tokio`, message-passing, **no `Mutex` on shared data**. The `Supervisor` owns all state; data
flows out through channels.

- Before any of this starts, `run_tray` claims `org.rigbat.Tray` on the session bus
  (`tray::single_instance`, `DoNotQueue`, no `ReplaceExisting`). A second `rigbat tray` (e.g. the
  systemd service and the "Start with session" autostart entry both enabled) sees the name
  already owned, prints one line to stderr, and exits 0 instead of publishing a duplicate set of
  `ksni` icons — `ksni`'s own `org.kde.StatusNotifierItem-<pid>-<n>` is PID-keyed and never
  collides, which is why that duplication otherwise goes unnoticed.
- `Supervisor::spawn(config_rx)` starts a **manager task** holding a `DeviceRegistry` — the
  device order, infos, readings and per-device task handles, keyed by `DeviceId`. It owns
  discovery: it runs `discover_all()` at start, every 30 s, and on every refresh request.
- Each discovered device gets its own **source task** that polls on that device's effective
  interval and sends `(DeviceId, reading)` to the manager over an `mpsc` channel. One failing
  source never affects the others.
- **Refresh** ("re-poll and re-discover now") is a `RefreshSignal`: a `watch` channel carrying a
  generation counter, not a `Notify`. `Notify::notify_waiters` wakes only the waiters registered
  at that instant, so a refresh fired while a task sat in `poll().await` or mid-discovery was
  lost. A `watch` retains the bump, so a busy task observes it on its next wait.
- Config reaches the supervisor as a `watch::Receiver<Config>`, not a snapshot. A source task
  re-reads its interval every iteration and wakes on `config_rx.changed()`, so a changed poll
  interval applies without restarting the tray.
- The manager publishes a `TrayState { devices }` snapshot through a `watch` channel after each
  reading or discovery change. `TrayState` carries raw observations only — which device is
  featured and whether it is low is decided by each consumer against the live config, so there
  is exactly one interpretation path instead of two that can disagree.
- **Re-discovery / reconcile**: on each discovery sweep the manager diffs the live set against
  running tasks by `DeviceId` (`name` + `transport` + `locator`). New devices get a task;
  vanished devices have their task aborted; stable devices keep running untouched (their
  just-opened transient discovery handle is dropped). This is how hotplugged devices appear
  without a restart, and why the tray menu's **Refresh** re-discovers.
- **Presence and retention**: each device carries a `Presence`
  (`Online`/`Unreachable`/`Disconnected`) alongside its last reading. A source that starts
  erroring flips to `Unreachable` without discarding that reading, so consumers can render
  "88% offline (2h ago)" instead of losing the value; `Disconnected` is reserved for a device
  reconcile no longer sees at all; such an entry is pruned from the roster once it has been gone
  for `DISCONNECTED_RETENTION` (24 h), or immediately if it never produced a reading. The same
  sweep also inspects each task's
  `JoinHandle::is_finished()` — a task that panicked (as opposed to one reconcile aborted
  itself for a vanished device) is demoted to `Unreachable` and respawned rather than left
  silently dead.
- **Stale rendering**: a device that is `Unreachable`/`Disconnected` but still holds a reading
  keeps its normal icon with the fill dimmed by `STALE_ALPHA`, rather than falling back to the
  empty offline battery — Bluetooth peripherals sleep constantly, and the charge is still known.
  Only the fill dims: the outline and the digits carry the reading, so they stay at full strength
  and above the 3:1 WCAG 2.1 SC 1.4.11 floor for graphical objects (the standard's exemption
  covers inactive _controls_, not information displays). A `Low` status never dims at all — a
  warning that has gone stale is exactly the one that must not get quieter.
- **Estimate**: on every reading the manager derives a time-remaining estimate
  (`domain::estimate`) from the device's percent-change history, refusing rather than guessing
  when the evidence is thin (coarse-bucket readings, a short window, an uneven step rate — see
  the module doc for why). The tray menu renders it via `format_coarse` as e.g. "left" appended
  to the device line.

## Data flow per surface

```text
CLI:       main → discover_all() → poll_once() (poll all in parallel) → cli::print_*  → exit

Waybar:    main → Supervisor::spawn(config_rx) ──watch<TrayState>──▶ run_waybar loop
                                                                    │ cli::render_waybar_line,
                                                                    │ printed only when the line
                                                                    │ changes, no exit
           session (logind PrepareForSleep) ──RefreshSignal──────────▶ Supervisor (re-poll + re-discover)
           bluez D-Bus signals (debounced) ────RefreshSignal──────────▶ Supervisor (re-poll + re-discover)
           config file watch ──watch<Config>──▶ loop (live primary_device/shown_devices)

Tray:      main → Supervisor::spawn(config_rx) ──watch<TrayState>──▶ tray::manager::run
                                                                    │ reconciles ksni items,
                                                                    │ renders icons (IconRenderer)
           appearance (xdg portal) ──watch<ColorScheme>────────────┘
           session (logind PrepareForSleep) ──RefreshSignal──────────▶ Supervisor (re-poll + re-discover)
           bluez D-Bus signals (debounced) ────RefreshSignal──────────▶ Supervisor (re-poll + re-discover)
           config file watch ──watch<Config>──▶ manager + notifications (live)
           notifications task ◀── TrayState + Config (edge-triggered low-battery)

Settings:  tray menu "Settings…" → spawn `rigbat settings` (separate process)
           egui window edits config.json (atomic temp+rename)
           tray's config file watch reloads ──watch<Config>──▶ live update
```

The settings window is a separate process on purpose: it owns the winit event loop, and eframe is
built with `default-features = false` (glow backend, no accesskit — an AT-SPI/zbus bridge assumes
a runtime that eframe itself never enters). It communicates with the tray only through
`config.json`; there is no IPC.

It does hold a tokio runtime, but nothing on the UI thread ever enters it: device discovery is
spawned onto it and the result arrives over an `mpsc` channel drained with `try_recv` at the top
of the frame, so the "Rescan" button cannot block the event loop. The runtime is dropped when the
window closes; that is safe because no `spawn_blocking` is reachable from `discover_all` — an
in-flight scan is plain async work and is simply cancelled.

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
- **session** — listens to logind `PrepareForSleep`; on resume it fires the shared
  `RefreshSignal` so all sources re-poll and the manager re-discovers.
- **bluez event watcher** — BlueZ is a push interface, so it is not polled for change detection.
  `bluez::watch_events` subscribes to `InterfacesAdded`/`InterfacesRemoved` and to
  `PropertiesChanged` (`Battery1` always, `Device1` filtered to `Connected` — `Device1` emits
  constant RSSI noise) and fires the same `RefreshSignal`, debounced to at most one trigger per
  5 s because a refresh wakes every source task, including the blocking hidraw one. It is an
  optimisation, never a dependency: the 30 s discovery sweep remains the safety net.
- **notifications** — a hand-rolled `zbus` `org.freedesktop.Notifications` proxy; an
  edge-triggered tracker fires once per low-battery crossing, using each device's effective
  threshold, gated by `notifications_enabled`. It re-arms when the device charges, rises back
  above the threshold, or goes offline — so a device hovering at the threshold notifies once,
  not on every poll. A crossing must be confirmed by `LOW_CONFIRMATIONS` (2) _distinct_ readings
  before it fires — distinctness keyed on `last_seen`, because `TrayState` is republished on every
  state change, not once per poll. One bad sample from a noisy BLE device therefore costs nothing;
  a real low battery is announced one poll interval later than it used to be. The deliberate
  consequence: a device that reports low exactly once and then dies or vanishes is never announced,
  since a non-Online device is skipped and its streak can no longer advance.
- **autostart** — writes/removes `~/.config/autostart/rigbat.desktop`.

## Diagnostics

Diagnostics go through `tracing` to **stderr**; `println!` is reserved for CLI output on stdout,
so `rigbat --json` stays machine-parseable at any verbosity. The filter comes from `RIGBAT_LOG`,
falling back to `RUST_LOG`, defaulting to `warn` for the one-shot CLI modes (`list`, `--json`)
and `info` for the daemons (`tray`, `settings`, `--waybar`). Under systemd the journal captures
stderr directly, so no journald transport is linked in.

Level policy: `error` when the user loses a feature and must act; `warn` for degraded but
self-healing or optional behaviour (portal, logind, notifications or the config watcher being
unavailable); `info` for lifecycle events worth keeping in the journal; `debug` for per-poll and
per-discovery detail; `trace` for raw protocol bytes.

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
