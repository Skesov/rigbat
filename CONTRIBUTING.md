# Contributing to rigbat

rigbat reads battery levels from peripherals through pluggable **backends**. Adding
support for a new device is the most common contribution. This guide shows where the
code goes and how to wire it in.

## How sources work

Two traits in `src/sources/mod.rs`:

- `BatterySource` — reads one device: `async fn poll(&mut self) -> anyhow::Result<BatteryReading>`.
- `BatteryBackend` — finds devices and creates sources:
  `async fn discover(&self, ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>>`.
  A backend that opens `/dev/hidraw*` also returns its device table from
  `fn hidraw_family(&self) -> Option<&'static HidrawFamily>` (default `None`).

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

`discovery::discover_all()` runs every backend and merges the results. `rigbat doctor` and the
udev rule test read the hidraw device tables through `registry::hidraw_families()`, derived from
`backends()`, so a registered backend is checked without a second list.

## Two ways to contribute

### 1. New device, vendor already supported

A hidraw backend declares its devices as a `HidrawFamily` (`src/sources/hidraw.rs`): vendor id,
battery interface, and a `models` table. A new model of the same protocol is one row:

```rust
pub static FAMILY: hidraw::HidrawFamily = hidraw::HidrawFamily {
    vendor: 0x1038,
    models: &[
        hidraw::HidrawModel { product: 0x1852, name: "SteelSeries Aerox 5 Wireless", kind: DeviceKind::Mouse },
        hidraw::HidrawModel { product: 0x____, name: "Your device",                 kind: DeviceKind::Mouse }, // ← add
    ],
    interface: Some(3),
};
```

Also add a matching line to `packaging/70-rigbat.rules` — see
[docs/adding-a-device.md](docs/adding-a-device.md#permissions). `cargo test` fails until the rule
and the tables agree. Without the rule the device reads `no access` (`Presence::NoAccess`) because
`/dev/hidraw*` is root-only by default. Full steps: [checklist](#checklists).

### 2. New vendor or transport

Add a file `src/sources/<vendor>.rs`, implement `BatteryBackend` + `BatterySource` under the
[backend contract](#backend-contract), and follow the matching [checklist](#checklists).

Pick the granularity that fits the protocol:

- **Self-describing** (the protocol reports its own capabilities) — one backend covers
  many devices with no per-device code. Reference: `src/sources/bluez.rs` (BlueZ Battery1).
- **Device table** (devices differ, matched by USB vendor/product id) — declare a
  `HidrawFamily` and keep per-device quirks in the backend. Reference: `src/sources/steelseries.rs`.

## Backend contract

The supervisor and the stores rely on these; a backend that breaks one misbehaves far from its
own code.

1. **Unique, stable `name()`.** The supervisor records which backend discovered each device and
   retires a vanished device only on its own backend's successful sweep. Two backends sharing a
   name, or a renamed backend, retire each other's devices.
2. **`Err` is not `Ok(vec![])`.** Return `Err` when the sweep itself failed (a bus call errored, a
   directory could not be read); `Ok(vec![])` means you looked and found nothing. A failed sweep
   leaves the backend's devices as they were; a successful one that omits a device marks it
   disconnected.
3. **Keep typed errors typed.** Open a hidraw node through `hidraw::open_verified`, which returns
   `AccessDenied` (the device reads `no access`) and `NodeReassigned` (the source is retired and
   rediscovered). The supervisor downcasts both, so propagate them with `?` or `.context(…)`,
   never by formatting them into a new error.
4. **Stable locator.** `DeviceInfo::locator` is part of the device's identity: a serial, a
   Bluetooth address or a USB topology path, never a `hidrawN` node that is renumbered on
   replug. `hidraw::stable_locator` picks one from the `uevent`; the shared discovery sets it.
5. **`name` is identity.** The device name is part of `DeviceId` and keys the config
   (`hidden_devices`, per-device overrides). Only the state store's inventory carries settings
   across a rename, and the store is optional — keep a shipped model's `name` unchanged.
6. **Ranged levels are coarse.** A protocol that reports a band (e.g. "low / medium / high")
   builds its reading with `BatteryReading::new_coarse`, so no time-remaining estimate is drawn
   from it.
7. **Blocking I/O runs in `spawn_blocking`, bounded.** `discover` and `poll` share the runtime with
   every other source. Every blocking call has a timeout (a `nix::poll` deadline, not a blocking
   `read`): dropping a runtime — the settings window closing mid-scan — waits for in-flight
   blocking calls.
8. **Pick the handle policy from the protocol.** Request/response devices keep their handle open
   for the source's lifetime; stream-only devices open per poll. See `CLAUDE.md` ("Source port").

## Checklists

### New model of a supported HID vendor

1. Read the model's report descriptor: same query, same report length, same interface as the
   family? If not, it is not one row — extend the backend.
2. Add a `HidrawModel` row to the backend's `FAMILY.models`.
3. Add its `ATTRS{idVendor}`/`ATTRS{idProduct}` line to `packaging/70-rigbat.rules`.
4. Add the model to the device list in `README.md`.
5. Run `cargo test`, reinstall the rule (`sudo make udev-install`), and run `rigbat doctor`: the
   device's node must open read-write.

### New HID vendor

1. Establish the handle policy: read the report descriptor and watch whether the device sends
   anything unprompted.
2. Add `src/sources/<vendor>.rs` with a `pub static FAMILY: HidrawFamily`, a backend whose
   `hidraw_family()` returns it and whose `discover` calls `hidraw::discover(&FAMILY)`, and a
   source whose `poll` opens the node through `hidraw::open_verified`. Keep protocol parsing in
   pure `parse_*` functions with unit tests.
3. Add `pub mod <vendor>;` to `src/sources/mod.rs`.
4. Register the backend in `backends()` and update the expected names in
   `backends_contains_all_expected_names` and the vendors in
   `hidraw_families_come_from_the_hidraw_backends` (`src/discovery/registry.rs`).
5. Add the udev lines to `packaging/70-rigbat.rules`.
6. Add the vendor to `README.md`'s device list and to the vendor names in
   `packaging/aur/rigbat-git/rigbat.install`.
7. A new device type: see "New `DeviceKind`" below.
8. Verify with `cargo test`, `rigbat list --wide`, and `rigbat doctor`.

### New non-HID transport

1. Add `src/sources/<transport>.rs` and `pub mod <transport>;` in `src/sources/mod.rs`.
2. Add a `Transport` variant: `domain/types.rs` (`as_str`) and `transport_from_str` in
   `src/state/store.rs` — the string match has a catch-all, so the compiler does not flag it.
3. Register the backend in `backends()` and update `backends_contains_all_expected_names`.
4. If devices appear and vanish without a poll noticing, add a hotplug watcher that calls
   `RefreshSignal::rediscover`, and start it in `spawn_bus_dependent_tasks` (`src/main.rs`), as
   `bluez::watch_events` does. If a device announces its level, implement
   `BatterySource::pushed` instead of re-polling on the announcement.
5. If access needs a udev rule or a group, add a `rigbat doctor` check for it.
6. Add the transport to `README.md` ("Supported connection types").
7. Verify with `cargo test`, `rigbat list --wide`, and `rigbat doctor`.

### New `DeviceKind`

1. `src/domain/types.rs`: the variant, `as_str`, `label`, the freedesktop icon name, and any
   `guess_kind` keywords.
2. A `kind-<name>` message in every `i18n/<lang>/rigbat.ftl`.
3. A silhouette in `maybe_draw_kind_glyph` (`src/icon/mod.rs`) and a glyph in `kind_glyph`
   (`src/dashboard/mod.rs`).
4. `kind_from_str` in `src/state/store.rs` — a catch-all string match the compiler does not flag.

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

A few tests are marked `#[ignore]` because `cargo test` is the wrong place for them: four need the
live session or system D-Bus (and `rigbat doctor`'s probe also reads `/dev/hidraw*`), and
`icon::tests::dump_icons` is a debug helper that writes every icon variant to `/tmp` as PNG for
visual inspection rather than asserting anything. `cargo test` skips all five; run them yourself on a desktop machine with:

```sh
make test-live         # cargo test -- --ignored
```

The D-Bus integration tests start a private `dbus-daemon` (package `dbus`) and skip themselves,
with a message, when it is not installed.

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
