# Contributing to rigbat

rigbat reads battery levels from peripherals through pluggable **backends**. Adding
support for a new device is the most common contribution. This guide shows where the
code goes and how to wire it in.

## How sources work

Two traits in `src/sources/mod.rs`:

- `BatterySource` — reads one device: `async fn poll(&mut self) -> Result<BatteryReading>`.
- `BatteryBackend` — finds devices and creates sources: `async fn discover(&self) -> Vec<Box<dyn BatterySource>>`.

Backends are listed in one place — `src/discovery/registry.rs`:

```rust
pub fn backends() -> Vec<Box<dyn BatteryBackend>> {
    vec![
        Box::new(SysfsBackend),
        Box::new(BluezBackend),
        Box::new(SteelSeriesBackend),
        Box::new(EightBitDoBackend),
    ]
}
```

`discovery::discover_all()` runs every backend and merges the results.

## Two ways to contribute

### 1. New device, vendor already supported

If the vendor backend uses a device table (e.g. `src/sources/steelseries.rs`), add one
row to its `DEVICES` array with the USB product id and metadata.

```rust
const DEVICES: &[SteelSeriesDevice] = &[
    SteelSeriesDevice { product_id: 0x1852, name: "SteelSeries Aerox 5 Wireless", kind: DeviceKind::Mouse },
    SteelSeriesDevice { product_id: 0x____, name: "Your device",                 kind: DeviceKind::Mouse }, // ← add
];
```

If the device talks over USB HID (not Bluetooth or sysfs), also add a matching line to
`packaging/70-rigbat.rules` — see [docs/adding-a-device.md](docs/adding-a-device.md#permissions).
Without it the device reads as `offline` because `/dev/hidraw*` is root-only by default.

### 2. New vendor or transport

Add a file `src/sources/<vendor>.rs`, implement `BatteryBackend` + `BatterySource`,
then register one line in `src/discovery/registry.rs`.

Pick the granularity that fits the protocol:

- **Self-describing** (the protocol reports its own capabilities) — one backend covers
  many devices with no per-device code. Reference: `src/sources/bluez.rs` (BlueZ Battery1).
- **Device table** (devices differ, matched by USB vendor/product id) — keep a `DEVICES`
  table and per-device quirks. Reference: `src/sources/steelseries.rs`.

## Requirements for a vendor file

- Module doc comment describing the protocol: USB ids, how the battery value is obtained
  (HID report / D-Bus / sysfs), and a link to your reverse-engineering source.
- No `unwrap`/`expect` in non-test code — return `anyhow::Result` with context.
- Pure parsing logic split into functions with unit tests (see the `parse_*` functions in
  `steelseries.rs`).
- Talk to devices through kernel interfaces directly (`/sys`, `/dev/hidraw`) or pure-Rust
  D-Bus (`zbus`) — no `libhidapi`, no `libdbus`. See
  [docs/adding-a-device.md](docs/adding-a-device.md). This is about device access, not a
  blanket ban on C dependencies: the state store is SQLite via `rusqlite`, chosen because no
  pure-Rust engine offers a comparable multi-process story (redb's is an experimental flag in
  an unreleased version; the Turso rewrite is pre-1.0 by its own maintainers).
- No `unsafe` in this crate — enforced by `unsafe_code = "forbid"` in `Cargo.toml`, which the
  compiler checks. Dependencies may use it internally; rigbat's own code may not.

## Build requirements

A C compiler (`cc`) must be on `PATH`: `rusqlite`'s `bundled` feature compiles SQLite from
source. Everything else in the dependency tree is pure Rust.

## Checks before opening a PR

```sh
cargo fmt
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -- list      # your device should appear
```

A few tests are marked `#[ignore]` because `cargo test` is the wrong place for them: two need a
live system D-Bus, and `tray::icon::tests::dump_icons` is a debug helper that writes every icon
variant to `/tmp` as PNG for visual inspection rather than asserting anything. `cargo test` skips
all three; run them yourself on a desktop machine with:

```sh
make test-live         # cargo test -- --ignored
```

## Adding a translation

The tray menu, the settings window and the low-battery notification read their text from
Fluent catalogues in `i18n/<lang>/rigbat.ftl`; the CLI stays English.

1. Copy `i18n/en/rigbat.ftl` to `i18n/<lang>/rigbat.ftl` and translate the values. Keep message
   ids and `{ $placeholders }` unchanged.
2. Add the language to `Lang` in `src/i18n/mod.rs` (`ALL`, `tag`, `native_name`, `from_tag`,
   `id`, `loader`).
3. Run `cargo test`. `i18n::tests` fails on a missing or extra message, and
   `settings::tests::device_table_text_is_not_clipped_in_any_language` fails on a column header
   too wide for its column — shorten the text rather than widening the column.

## Figuring out the protocol

See [docs/adding-a-device.md](docs/adding-a-device.md) for how to discover where a
device's battery data lives and how to reverse-engineer HID protocols.

## License of contributions

rigbat is dual-licensed under [Apache-2.0](LICENSE-APACHE) and [MIT](LICENSE-MIT), and a
contribution arrives under both. In the words the Rust ecosystem uses:

> Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
> the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
> any additional terms or conditions.

Two licences rather than one because they cover different things: MIT is short and permissive,
Apache-2.0 adds an explicit patent grant from each contributor for their own contribution. Nothing
here asks you to sign anything, and you keep the copyright on what you write.

Only submit code you have the right to submit. Code copied from another project carries that
project's licence with it, and code written for an employer usually belongs to the employer — in
either case it cannot simply be pasted in here.
