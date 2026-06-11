# rigbat

System tray battery monitor for gaming peripherals (Linux). Rust port of the proven Python utility `universal-battery-tray`.

## Status

Working: `rigbat list` / `--json` / `--wide` / `tray`. Sources: sysfs, bluez, steelseries (via
`discovery::discover_all`, re-discovered live so hotplugged devices appear). Tray: left-click
menu, device-type corner glyph, light/dark theme, display modes, low-battery notifications,
separate settings window, per-device poll intervals/thresholds, config persistence.

## Compatibility

DE-agnostic — runs on any desktop with a StatusNotifierItem tray host: KDE Plasma (native),
GNOME (AppIndicator extension), Waybar/wlroots, XFCE (via `snixembed`), COSMIC. All integration
is standard freedesktop: SNI (`ksni`), xdg-desktop-portal appearance, logind resume,
`org.freedesktop.Notifications`; battery data comes from BlueZ/sysfs/hidraw at the kernel level.
No DE-specific dependencies. COSMIC is the primary development/test environment and the strictest
SNI host, which is why the workarounds below are framed around it.

## Launch modes

One binary, three surfaces over a common headless core (`domain` + `sources`); `main.rs`
dispatches on the first argument and builds the tokio runtime only for the non-GUI modes.

- `rigbat` / `rigbat list` — one-shot table of charge levels (`app::poll_once`); `--wide` adds
  transport/locator columns.
- `rigbat --json` — machine-readable output.
- `rigbat tray` — SNI daemon (`Supervisor` + `ksni`), the long-running mode.
- `rigbat settings` — GTK-free eframe/egui settings window in a SEPARATE process with no tokio
  runtime (the tray spawns it). It edits `config.json`; the tray applies changes via the file watch.

## Toolchain and commands

- Rust stable, edition 2024, MSRV `1.96` (`rust-version` in `Cargo.toml`).
- Build: `cargo build`
- Run: `cargo run`
- Tests: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`

`Cargo.lock` is committed (project is a binary).

## Key decisions

- **Concurrency:** `tokio`, message-passing, no `Mutex` on shared data. Supervisor owns state; data flows through `mpsc` (readings) and `watch` (to tray). Each source is a separate task with its own interval; failure of one does not crash the others.
- **Source port:** `trait BatterySource { async fn poll(&mut self) -> Result<BatteryReading> }`. Source holds an open handle for its entire lifetime (does not reopen on each poll — fixes deadlocks in the original).
- **Tray:** `ksni`, SNI-only. XEmbed is not embedded — closed by external `snixembed`. No GTK dependency.
- **Icon:** render behind the `IconRenderer -> Vec<ksni::Icon>` port (multiple sizes for HiDPI). Implementation in `tiny-skia`; migration to SVG/resvg is a new implementation behind the same port.
- **Extensibility:** sources are built-in adapters behind a trait, no dlopen plugins (YAGNI).

Full rationale, data flow, and contracts: [`docs/architecture.md`](docs/architecture.md).

## Module structure

```text
src/
├── domain/        # types, classify, guess_kind, freedesktop_icon_name
├── sources/       # BatterySource + BatteryBackend; sysfs/bluez/steelseries
├── discovery/     # discover_all + registry::backends()
├── cli/           # output adapter: table / --json / --wide
├── tray/          # ksni + IconRenderer (tiny-skia) + device-type corner glyph
├── appearance/    # theme from xdg-portal (light/dark)
├── notifications/ # low-battery desktop notifications (zbus)
├── session/       # logind PrepareForSleep → resume re-poll
├── settings/      # eframe/egui settings window (separate process)
├── autostart/     # ~/.config/autostart/rigbat.desktop
├── app/           # poll_once + Supervisor (owns discovery) + wiring
└── config/        # XDG ~/.config/rigbat/config.json
```

Dependencies point inward: `domain` does not import `zbus`/`tiny-skia`/`nix`.

## Conventions

- English-only repo content (comments, docs). Domain types, not stringly-typed.
- No `unwrap`/`expect` in non-test code — return `anyhow::Result` with context.
- HID via `/dev/hidraw` directly (no C `libhidapi`); BlueZ via `zbus` (no `bluer`/libdbus).

## Development

- Gates (must pass before commit): `cargo fmt`, `cargo build`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings`.
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

COSMIC-specific — the strictest SNI host; these workarounds are safe everywhere:

- No hover tooltip for tray icons, so the icon image and the menu are the only identification
  channels (drives the device-type corner glyph and the full-roster menu).
- `ksni` `RadioGroup`/nested submenus drop clicks — use plain `StandardItem` for actions;
  left-click opens the menu via `const MENU_ON_ACTIVATE: bool = true`.
