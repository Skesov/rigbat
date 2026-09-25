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
- **Dashboard** — `rigbat dashboard`: the tray icon's left click, a row per shown device.

The core (`domain` + `sources` + `discovery` + `app`) has no knowledge of CLI tables, tray
icons, or GUI widgets. The surfaces are adapters on top of it.

## Layering and dependency direction

Dependencies point inward. `domain` is pure: it imports only `i18n` and no infrastructure crate
(`zbus`/`tiny-skia`/`nix`/`eframe`/`rusqlite`/`tokio`). Each outer layer may depend on inner
layers, never the reverse.

```text
        domain            pure types + logic (DeviceInfo, BatteryReading, PrimaryStatus,
          ▲               classify, guess_kind, freedesktop_icon_name, DeviceId, estimate,
          │               roster policy, device text, TrayState, DisplayMode/TrayMode)
        i18n              Fluent catalogues + Lang; pure, so domain may use it
        refresh           RefreshSignal, a tokio-only primitive with no crate imports
          │
        config            config.json: user intent, serialized; imports domain + i18n
          │
   icon / ipc / gui / appearance / clock   shared ports: domain, refresh and each other, never config
          │
        sources           BatterySource / BatteryBackend traits + Context + supervise +
          ▲               sysfs/bluez/steelseries/eightbitdo impls
        discovery         registry of backends + discover_all()
          ▲
          app             poll_once (list/--json) and Supervisor (tray, --waybar): orchestration
          ▲
   cli / tray / dashboard / settings   output + input adapters (table/json, ksni, egui windows)
   session / notifications / state / autostart                side services
```

`main.rs`, `app` and `doctor` are the composition roots and may import anything. Every other
module is checked by `tests/architecture.rs`, which strips `#[cfg(test)]` code and fails on a
dependency cycle or an edge outside these rules. Its few allowed sideways edges carry their
reason: `discovery → sources` (the registry), `session → sources` (the shared system bus it
listens on) and `settings` wiring its own process (`app::poll_once`, `discovery`, `state`,
`autostart`).

New infrastructure (a D-Bus client, a HID transport, a renderer) goes behind a **port** — a
trait in an inner layer — with the concrete dependency living in the implementation. This keeps
`domain` testable without a bus or a display and lets implementations be swapped.

`cli`, `tray` and `settings` sit at the same level, so none may import another: a sideways edge
is how one surface's incidental choice becomes another's contract. What they share moves inward
instead. Two things did, and they are the shape to copy — `domain::text` (`state_str`,
`format_age`, `format_device_entry`, `device_line`), because a device reads the same in the tray
menu, the dashboard, the CLI table and the Devices tab, and `domain::roster` (`Roster`,
`is_visible`, `roster_order`), because which devices a view lists, in what order (online first,
then by name), and which one the single view shows is policy, answered identically by the tray
icons and menu, the dashboard, `--waybar` and the CLI table. Both are pure, so both are tested
without a bus or a display.

Translated text takes the language as a parameter (`format_device_entry(state, now, lang)`),
the same way it takes `now`: the language changes at runtime and tests run in parallel, so no
global holds it. Machine-facing values stay English and separate from their labels —
`DeviceKind::as_str` and `state_str` feed `--json` and the inventory, `DeviceKind::label` and
`state_label` feed the UI. The CLI (`list`, `--json`, `--waybar`, `--help`) is never
translated.

## Ports (contracts)

- **`BatterySource`** (`sources`): `async fn poll(&mut self) -> Result<BatteryReading>` and
  `fn device(&self) -> &DeviceInfo`. One source = one device. Handle policy follows how the
  device talks, not a fixed rule: a request/response device (write a query, read the echo —
  SteelSeries) opens its handle once and holds it for the source's whole lifetime, since
  reopening per poll can deadlock it mid-exchange; a stream-only device (pushes input reports
  unprompted — 8BitDo in DInput) opens per poll and closes, since holding the handle open just
  queues discarded reports into the kernel's ring and nothing is written to deadlock. A new
  backend determines which kind it is before choosing.
- **`BatteryBackend`** (`sources`): `async fn discover(&self, ctx: &Context) ->
anyhow::Result<Vec<Box<dyn BatterySource>>>`. Finds devices and constructs sources; `Err` is a
  failed sweep, distinct from `Ok(vec![])`. A hidraw backend also returns its `HidrawFamily`
  (vendor, models, battery interface) from `hidraw_family()`: the shared discovery in
  `sources::hidraw`, `rigbat doctor` and the udev rule test all read that one table. Backends are
  listed in `discovery::registry::backends()`; what a backend must guarantee is the
  [backend contract](../CONTRIBUTING.md#backend-contract), and the steps to add one are its
  [checklists](../CONTRIBUTING.md#checklists). `Context` is the dependency container built at the
  composition root: it holds the process-wide system-bus connection (`sources/context.rs`),
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
  discovery: it runs `discover_all()` at start, every 30 s, and on every refresh request. The
  30 s cadence is a `tokio::time::interval` created once, not a `sleep` in the `select!` loop: a
  per-iteration sleep restarts on every reading, so with a few devices answering the sweep never
  fired. A refresh-triggered sweep resets the interval.
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
  (`Online`/`Unreachable`/`Disconnected`/`NoAccess`) alongside its last reading. A source that starts
  erroring flips to `Unreachable` without discarding that reading, so consumers can render
  "88% offline (2h ago)" instead of losing the value; `Disconnected` is reserved for a device
  reconcile no longer sees at all; such an entry is pruned from the roster once it has been gone
  for `DISCONNECTED_RETENTION` (24 h), or immediately if it never produced a reading. A source
  whose open fails with a permission error returns the typed `sources::AccessDenied`; the device
  reads `NoAccess` from the first such poll (no debounce — a denial is not a dropped packet), its
  task keeps polling so a newly installed udev rule takes effect, and every surface says "no
  access" and names `rigbat doctor` instead of "offline". The same
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
- **Shelf life**: dimming says "remembered", but a memory stops being worth a tray slot. A device
  that is not `Online` loses its icon and its menu row once `DeviceState::is_currently_informative`
  says no — its reading is older than `RETAINED_ICON_MAX_AGE` (24 h, matching the supervisor's
  `DISCONNECTED_RETENTION` so an icon never outlives the roster entry behind it), or it has no
  reading at all. The second case is the common one: a wireless dongle stays enumerated while its
  mouse is switched off, so the device is discovered, polled, and never answers — an empty battery
  outline that has never meant anything. Display only: the device stays in the roster, keeps being
  polled, and reappears on its next successful reading. The tray icons, the menu, the aggregate
  icon's pick and `--waybar` all go through one predicate (`domain::is_visible`, via
  `Roster::visible`), so they cannot disagree about which devices exist.
- **Estimate**: on every reading the manager derives a time-remaining estimate
  (`domain::estimate`) from the device's percent-change history. Only a discharging, non-coarse
  reading gets one. The rate runs from the first edge (a moment the percent was seen to change) to
  the last, never from the first observation, which only says when a level was first seen. A rise
  or a drop above 5 % restarts the window at that edge instead of disabling the estimate. Fewer
  than two edges or under 30 min between them is no estimate. The tray menu and the dashboard
  render it via `format_coarse` ("~3h"), capped at ">4d" (">4 д") — beyond that the evidence is a
  handful of edges days apart.
- **Clock**: reading times, ages and the estimate use `domain::BootTime`, read from
  `CLOCK_BOOTTIME` by `clock::now`. Unlike `Instant` (`CLOCK_MONOTONIC`) it counts suspend, so a
  discharge across a night's suspend is not squeezed into seconds, and the 24 h retention caps are
  24 h of real time, not of awake time.

## Data flow per surface

```text
CLI:       main → discover_all() → poll_once() (poll all in parallel) → cli::print_*  → exit

Waybar:    main → Supervisor::spawn(config_rx) ──watch<TrayState>──▶ cli::waybar::run
                                                                    │ render_waybar_line,
                                                                    │ printed only when the line
                                                                    │ changes, no exit
           session (logind PrepareForSleep) ──RefreshSignal──────────▶ Supervisor (re-poll + re-discover)
           bluez D-Bus signals (debounced) ────RefreshSignal──────────▶ Supervisor (re-poll + re-discover)
           config file watch ──watch<Config>──▶ loop (live primary_device/hidden_devices)

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
           org.rigbat.Tray1.State() ──JSON Snapshot (shown + hidden)──▶ device list
             (no tray on the bus: discover_all + poll_once in the settings process)
           state store (SQLite) ──▶ device table rows for devices not currently present
           tray's config file watch reloads ──watch<Config>──▶ live update

Dashboard: tray left click (SNI Activate) → spawn `rigbat dashboard` (separate process)
           org.rigbat.Tray1.State() ──JSON Snapshot──▶ cards
           org.rigbat.Tray1.StateChanged ──▶ re-read, repaint
           second launch → org.rigbat.Dashboard1.Close() on the open one → exit
```

The dashboard is a view of the tray's state, not a second monitor: the tray serves
`org.rigbat.Tray1` on the connection that holds its single-instance name, and the dashboard never
polls a device. It opens instantly, wakes nothing, and cannot classify a device differently from
its tray icon — the snapshot carries each device already classified by `domain::device_status`,
and the card draws its glyph with the same `icon::TinySkiaRenderer` the tray uses. It also lists
devices the tray has dropped after a day of silence, dimmed and last; hidden ones never appear.
If the tray goes away the window says so, and it reloads when the tray comes back.

The settings window reads the same snapshot, which also lists hidden devices apart
(`Snapshot::hidden`) and carries each device's locator, so its rows match inventory records. It
has the tray re-poll on its Refresh button (`Refresh()`, then the next `StateChanged`, bounded at
3 s, then `State()`), and polls devices itself only when nothing owns `org.rigbat.Tray`: a
request/response device such as SteelSeries can interleave two processes' exchanges.

The settings window is a separate process on purpose: it owns the winit event loop, and eframe is
built with `default-features = false` (glow backend). The `accesskit` feature is on: its AT-SPI
bridge runs zbus on a thread of its own with the `async-io` backend, which zbus >= 5.19 selects
per thread when no tokio runtime is current there, so it needs no runtime from us. The
window communicates with the tray only through `config.json`.

It does hold a tokio runtime, but nothing on the UI thread ever enters it: device discovery is
spawned onto it and the result arrives over an `mpsc` channel drained with `try_recv` at the top
of the frame, so the "Refresh" button cannot block the event loop. The runtime is dropped when the
window closes. Dropping it cancels the async part of an in-flight scan but waits for its
`spawn_blocking` calls — the hidraw backends' sysfs walk and every hidraw `poll` run there — so
each blocking call a backend makes is bounded: a sysfs read, or a `nix::poll` with the
backend's timeout (at most 1 s).

## Configuration and persistence

Two stores, split by what the data **is**, not by convenience:

| Store                                 | Holds                                                         | If it is lost                     |
| ------------------------------------- | ------------------------------------------------------------- | --------------------------------- |
| `$XDG_CONFIG_HOME/rigbat/config.json` | user intent (thresholds, intervals, which devices are hidden) | the user's decisions are gone     |
| `$XDG_STATE_HOME/rigbat/rigbat.db`    | observations (device inventory, reading history)              | rebuilt by watching devices again |

The XDG spec defines `STATE_HOME` as data "not important enough" for `DATA_HOME` — a directory
whose loss must be survivable. Decisions therefore cannot live there, and an append-only
observation log has no business in a file the user edits by hand. Desktop practice splits the
same way: Chrome keeps `Preferences` as JSON beside `History` as SQLite.

- `config` module, XDG `~/.config/rigbat/config.json`, `serde`. Every field is
  `#[serde(default)]` at the struct level, so older config files load and missing keys fall back
  to `Config::default()`.
- `language` holds the UI language tag picked in the settings window; `None` follows
  `LC_ALL`/`LC_MESSAGES`/`LANG` (and `LANGUAGE`), as gettext does. Like every other field it
  reaches the tray through the file watch, so switching needs no restart.
- Saves are atomic: write a temp file, then rename.
- A `notify` file watcher pushes reloads into a `watch<Config>` channel. It reacts only to
  content-changing events (create / data write / rename), never to `Access` events — reacting to
  reads would make the watcher's own `load()` feed an infinite loop.
- Per-device overrides: `device_overrides: HashMap<name, DeviceSettings>` with optional poll
  interval and low threshold; `Config::effective_*` resolve override → global → built-in default.
- `primary_device` pins the device the aggregate icon features; `None` means the first connected
  visible device. It is set from the selected device's settings under the Devices tab's table — one
  device at a time, so it is an action on a device rather than a column every row would carry — and
  the General tab names the current choice beside the single-icon option instead of repeating the
  control, plus a `Clear` button — the one action that needs no row, and therefore the way out of
  a pin naming a device the inventory has no row for (retired before the inventory existed, or
  deleted since). Forgetting a device clears its pin along with its inventory row.
  In single-icon mode the tray menu sets it too: an `Automatic` checkmark clears it and a checkmark
  per shown device pins that device.
- Visibility is recorded as **who to hide** (`hidden_devices`), not who to show. A whitelist has
  to be rebuilt from the devices visible at that moment, so editing it from a partial view
  silently drops every device the view did not contain — which is exactly what made checkboxes
  reset themselves. An exclusion list is only ever edited by the one name a toggle mentions, so a
  partial view cannot damage what it cannot see. The old `shown_devices` whitelist still
  deserializes and is converted once, by `app::migration` (driven by the supervisor) after a
  discovery sweep that has seen every backend report in — never by `config::load`, which has no
  device roster to convert against. If a backend is still missing after `MIGRATION_MAX_SWEEPS`
  sweeps, the conversion runs anyway rather than blocking visibility forever.
- `state` module (`rusqlite`, bundled SQLite): the device inventory (first seen, last seen,
  transport, kind) and a reading history collapsed to change points (`LAG()` over equal-percent
  runs). Schema version lives in `PRAGMA user_version`; WAL plus `busy_timeout` plus
  `BEGIN IMMEDIATE` let the tray and the settings process write the same file;
  `synchronous=NORMAL` (safe in WAL: power loss can drop the last commits, never corrupt the file)
  and a 2 MiB `journal_size_limit` keep the fsync count and the WAL file small. Inside a process
  the connection belongs to one dedicated thread (`state::Store`, an actor): callers send a
  request and await a oneshot reply, and a reading is queued without waiting. A write the other
  process holds can stall that thread for up to `busy_timeout`, never a tokio worker. The store is
  **optional**: if it cannot be opened, the error is logged and monitoring continues without it —
  history is a convenience, not a prerequisite for reading a battery.
- What the history is _for_: the time-remaining estimate needs a run of percent changes, and a
  restart used to throw that away, so every device showed no estimate until it had discharged a
  few percent again. `reconcile` seeds a newly-discovered device's in-memory history from the
  store (`seed_history`, `HISTORY_CAP` = 20 change points, most recent first). Each stored
  wall-clock timestamp becomes a `BootTime` once, by its age; `BootTime` is signed, so a reading
  from before this boot keeps its place.
- Retention: `state::spawn_retention` prunes readings older than `RETENTION_SECS` (14 days) every
  `RETENTION_INTERVAL` (1 h), and drops readings orphaned by a deleted device. Inventory rows are
  never pruned on age — a device you own but have not switched on for a month must still be in the
  table you manage it from. Storing change points rather than samples is what keeps 14 days small:
  a device at a steady 80% writes one row, not one per poll. A reading is stored only when its
  percent or charge state differs from the device's last stored row, or that row is an hour old
  (`READING_HEARTBEAT_SECS`); a heartbeat repeats the percent, so the `LAG()` collapse drops it
  and the seeded history is the same change points. Each sweep upserts all its devices'
  inventory rows in one transaction, and `last_seen` moves only once it lags by
  `LAST_SEEN_RESOLUTION_SECS` (5 min). The age prune uses an index on `readings.at` (schema v4).

### Device identity

`DeviceId` is `(name, transport, locator)` — the tuple the supervisor keys sources by, the
notification tracker keys streaks by, and the state store enforces `UNIQUE` on. All three break
if the locator changes while the device does not: the retained reading is dropped, the low-battery
streak restarts, and the inventory grows a second row with a fresh "first seen".

The hidraw backends originally used the `hidrawN` node name, which the kernel assigns in
enumeration order — so one controller produced a new identity on every replug, observed live as
two inventory rows for the same 8BitDo. `sources::hidraw::stable_locator` replaces it with, in
order, the device's own serial (`HID_UNIQ`), its USB topology path (`HID_PHYS`), then the node name
as a last resort. A device with no serial is therefore identified by the port its dongle sits in —
moving the dongle reads as a different device, which is as far as the hardware allows.

The sysfs backend had the same defect one layer over: it used the `power_supply` directory name,
and `hid-logitech-hidpp` builds that from a module-global counter —
`n = atomic_inc_return(&battery_no) - 1; sprintf(battery->name, "hidpp_battery_%ld", n)` — so a
Logitech mouse is `hidpp_battery_6` now and something else after the next reconnect. The kernel
registers the supply with the HID device as its parent, so `<supply>/device/uevent` is that
device's uevent and the same serial-then-USB-path preference applies. A supply with no HID parent
keeps its directory name.

The remaining mutable part of the identity is the **name**, and it is mutable by the user: a BlueZ
alias edit renames a device while its hardware identity stays put. `record_seen` treats "same
transport, same non-NULL locator, different name" as a rename — the row is renamed in place,
keeping its history and first-seen date — and returns `Seen::Renamed`, which the supervisor turns
into a `config::rename_device` call so `hidden_devices`, `device_overrides` and `primary_device`
follow the device instead of silently resetting it to defaults. The live roster is keyed by the
same identity, so `reconcile` also forgets the entry under the old name immediately: left to the
vanished path it would sit there as a `Disconnected` duplicate for `DISCONNECTED_RETENTION`, one
device showing twice in the tray for a day while the Devices tab already shows it once. A NULL locator never matches: with
no locator the only thing left is the name, and "same transport, no locator" would fuse two
unrelated devices the moment one was renamed.

Schema v2 and v3 delete the rows the old locators wrote rather than trying to merge them: two rows
with the same name are equally consistent with one device replugged and two identical devices, and
a guess here would silently fuse two devices' histories. v3 drops **all** sysfs rows, not only the
`hidpp_battery_*` ones, because which stored locator the new scheme reproduces cannot be known
without the device present.

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
