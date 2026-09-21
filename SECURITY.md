# Security policy

## Reporting a vulnerability

Report privately through GitHub's
[security advisories](https://github.com/Skesov/rigbat/security/advisories/new) rather than in a
public issue. Expect an acknowledgement within a week.

## What rigbat touches

Worth knowing when judging whether something is a vulnerability:

- **`/dev/hidraw*`** — rigbat reads battery reports from HID devices, and the SteelSeries backend
  writes a query report to get an answer. Access comes from the shipped udev rule
  (`packaging/70-rigbat.rules`), which grants the logged-in user access to the specific devices in
  the backends' tables (`TAG+="uaccess"`), not to every HID device on the system.
- **D-Bus** — the session bus for the tray icon, notifications and the desktop portal; the system
  bus for BlueZ and logind. It reads device properties and subscribes to signals; it does not pair,
  connect or disconnect anything.
- **Files it writes** — `$XDG_CONFIG_HOME/rigbat/config.json`,
  `$XDG_STATE_HOME/rigbat/rigbat.db` and, when the autostart toggle is on,
  `$XDG_CONFIG_HOME/autostart/rigbat.desktop`. Nothing outside the user's own directories, and
  `make udev-install` is the only step that asks for root.
- **The network** — rigbat makes no network connections of any kind.

`unsafe` is forbidden crate-wide (`unsafe_code = "forbid"`), so a memory-safety bug would have to
come from a dependency.
