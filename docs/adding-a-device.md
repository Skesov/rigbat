# Adding a device: finding the battery data

Before writing a driver, find out where your device exposes its battery. Work down this
list — the earlier options need little or no code.

## 1. Already in the kernel? (`sysfs`)

Many devices appear under `/sys/class/power_supply`:

```sh
for d in /sys/class/power_supply/*; do
  echo "$(basename "$d"): $(cat "$d/type") $(cat "$d/capacity" 2>/dev/null)%"
done
```

If yours is listed with a `capacity`, the existing `sysfs` backend already handles it —
no new code needed.

## 2. Bluetooth? (BlueZ Battery1)

Connected Bluetooth devices that report battery expose `org.bluez.Battery1`:

```sh
busctl get-property org.bluez /org/bluez/hci0/dev_XX_XX_XX_XX_XX_XX \
  org.bluez.Battery1 Percentage
```

If this returns a value, the existing `bluez` backend already handles it — no new code
needed. (Some devices need the "Battery Service" enabled or a quirk to expose it.)

## 3. USB HID with a vendor protocol

If the value lives behind a vendor-specific HID report, you need a driver. rigbat talks to
`/dev/hidraw*` directly (no `libhidapi`).

### Locate the device and interface

```sh
# hidraw nodes and their USB ids
for h in /sys/class/hidraw/hidraw*; do
  echo "$(basename "$h"): $(grep HID_ID= "$h/device/uevent")"
done

# USB interface number of a node (the ":1.N" segment)
readlink -f /sys/class/hidraw/hidraw6/device | grep -oE ':[0-9]+\.[0-9]+'
```

Vendor protocols usually answer on a dedicated interface (SteelSeries uses interface 3).

### Reverse-engineer the report

Options, easiest first:

- **Reference existing open-source drivers.** Most protocols are already documented:
  - Logitech: [Solaar](https://github.com/pwr-Solaar/Solaar) (HID++)
  - Razer: [OpenRazer](https://github.com/openrazer/openrazer)
  - Headsets: [HeadsetControl](https://github.com/Sapd/HeadsetControl)
  - Mice: [libratbag](https://github.com/libratbag/libratbag)
- **Capture USB traffic** from the vendor's official software (usually Windows):
  `usbmon` + Wireshark, or a VM with USB passthrough. Find the request/response that
  changes with battery level.
- **Probe** `/dev/hidraw` feature reports and watch which byte tracks the charge.

### Implement

Copy `src/sources/steelseries.rs` as a template. It shows the full shape:

- a `DEVICES` table keyed by USB product id,
- `discover()` that walks `/sys/class/hidraw` and matches vendor/product/interface,
- `poll()` that opens the node `O_NONBLOCK`, writes the query, waits with `nix::poll`
  (so it never hangs), and parses the response,
- pure `parse_*` functions with unit tests.

Then add a row to `backends()` in `src/discovery/registry.rs`.

## Permissions

Reading `/dev/hidraw*` requires a udev rule granting your user access — `/dev/hidraw*`
nodes are root-only by default on most distros. `packaging/70-rigbat.rules` ships one
line per supported USB HID device, scoped by vendor/product id; `sudo make
udev-install` installs it (see the README's Permissions section). When you add a device
to a vendor's `DEVICES` table, add a matching `ATTRS{idVendor}`/`ATTRS{idProduct}` line
to `packaging/70-rigbat.rules` too.

For a quick local check while reverse-engineering, without installing the rule, a
development-only shortcut works:

```
KERNEL=="hidraw*", MODE="0660", TAG+="uaccess"
```

**Do not install this as a rule.** It matches every `hidraw` node — every HID device on
the system, keyboards included — which is a keylogging surface. Use it only as a
temporary local snippet while probing, then remove it.

## Multiple batteries

Some devices report several batteries (earbuds: left / right / case). rigbat does not
model this yet. If you need it, open an issue first so we can design the source-to-device
mapping before you implement.
