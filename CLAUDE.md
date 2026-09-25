# rigbat

System tray battery monitor for gaming peripherals (Linux), written in Rust.

## Status

Working: `rigbat list` / `--json` / `--wide` / `--waybar` / `tray` / `settings`. Sources: sysfs,
bluez, steelseries, eightbitdo (via `discovery::discover_all`, re-discovered live so hotplugged
devices appear; BlueZ signals debounced, and a backend whose sweep _fails_ does not retire its
devices — an empty result and an error are different things). Devices retain their last reading
across drops (`Presence`: Online/Unreachable/Disconnected/NoAccess) and render dimmed while unreachable,
except a low reading, which never dims. A device that is not online loses its tray icon once its
reading passes `RETAINED_ICON_MAX_AGE` (24 h) or if it never produced one — an enumerated dongle
whose mouse is switched off is not a battery level. It keeps being polled and returns on its next
answer. A device whose node the user may not open (`sources::AccessDenied`, typically a missing
udev rule) reads `NoAccess` ("no access", pointing at `rigbat doctor`) instead of offline: it
keeps its tray entry, never notifies, and is never featured over an online device. Tray: left click opens the dashboard (a row per device), right click the menu listing
device status (in single-icon mode a checkmark per device pins the icon to it; unpinned, it shows the online device with the lowest charge); device-type glyph, light/dark theme, display modes, time-remaining estimate, low-battery notifications
(confirmed by two distinct readings), separate settings window listing every device seen (expander rows),
per-device poll intervals/thresholds and aggregate-icon pin, config persistence. UI in English and
Russian (Fluent, `i18n/`), switchable live; the CLI stays English. A second `rigbat tray` exits
instead of doubling every icon. Diagnostics via `tracing`. Packaging: udev rule, desktop entry,
systemd user service.

## Compatibility

DE-agnostic: every integration is standard freedesktop (SNI, xdg-desktop-portal, logind,
`org.freedesktop.Notifications`), so any StatusNotifierItem host works and no DE-specific
dependency is linked in. COSMIC is the primary development/test environment and the strictest
SNI host, which is why the workarounds below are framed around it. Host list and the full
rationale: [`docs/architecture.md`](docs/architecture.md#compatibility-and-platform-constraints).

## Launch modes

One binary, three surfaces over a common headless core (`domain` + `sources`); `main.rs`
dispatches on the first argument and builds the tokio runtime only for the non-GUI modes.
`-h`/`--help` and `-V`/`--version` print and exit before that dispatch.

- `rigbat` / `rigbat list` — one-shot table of charge levels (`app::poll_once`); `--wide` adds
  transport/locator columns.
- `rigbat --json` — machine-readable output.
- `rigbat --waybar` — long-lived waybar custom-module: holds its own `Supervisor` and prints one
  JSON line for the featured device on startup and on every state change (run with `interval`
  omitted, not `tray`'s icon/notifications).
- `rigbat tray` — SNI daemon (`Supervisor` + `ksni`), the long-running mode. Claims
  `org.rigbat.Tray` on the session bus before publishing anything (`ipc::single_instance`); a
  second instance sees the name taken and exits 0 instead of doubling every tray icon. On the
  same connection it serves `org.rigbat.Tray1` (state snapshot, `Refresh`, `StateChanged`).
- `rigbat dashboard` — eframe window the tray's left click spawns: a row per shown device, read
  from `org.rigbat.Tray1`, never polled. Holds `org.rigbat.Dashboard`; a second launch closes the
  open one (the toggle) and exits.
- `rigbat doctor` — one-shot setup check (`doctor::run`): session bus, tray host, running tray,
  systemd unit + autostart both enabled, BlueZ, portal, read-write access to each supported
  hidraw node (via `discovery::registry::hidraw_families`), config and state DB. Prints
  `ok`/`warn`/`fail` with a fix per problem; exits 1 on any `fail`, warnings do not fail.
- `rigbat settings` — GTK-free eframe/egui settings window in a SEPARATE process (the tray spawns
  it). It edits `config.json`; the tray applies changes via the file watch. Its device list is
  the running tray's `org.rigbat.Tray1` snapshot; only with no tray does it discover and poll
  itself. It holds a tokio runtime only for that work off the UI thread — the winit event loop is
  never entered from inside it. Both windows export an AT-SPI tree via eframe's `accesskit` feature.

## Toolchain and commands

- Rust stable, edition 2024, MSRV `1.96` (`rust-version` in `Cargo.toml`).
- Build: `cargo build`
- Run: `cargo run`
- Tests: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`

`Cargo.lock` is committed (project is a binary).

## Key decisions

- **Concurrency:** `tokio`, message-passing, no `Mutex` on shared data (`disallowed-types` in `clippy.toml`). Supervisor owns state; data flows through `mpsc` (readings) and `watch` (to tray, and for the refresh signal — a generation counter, `refresh::RefreshSignal`, not `Notify`, whose `notify_waiters()` drops triggers fired mid-poll). Each source is a separate task with its own interval; a panicking source is detected on the next discovery sweep (`is_finished()`) and respawned, not left silently dead.
- **Source port:** `trait BatterySource { async fn poll(&mut self) -> Result<BatteryReading> }`. Handle policy is not universal — it follows how the device talks, which is the vendor's choice, not rigbat's: a **request/response** device (write a query, read the echo — SteelSeries) holds its handle for the source's whole lifetime, because reopening per poll can deadlock it mid-exchange; a **stream-only** device (pushes input reports unprompted — 8BitDo in DInput at 1000 Hz) opens per poll and closes, because holding the handle open just queues reports into the kernel's ring for the whole interval between polls, almost all discarded, and there is nothing to deadlock since nothing is written. A new backend establishes which kind it faces — read the report descriptor, watch whether the device sends anything unprompted — before picking a policy; neither is the default.
- **Tray:** `ksni`, SNI-only. XEmbed is not embedded — closed by external `snixembed`. No GTK dependency.
- **Icon:** render behind the `IconRenderer -> Vec<ksni::Icon>` port (multiple sizes for HiDPI). Implementation in `tiny-skia`; migration to SVG/resvg is a new implementation behind the same port.
- **Extensibility:** sources are built-in adapters behind a trait, no dlopen plugins (YAGNI).
- **Two stores, split by what the data _is_:** `config.json` under `XDG_CONFIG_HOME` holds user
  intent (thresholds, intervals, `hidden_devices`); a SQLite database under `XDG_STATE_HOME` holds
  observations (device inventory, reading history). The XDG spec defines `STATE_HOME` as data not
  important enough for `DATA_HOME` — a directory whose loss must be survivable — so decisions do
  not belong there. Desktop practice agrees: Chrome keeps `Preferences` as JSON beside `History` as
  SQLite. The store is optional: if it cannot be opened, monitoring continues without it. One
  thread owns the connection (`state::Store`, an actor); async callers await its reply.
- **Hide, don't show:** the config records which devices to _hide_. A whitelist has to be rebuilt
  from whatever is connected at the moment, which silently drops the rest; an exclusion list is
  edited one entry at a time, so a partial view cannot damage what it cannot see.

Full rationale, data flow, and contracts: [`docs/architecture.md`](docs/architecture.md).

## Module structure

```text
src/
├── domain/        # types, classify, guess_kind, freedesktop_icon_name, estimate,
│                 # roster policy, device text (state_str/format_age/entry line),
│                 # TrayState, DisplayMode/TrayMode/Palette
├── refresh.rs     # RefreshSignal: "re-poll and re-discover now" generation counter
├── clock.rs       # clock::now(): domain::BootTime from CLOCK_BOOTTIME (counts suspend)
├── sources/       # BatterySource + BatteryBackend + Context (shared system bus) +
│                 # supervise (bus-watcher retry); sysfs/bluez/steelseries/eightbitdo
├── discovery/     # discover_all + registry::backends()
├── cli/           # output adapter: table / --json / --wide; waybar.rs: --waybar line + loop
├── tray/          # manager.rs (icon set + run loop), item.rs (SNI item + menu),
│                 # resolve.rs (device per icon), launch.rs, state service for the dashboard
├── icon/          # IconRenderer (tiny-skia) + device-type corner glyph, shared by tray/dashboard
├── gui/           # shared egui theme for dashboard and settings
├── palette.rs     # colour tables per Palette + WCAG readable(): shared by icon/gui/settings
├── dashboard/     # eframe device overview (separate process, left click)
├── ipc/           # session-bus names, Snapshot contract, proxies, single-instance claim
├── appearance/    # theme from xdg-portal (light/dark)
├── notifications/ # low-battery desktop notifications (zbus)
├── session/       # logind PrepareForSleep → resume re-poll
├── settings/      # eframe/egui settings window (separate process): mod.rs shell,
│                 # general_tab.rs, devices_tab.rs, devices.rs (row state), widgets.rs,
│                 # scan.rs (tray roster, or a local poll when no tray runs)
├── autostart/     # ~/.config/autostart/rigbat.desktop + systemd user unit state
├── doctor/        # `rigbat doctor`: setup checks with a fix per problem
├── i18n/          # Lang, per-language Fluent loaders, locale detection (catalogues in /i18n)
├── app/           # poll_once + Supervisor (owns discovery) + wiring; migration.rs:
│                 # one-time shown_devices → hidden_devices conversion
├── state/         # SQLite device inventory + reading history (XDG_STATE_HOME)
└── config/        # XDG ~/.config/rigbat/config.json
```

Dependencies point inward: `domain` imports only `i18n` and no infrastructure crate
(`zbus`/`tiny-skia`/`nix`/…), and no adapter imports another adapter — text or policy that
`cli`, `tray` and `settings` all render lives in `domain`, not in whichever surface happened to
need it first. Adapters may use `domain`, `refresh`, `config` and the shared ports (`i18n`,
`icon`, `ipc`, `gui`, `appearance`, `clock`, `palette`), which themselves never import `config`; `main.rs`, `app`
and `doctor` are the composition roots. The few allowed sideways edges (`discovery → sources`,
`session → sources`, `settings` wiring its own process) are listed in `tests/architecture.rs`,
which fails on any other edge and on any cycle.

## Conventions

- English-only repo content (comments, docs). Domain types, not stringly-typed.
- UI text comes from `i18n/<lang>/rigbat.ftl` via `fl!`, never a literal; a wire value
  (`as_str`, `state_str`) is never translated.
- No `unwrap`/`expect` in non-test code — return `anyhow::Result` with context.
- HID via `/dev/hidraw` directly (no C `libhidapi`); BlueZ via `zbus` (no `bluer`/libdbus).
- Diagnostics go through `tracing`; `println!` is reserved for CLI output on stdout.
- These rules (plus no `unsafe`) are enforced by `[lints.clippy]` in `Cargo.toml`, not only by
  review — `cargo clippy --all-targets -- -D warnings` fails the gate on a violation.
  (`clippy.toml` only exempts `#[cfg(test)]` code from the no-unwrap rule.)
- Suppressions are `#[expect(...)]`, never `#[allow(...)]`: `allow_attributes = "deny"` makes
  that the only option, and clippy reports an `expect` that has stopped being needed. Each one
  says above it why the rule does not apply there.

## Development

- Gates (must pass before commit): `cargo fmt`, `cargo build`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings`.
- Window rendering has its own coverage: `egui_test` runs frames in a headless `egui::Context`
  and returns what was painted, whole or cut, and how many lines it wrapped to, so a widget drawn
  into a few pixels or a label that wraps fails the test instead of shipping. Assert against what
  is painted, never only that the code ran.
- UI conventions (principles, tokens, components, wording, UI tests): [`docs/ui-design.md`](docs/ui-design.md).
- D-Bus loops are tested on a private bus (`bus_test`: its own `dbus-daemon`, the test re-run in a
  child process pointed at it). zbus spawns tasks with `tokio::spawn` when an object server starts
  and when a proxy or signal stream is dropped, so both must happen inside the runtime — a window
  process's bus setup is tested from a plain `#[test]` for exactly that reason.
- hidraw nodes are opened through `sources::hidraw::open_verified`: the node's uevent identity is
  re-checked after every open, because a replugged device can take over a recycled `hidrawN`.
- Adding infrastructure (a new D-Bus/HID/GUI dependency): put it behind a port (a trait) plus
  an implementation; never import it into `domain` — dependency direction stays inward.
- Where to change what: new device → `CONTRIBUTING.md`; CLI flag → `main.rs` dispatch + `cli/`;
  icon rendering → behind the `IconRenderer` port in `tray/`; UI text or a new language →
  `i18n/` (see `CONTRIBUTING.md`); persisted settings → `config/`
  (serde, `#[serde(default)]` so old config files keep loading).
- Commits: Conventional Commits — `type(scope): description` (e.g. `feat(tray): …`).

## Platform gotchas

General — apply on every host:

- Most SNI hosts fit the icon into a square slot — keep pixmaps square and fill them.
- `tiny-skia` can panic on thin anti-aliased rects — all paints use `anti_alias = false`. Pure
  rendering, unrelated to any desktop.
- `ksni` does not re-publish the icon after a menu event — menu actions that change the icon must
  route config through the `watch` channel so the main loop calls `handle.update`.
- A udev rule granting `TAG+="uaccess"` must sort lexically before `73-seat-late.rules` — that
  file applies the ACL for tagged devices, and a rule numbered `99-` installs and verifies
  cleanly but silently grants no access. The shipped rule is `70-rigbat.rules`.

COSMIC-specific — the strictest SNI host; these workarounds are safe everywhere:

- No hover tooltip for tray icons, so the icon image and the menu are the only identification
  channels (drives the device-type corner glyph and the full-roster menu).
- `ksni` `RadioGroup`/nested submenus drop clicks — use plain `StandardItem` for actions and
  `CheckmarkItem` for a choice (`cosmic-applets` 1.8 draws `toggle-state 1` as a checkmark). A
  disabled item gets no press handler. Labels follow the DBusMenu mnemonic rule: a single `_`
  is swallowed, so device names are escaped to `__`.
- `cosmic-applets` 1.8: left press calls `Activate(0, 0)` unless `ItemIsMenu`, right press opens
  the menu, so `MENU_ON_ACTIVATE = false` and `activate` spawns the dashboard. The coordinates
  are always zero and a Wayland toplevel cannot place itself — the dashboard opens where the
  compositor puts it. The applet's `ProvideXdgActivationToken` call fails (ksni lacks it), so
  the window may open unfocused.
