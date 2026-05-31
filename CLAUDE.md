# rigbat

System tray battery monitor for gaming peripherals (Linux). Rust port of the proven Python utility `universal-battery-tray`.

## Status

Working: `rigbat list` / `--json` / `tray`. Sources: sysfs, bluez, steelseries (via
`discovery::discover_all`). Tray: left-click menu, light/dark theme, display modes, config
persistence.

## Launch modes

One binary, two output adapters over a common headless core (`domain` + `sources`):

- `rigbat` / `rigbat list` — one-shot table of charge levels (`app::poll_once`).
- `rigbat --json` — machine-readable output.
- `rigbat tray` — SNI daemon (`Supervisor` + `ksni`).

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

## Module structure (target)

```text
src/
├── domain/      # types, classify, guess_kind, freedesktop_icon_name  [exists]
├── sources/     # BatterySource + BatteryBackend; sysfs/bluez/steelseries [exists]
├── discovery/   # discover_all + registry::backends()                 [exists]
├── cli/         # output adapter: table / --json                      [exists]
├── tray/        # ksni + IconRenderer (tiny-skia)                     [exists]
├── appearance/  # theme from xdg-portal (light/dark)                  [exists]
├── session/     # logind PrepareForSleep → resume re-poll             [exists]
├── app/         # poll_once + Supervisor + wiring                     [exists]
└── config/      # XDG ~/.config/rigbat/config.json                    [exists]
```

Dependencies point inward: `domain` does not import `zbus`/`tiny-skia`/`nix`.

## Conventions

- English-only repo content (comments, docs). Domain types, not stringly-typed.
- New device support: see `CONTRIBUTING.md` — add a row to a backend's device table, or a
  new `sources/<vendor>.rs` + one line in `discovery::registry::backends()`.
- HID via `/dev/hidraw` directly (no C `libhidapi`); BlueZ via `zbus` (no `bluer`/libdbus).
- No `unwrap`/`expect` in non-test code. Gates: fmt/build/test/`clippy --all-targets -D warnings`.

## Platform gotchas

- SNI hosts (COSMIC) fit the icon into a square slot — keep pixmaps square, fill them.
- tiny-skia can panic on thin anti-aliased rects — all paints use `anti_alias = false`.
- ksni `RadioGroup`/nested submenus drop clicks on COSMIC — use `StandardItem` for actions;
  left-click opens the menu via `const MENU_ON_ACTIVATE: bool = true`.
- Menu actions changing the icon must route config through the `watch` channel so the main
  loop calls `handle.update` (ksni does not re-publish the icon after a menu event).
