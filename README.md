# rigbat

System tray battery monitor for gaming peripherals.

## What it does

- Shows battery level of connected peripherals (mice, keyboards, headsets, controllers) in the system tray
- Displays one tray icon for every device, or a single aggregate icon (switch in the settings
  window); the aggregate icon follows `primary_device` from the config if set, otherwise the
  first connected device among the shown ones
- Automatically discovers connected devices on startup — no manual configuration required
- Supports multiple devices simultaneously; choose which ones appear in the settings window
- Sends desktop notifications when battery is low
- Works with wired, wireless, and Bluetooth devices

## Supported connection types

- USB HID (wired and wireless dongles)
- Bluetooth (BLE and BR/EDR)
- Linux kernel power supply (sysfs)

## Requirements

- Linux
- System tray support (StatusNotifierItem / XEmbed)
- Rust toolchain (stable, edition 2024, MSRV 1.96) to build from source

## Build and install

The `Makefile` standardizes the local flow. Run `make` for the full target list.

```sh
make install       # cargo install --path . --force --locked → ~/.cargo/bin/rigbat
make run           # run the tray in the foreground (quick look, Ctrl-C to stop)
sudo make udev-install   # USB HID permissions — see Permissions below
```

`make install` puts `rigbat` on your `PATH` (assuming `~/.cargo/bin` is on it), and installs
a desktop entry and icon (`packaging/rigbat.desktop`, `packaging/icons/`) so `rigbat settings`
gets an application menu entry and a taskbar icon instead of a generic placeholder. `--locked`
builds against the committed `Cargo.lock` — a dependency fix released after the lockfile was
written is not picked up until the lockfile is updated. Run modes:

```sh
rigbat            # one-shot battery table
rigbat --wide     # one-shot table with transport and locator columns
rigbat --json     # machine-readable
rigbat --waybar   # long-lived waybar custom module (see Status bars below)
rigbat tray       # tray daemon
rigbat settings   # settings window
rigbat --help     # show usage (-h)
rigbat --version  # show the version (-V)
```

`--help` and `--version` exit 0. An unknown argument, or `--json` and `--waybar` passed together,
prints usage to stderr and exits 2.

## Run as a systemd user service

A systemd _user_ service runs the tray automatically with your graphical session
and exposes it to `systemctl` / `journalctl`.

```sh
make service   # install binary + ~/.config/systemd/user/rigbat.service
make enable    # systemctl --user enable --now rigbat.service
make logs      # journalctl --user -u rigbat -f
make status    # service status
make restart   # after reinstalling the binary
make disable   # systemctl --user disable --now rigbat.service
make uninstall # stop and remove everything installed (see below)
```

`make uninstall` removes the unit, desktop entry, icon, autostart entry and the binary.
It asks for `sudo` only if the udev rule is actually present, and every step is
best-effort, so it finishes even on a session with no user D-Bus (a plain SSH login).

The service needs the session environment (Wayland/X display, session D-Bus),
which modern desktops (GNOME, KDE, COSMIC) import into the systemd user manager
at login automatically.

Use the service **or** the in-app "Startup" toggle (which writes an XDG autostart
entry), not both — each launches `rigbat tray`. A second instance detects the
first through a session-bus name and exits immediately instead of doubling
every device in the tray; the settings window also disables the Startup
checkbox and explains why when it sees the service enabled.

## Permissions

`/dev/hidraw*` nodes are root-only by default on most distros, so a USB HID device
(e.g. a SteelSeries mouse) shows as `offline` until you install the udev rule:

```sh
sudo make udev-install
```

This installs `packaging/70-rigbat.rules` to `/etc/udev/rules.d/` and reloads udev. The
rule grants the logged-in user access, scoped to the specific vendor/product ids rigbat
supports (`TAG+="uaccess"` via logind) — not a blanket grant to every HID device. `make
install`/`make service` never need root; only this step does, since it writes to `/etc`.

Bluetooth and sysfs (kernel power_supply) devices need no rule — only USB HID access is
gated by permissions. A device already plugged in when you run `udev-install` is
re-triggered automatically; if it still shows `offline`, replug it.

## Settings

`rigbat settings` opens the settings window. It edits the poll interval (10–3600 s, default
60), the low-battery threshold (5–50%, default 20), the display mode, which devices are shown,
whether one icon per device is used, whether notifications fire, whether rigbat starts with the
session, and per-device overrides of the interval and threshold. Changes are written to
`config.json` and the tray picks them up through a file watch — no restart.

Config lives at `~/.config/rigbat/config.json`. `make uninstall` leaves it in place —
remove it yourself if you want a clean slate.

## Status bars

rigbat's tray is a StatusNotifierItem, which does not cover wlroots compositors running
Waybar/Polybar instead of an SNI host.

### Waybar

`rigbat --waybar` is a long-lived process: it prints one `custom` module JSON line on startup
and again on every state change, describing the featured device — the same device the aggregate
tray icon shows. Add to `~/.config/waybar/config`, **omitting** `interval` — a script with no
`interval` and no `signal` is expected to loop and push updates itself:

```jsonc
"custom/rigbat": {
  "exec": "rigbat --waybar",
  "return-type": "json"
}
```

Setting `interval: 0` does **not** mean continuous — since Waybar issue #4522 (closed October
2025), `interval: 0` disables polling entirely instead. If the process ever exits (crash, restart),
add `restart-interval` so Waybar respawns it:

```jsonc
"custom/rigbat": {
  "exec": "rigbat --waybar",
  "return-type": "json",
  "restart-interval": 30
}
```

`class` is one of `charging`, `low`, `ok`, `offline` — style it in `~/.config/waybar/style.css`:

```css
#custom-rigbat.low {
  color: #e06c75;
}
```

`--waybar` retains the last reading of a device that goes unreachable (asleep, switched off, out
of range), the same as the tray: the tooltip shows it with its age (e.g. "mouse: 88% offline
(5m ago)") instead of losing the value. The featured device's own `text`/`class`/`percentage`
still report a plain `offline` with no `percentage` key while it is unreachable — only the
tooltip carries the retained value.

### Polybar

Polybar's `custom/script` consumes plain text, not JSON, so `--waybar` does not serve it. Pipe
`rigbat --json` through `jq` instead:

```sh
rigbat --json | jq -r '.[0] | if .online then "\(.percent)% \(.name)" else "\(.name) offline" end'
```

## Troubleshooting / logs

Diagnostics go to stderr via `tracing`; `journalctl` captures that stream for the
systemd service. No journald transport is linked in, so levels show up in the
message text rather than as journald priorities:

```sh
journalctl --user -u rigbat -f
```

Raise verbosity with `RIGBAT_LOG` (falls back to `RUST_LOG`), default `info` for the daemons
(`tray`, `settings`, `--waybar`) and `warn` for the one-shot CLI modes (`list`, `--json`):

```sh
RIGBAT_LOG=debug rigbat tray
RIGBAT_LOG=rigbat::sources=trace rigbat tray   # one module only
```
