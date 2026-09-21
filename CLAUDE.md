# rigbat

System tray battery monitor for gaming peripherals (Linux), written in Rust.

## Status

Working: `rigbat list` / `--json` / `--wide` / `--waybar` / `tray` / `settings`. Sources: sysfs,
bluez, steelseries, eightbitdo (via `discovery::discover_all`, re-discovered live so hotplugged
devices appear; BlueZ signals debounced, and a backend whose sweep _fails_ does not retire its
devices — an empty result and an error are different things). Devices retain their last reading
across drops (`Presence`: Online/Unreachable/Disconnected) and render dimmed while unreachable,
except a low reading, which never dims. Tray: left-click menu listing device status, device-type
glyph, light/dark theme, display modes, time-remaining estimate, low-battery notifications
(confirmed by two distinct readings), separate settings window with a device inventory table,
per-device poll intervals/thresholds and aggregate-icon pin, config persistence. A second `rigbat tray` exits instead of
doubling every icon. Diagnostics via `tracing`. Packaging: udev rule, desktop entry, systemd user
service.

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
  `org.rigbat.Tray` on the session bus before publishing anything (`tray::single_instance`); a
  second instance sees the name taken and exits 0 instead of doubling every tray icon.
- `rigbat settings` — GTK-free eframe/egui settings window in a SEPARATE process (the tray spawns
  it). It edits `config.json`; the tray applies changes via the file watch. It holds a tokio
  runtime only to run device discovery off the UI thread — the winit event loop is never entered
  from inside it, and eframe is built without accesskit for that reason.

## Toolchain and commands

- Rust stable, edition 2024, MSRV `1.96` (`rust-version` in `Cargo.toml`).
- Build: `cargo build`
- Run: `cargo run`
- Tests: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`

`Cargo.lock` is committed (project is a binary).

## Key decisions

- **Concurrency:** `tokio`, message-passing, no `Mutex` on shared data. Supervisor owns state; data flows through `mpsc` (readings) and `watch` (to tray, and for the refresh signal — a generation counter, `app::refresh::RefreshSignal`, not `Notify`, whose `notify_waiters()` drops triggers fired mid-poll). Each source is a separate task with its own interval; a panicking source is detected on the next discovery sweep (`is_finished()`) and respawned, not left silently dead.
- **Source port:** `trait BatterySource { async fn poll(&mut self) -> Result<BatteryReading> }`. Handle policy is not universal — it follows how the device talks, which is the vendor's choice, not rigbat's: a **request/response** device (write a query, read the echo — SteelSeries) holds its handle for the source's whole lifetime, because reopening per poll can deadlock it mid-exchange; a **stream-only** device (pushes input reports unprompted — 8BitDo in DInput at 1000 Hz) opens per poll and closes, because holding the handle open just queues reports into the kernel's ring for the whole interval between polls, almost all discarded, and there is nothing to deadlock since nothing is written. A new backend establishes which kind it faces — read the report descriptor, watch whether the device sends anything unprompted — before picking a policy; neither is the default.
- **Tray:** `ksni`, SNI-only. XEmbed is not embedded — closed by external `snixembed`. No GTK dependency.
- **Icon:** render behind the `IconRenderer -> Vec<ksni::Icon>` port (multiple sizes for HiDPI). Implementation in `tiny-skia`; migration to SVG/resvg is a new implementation behind the same port.
- **Extensibility:** sources are built-in adapters behind a trait, no dlopen plugins (YAGNI).
- **Two stores, split by what the data _is_:** `config.json` under `XDG_CONFIG_HOME` holds user
  intent (thresholds, intervals, `hidden_devices`); a SQLite database under `XDG_STATE_HOME` holds
  observations (device inventory, reading history). The XDG spec defines `STATE_HOME` as data not
  important enough for `DATA_HOME` — a directory whose loss must be survivable — so decisions do
  not belong there. Desktop practice agrees: Chrome keeps `Preferences` as JSON beside `History` as
  SQLite. The store is optional: if it cannot be opened, monitoring continues without it.
- **Hide, don't show:** the config records which devices to _hide_. A whitelist has to be rebuilt
  from whatever is connected at the moment, which silently drops the rest; an exclusion list is
  edited one entry at a time, so a partial view cannot damage what it cannot see.

Full rationale, data flow, and contracts: [`docs/architecture.md`](docs/architecture.md).

## Module structure

```text
src/
├── domain/        # types, classify, guess_kind, freedesktop_icon_name, estimate,
│                 # select_featured, device text (state_str/format_age/entry line)
├── sources/       # BatterySource + BatteryBackend; sysfs/bluez/steelseries/eightbitdo
├── discovery/     # discover_all + registry::backends() + Context (shared system bus) + backoff
├── cli/           # output adapter: table / --json / --wide / --waybar
├── tray/          # ksni + IconRenderer (tiny-skia) + device-type corner glyph
├── appearance/    # theme from xdg-portal (light/dark)
├── notifications/ # low-battery desktop notifications (zbus)
├── session/       # logind PrepareForSleep → resume re-poll
├── settings/      # eframe/egui settings window (separate process) + device table state
├── autostart/     # ~/.config/autostart/rigbat.desktop
├── app/           # poll_once + Supervisor (owns discovery) + refresh signal + wiring
├── state/         # SQLite device inventory + reading history (XDG_STATE_HOME)
└── config/        # XDG ~/.config/rigbat/config.json
```

Dependencies point inward: `domain` does not import `zbus`/`tiny-skia`/`nix`, and no adapter
imports another adapter — text or policy that `cli`, `tray` and `settings` all render lives in
`domain`, not in whichever surface happened to need it first.

## Conventions

- English-only repo content (comments, docs). Domain types, not stringly-typed.
- No `unwrap`/`expect` in non-test code — return `anyhow::Result` with context.
- HID via `/dev/hidraw` directly (no C `libhidapi`); BlueZ via `zbus` (no `bluer`/libdbus).
- Diagnostics go through `tracing`; `println!` is reserved for CLI output on stdout.
- These rules (plus no `unsafe`) are enforced by `[lints.clippy]` in `Cargo.toml`, not only by
  review — `cargo clippy --all-targets -- -D warnings` fails the gate on a violation.
  (`clippy.toml` only exempts `#[cfg(test)]` code from the no-unwrap rule.)

## Development

- Gates (must pass before commit): `cargo fmt`, `cargo build`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings`.
- Settings-window rendering has its own coverage: `settings::tests::painted_text` runs a frame in
  a headless `egui::Context` and returns the strings that survived their clip rectangle, so a
  widget drawn into a few pixels fails the test instead of shipping. Assert against what is
  painted, never only that the code ran.
- Adding infrastructure (a new D-Bus/HID/GUI dependency): put it behind a port (a trait) plus
  an implementation; never import it into `domain` — dependency direction stays inward.
- Where to change what: new device → `CONTRIBUTING.md`; CLI flag → `main.rs` dispatch + `cli/`;
  icon rendering → behind the `IconRenderer` port in `tray/`; persisted settings → `config/`
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
- `ksni` `RadioGroup`/nested submenus drop clicks — use plain `StandardItem` for actions;
  left-click opens the menu via `const MENU_ON_ACTIVATE: bool = true`.
