# rigbat

System tray battery monitor for gaming peripherals.

## What it does

- Shows battery level of connected peripherals (mice, keyboards, headsets, controllers) in the system tray
- Displays a single tray icon that reflects the charge state of the primary device
- Automatically discovers connected devices on startup — no manual configuration required
- Supports multiple devices simultaneously; switch the primary device from the tray menu
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
make install   # cargo install --path . --force → ~/.cargo/bin/rigbat
make run       # run the tray in the foreground (quick look, Ctrl-C to stop)
```

`make install` puts `rigbat` on your `PATH` (assuming `~/.cargo/bin` is on it).
Run modes:

```sh
rigbat            # one-shot battery table
rigbat --json     # machine-readable
rigbat tray       # tray daemon
rigbat settings   # settings window
```

## Run as a systemd user service

A systemd _user_ service runs the tray automatically with your graphical session
and exposes it to `systemctl` / `journalctl`.

```sh
make service   # install binary + ~/.config/systemd/user/rigbat.service
make enable    # systemctl --user enable --now rigbat.service
make logs      # journalctl --user -u rigbat -f
make status    # service status
make restart   # after reinstalling the binary
make uninstall # stop, remove the unit and the binary
```

The service needs the session environment (Wayland/X display, session D-Bus),
which modern desktops (GNOME, KDE, COSMIC) import into the systemd user manager
at login automatically.

Use the service **or** the in-app "Startup" toggle (which writes an XDG autostart
entry), not both — each launches `rigbat tray`, so enabling both starts two
instances.
