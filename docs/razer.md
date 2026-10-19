# Razer wireless mice: protocol notes

Facts behind `src/sources/razer.rs`: USB ids, the battery interface, the request, the reply
layout, the wait between request and reply, how the receiver says "mouse off", and how rigbat
shares a mouse with OpenRazer. Each fact names its source. The code is written from this note, not
from the sources.

Razer mice speak one 90-byte feature-report protocol, on the cable and through a HyperSpeed
receiver. The kernel has no battery driver for them; OpenRazer's out-of-tree `razermouse` driver
exposes the level as its own sysfs attribute, not as a `power_supply`, so the sysfs backend does
not see it.

OpenRazer and the kernel are GPL-2.0. rigbat is MIT OR Apache-2.0, so only facts come from them —
ids, byte positions, byte values, delays — never code, comments or table text.

None of this is verified on real hardware: the maintainer owns no Razer mouse.

## Sources

OpenRazer at commit `477f4d6d` (2026-10-05), Linux at commit `c03c1a14`.

| Key       | Source                                                                                                                                                                                                                                                                                                                   |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| or-common | OpenRazer [`driver/razercommon.h`](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razercommon.h) and [`razercommon.c`](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razercommon.c)                                           |
| or-chroma | OpenRazer [`driver/razerchromacommon.c`](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razerchromacommon.c#L1057-L1070)                                                                                                                                                    |
| or-mouse  | OpenRazer [`driver/razermouse_driver.c`](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razermouse_driver.c)                                                                                                                                                                |
| or-ids    | OpenRazer [`driver/razermouse_driver.h`](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razermouse_driver.h)                                                                                                                                                                |
| or-udev   | OpenRazer [`install_files/udev/99-razer.rules`](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/install_files/udev/99-razer.rules) and [`razer_mount`](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/install_files/udev/razer_mount#L57-L60) |
| usbhid    | Linux [`drivers/hid/usbhid/hid-core.c`](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/drivers/hid/usbhid/hid-core.c#L873-L940), `usbhid_get_raw_report` / `usbhid_set_raw_report`                                                                                                      |
| hidraw    | Linux [`drivers/hid/hidraw.c`](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/drivers/hid/hidraw.c#L570-L596), `hidraw_report_event`                                                                                                                                                    |
| pb-razer  | PeripheralBattery (MIT) [`collector/providers/razer.py`](https://github.com/wh23423/PeripheralBattery/blob/8bd93437ed8530c272314b3e388acb4c87522942/collector/providers/razer.py#L155-L171), a Windows battery tray read on real devices                                                                                 |

## Ids

Vendor `0x1532` (or-common). The products are the ones `razer_attr_read_charge_level` in or-mouse
handles, named after their or-ids defines. OpenRazer's `_WIRELESS` / `_RECEIVER` define is the
receiver, `_WIRED` the mouse on its cable; a model with one id shows only through its receiver.
Cable and receiver share a rigbat name; the locator tells them apart.

| Products (cable / receiver) | rigbat name                                     | Txn    | Wait         | Charging query |
| --------------------------- | ----------------------------------------------- | ------ | ------------ | -------------- |
| `0x001F`                    | Razer Naga Epic                                 | `0xFF` | 0.6 ms       | yes            |
| `0x0024` / `0x0025`         | Razer Mamba 2012                                | `0xFF` | 0.6 ms       | yes            |
| `0x0032`                    | Razer Ouroboros                                 | `0xFF` | 0.6 ms       | yes            |
| `0x003E` / `0x003F` (dock)  | Razer Naga Epic Chroma                          | `0xFF` | 0.6 ms       | yes            |
| `0x0044` / `0x0045`         | Razer Mamba                                     | `0xFF` | 0.6 ms       | yes            |
| `0x0059` / `0x005A`         | Razer Lancehead                                 | `0x3F` | 0.6 ms       | yes            |
| `0x0070` / `0x006F`         | Razer Lancehead Wireless                        | `0x1F` | 31 ms        | yes            |
| `0x0073` / `0x0072`         | Razer Mamba Wireless                            | `0x3F` | 31 ms        | yes            |
| — / `0x0062`                | Razer Atheris                                   | `0x1F` | 400 ms       | no (AA)        |
| `0x0080` / `0x0077`         | Razer Pro Click                                 | `0x1F` | 31 ms        | yes            |
| `0x007A` / `0x007B`         | Razer Viper Ultimate                            | `0xFF` | 59.9 ms      | yes            |
| `0x007C` / `0x007D`         | Razer DeathAdder V2 Pro                         | `0x3F` | 59.9 ms      | yes            |
| — / `0x0083`                | Razer Basilisk X HyperSpeed                     | `0xFF` | 31 ms        | no (AA)        |
| `0x0086` / `0x0088`         | Razer Basilisk Ultimate                         | `0x1F` | 31 ms        | yes            |
| `0x008F` / `0x0090`         | Razer Naga Pro                                  | `0x1F` | 0.6 / 31 ms  | yes            |
| — / `0x0094`                | Razer Orochi V2                                 | `0x1F` | 400 ms       | no (AA)        |
| — / `0x009A`                | Razer Pro Click Mini                            | `0x1F` | 31 ms        | yes            |
| — / `0x009C`                | Razer DeathAdder V2 X HyperSpeed                | `0x1F` | 31 ms        | no (AA)        |
| `0x009E` / `0x009F`         | Razer Viper Mini SE                             | `0x1F` | 59.9 ms      | yes            |
| `0x00A5` / `0x00A6`         | Razer Viper V2 Pro                              | `0x1F` | 31 ms        | yes            |
| `0x00A7` / `0x00A8`         | Razer Naga V2 Pro                               | `0x1F` | 31 ms        | yes            |
| `0x00AA` / `0x00AB`         | Razer Basilisk V3 Pro                           | `0x1F` | 31 ms        | yes            |
| `0x00AF` / `0x00B0`         | Razer Cobra Pro                                 | `0x1F` | 31 ms        | yes            |
| — / `0x00B3`                | Razer HyperPolling Wireless Dongle              | `0x1F` | 59.9 ms      | yes            |
| — / `0x00B4`                | Razer Naga V2 HyperSpeed                        | `0x1F` | 31 ms        | no (AA)        |
| `0x00B6` / `0x00B7`         | Razer DeathAdder V3 Pro                         | `0x1F` | 31 ms        | yes            |
| — / `0x00B8`                | Razer Viper V3 HyperSpeed                       | `0x1F` | 59.9 ms      | no (AA)        |
| — / `0x00B9`                | Razer Basilisk V3 X HyperSpeed                  | `0x1F` | 31 ms        | no (AA)        |
| `0x00BE` / `0x00BF`         | Razer DeathAdder V4 Pro                         | `0x1F` | 31 ms        | yes            |
| `0x00C0` / `0x00C1`         | Razer Viper V3 Pro                              | `0x1F` | 31 / 59.9 ms | yes            |
| `0x00C2` / `0x00C3`         | Razer DeathAdder V3 Pro                         | `0x1F` | 31 ms        | yes            |
| `0x00C4` / `0x00C5`         | Razer DeathAdder V3 HyperSpeed                  | `0x1F` | 31 ms        | yes            |
| `0x00C7` / `0x00C8`         | Razer Pro Click V2 Vertical Edition             | `0x1F` | 31 ms        | yes            |
| `0x00CC` / `0x00CD`         | Razer Basilisk V3 Pro 35K                       | `0x1F` | 31 ms        | yes            |
| `0x00D0` / `0x00D1`         | Razer Pro Click V2                              | `0x1F` | 31 ms        | yes            |
| `0x00D3` / `0x00D4`         | Razer Basilisk Mobile                           | `0x1F` | 31 ms        | no (AA)        |
| `0x00D6` / `0x00D7`         | Razer Basilisk V3 Pro 35K Phantom Green Edition | `0x1F` | 31 ms        | yes            |
| `0x00DA` / `0x00DB`         | Razer Cobra HyperSpeed                          | `0x1F` | 31 ms        | yes            |
| `0x00DE` / `0x00DF`         | Razer Viper V3 Pro SE                           | `0x1F` | 31 / 59.9 ms | yes            |

- **Txn** is the transaction id `razer_attr_read_charge_level` and `razer_attr_read_charge_status`
  put in the request (or-mouse); the two functions agree for every id. The Lancehead's first ids
  (`0x0059`/`0x005A`) take `0x3F`, its later "Wireless" ids `0x1F`.
- **Wait** is the delay between the request and the reply, from `razer_get_report` (or-mouse)
  and the `*_WAIT_US` constants (or-ids): `RAZER_MOUSE_WAIT_US` 600 µs (the default),
  `RAZER_NEW_MOUSE_RECEIVER_WAIT_US` 31 000 µs, `RAZER_VIPER_MOUSE_RECEIVER_WAIT_US` 59 900 µs,
  `RAZER_ATHERIS_RECEIVER_WAIT_US` 400 000 µs. Three models wait differently on cable and
  receiver: Naga Pro 0.6 ms / 31 ms, Viper V3 Pro and Viper V3 Pro SE 31 ms / 59.9 ms. rigbat
  uses the per-id value.
- **Charging query** "no": `razer_attr_read_charge_status` answers a constant `0` for these ids
  without asking the mouse (or-mouse); they run on AA batteries. rigbat skips the query and reports
  them as discharging.
- **Not in the table**, though `razer_attr_read_charge_level` lists them:
  - `0x0095` Orochi V2 over Bluetooth: not a USB device. Its Bluetooth battery reaches rigbat
    through BlueZ, if the mouse offers one.
  - `0x00CB` Basilisk V3 35K: a wired-only mouse with no battery, and one of the three ids that
    or-mouse addresses on interface 3 (below).
- Research that preceded this note listed the Lancehead ids as `0x3F` throughout; or-mouse gives
  `0x1F` for `0x006F`/`0x0070`. This table follows or-mouse.

## Interface

- OpenRazer sends every request to USB interface 0 (`wIndex` 0) except for Naga X (`0x0096`),
  Basilisk V3 (`0x0099`) and Basilisk V3 35K (`0x00CB`), which use interface 3 (or-mouse,
  `razer_get_report`). None of those three is in the table, so every id above uses interface 0.
- On hidraw, a feature report sent or read through a node goes to that node's own USB interface:
  usbhid fills `wIndex` with the interface number (usbhid). The family therefore matches the
  hidraw node of interface 0, which is one node per device. No source says what else interface 0
  carries on each model; OpenRazer addresses it for every id above.

## Request

90 bytes (or-common, `struct razer_report`, size checked by a `static_assert`):

| Offset | Field             | Request value                         |
| ------ | ----------------- | ------------------------------------- |
| 0      | status            | `0x00` (new command)                  |
| 1      | transaction id    | per id, table above                   |
| 2–3    | remaining packets | `0x0000`, big-endian                  |
| 4      | protocol type     | `0x00`                                |
| 5      | data size         | `0x02`                                |
| 6      | command class     | `0x07`                                |
| 7      | command id        | `0x80` battery level, `0x84` charging |
| 8–87   | arguments         | zero                                  |
| 88     | crc               | XOR of bytes 2–87                     |
| 89     | reserved          | `0x00`                                |

- Battery level is class `0x07`, id `0x80`, data size `0x02`; charging is class `0x07`, id
  `0x84`, data size `0x02` (or-chroma).
- The crc is the XOR of bytes 2 to 87 (or-common, `razer_calculate_crc`). With every other byte
  zero, the battery request's crc is `0x85` and the charging request's `0x81`.
- OpenRazer sends it as a HID SET_REPORT control transfer (request `0x09`, type `0x21`, value
  `0x0300`), waits, then reads the reply with GET_REPORT (request `0x01`, type `0xA1`, value
  `0x0300`) (or-common, `razer_send_control_msg`, `razer_get_usb_response`). Value `0x0300` is
  feature report id 0.
- usbhid builds the same transfers from `HIDIOCSFEATURE`/`HIDIOCGFEATURE` on a hidraw node: report
  type feature, value `(type + 1) << 8 | id` = `0x0300` for id 0, and with id 0 the first buffer
  byte is the report id and is not sent (usbhid). rigbat passes 91 bytes — `0x00` then the 90-byte
  report — through `sources::hid_feature`.
- usbhid bounds each transfer with `USB_CTRL_SET_TIMEOUT` (5 s) (usbhid), so both ioctls return
  within that.

## Reply

The same 90-byte layout, read back as 91 bytes with the report id first.

- Status, byte 0: `0x01` busy, `0x02` successful, `0x03` failure, `0x04` timeout, `0x05` not
  supported (or-common).
- OpenRazer accepts a reply only when remaining packets, command class and command id equal the
  request's; otherwise it retries, up to five times, 10 ms apart (or-mouse,
  `razer_send_payload`). rigbat checks the same three fields and fails the poll on a mismatch:
  another program's exchange answered, and the next poll asks again.
- OpenRazer takes busy as success (or-mouse); pb-razer reads
  again while the status is busy. rigbat reads again after the same wait, at most three more times,
  and fails the poll if the mouse stays busy; it never reads a level from a busy reply.
- Battery level: argument byte 1 (offset 9), 0–255; OpenRazer's comment says it needs scaling to
  0–100, and its sysfs attribute returns it raw (or-mouse). rigbat reports
  `round(raw × 100 / 255)`.
- Charging: argument byte 1, `0` not charging, `1` charging (or-mouse). rigbat rejects any other
  value.
- The reply's crc: neither OpenRazer nor pb-razer checks it, and no source says the mouse fills
  it. rigbat does not reject a reply on it, so a firmware that leaves it zero still reads; it logs
  a mismatch at debug level, for reports from hardware.

## Offline signalling

- pb-razer reads status `0x04` (timeout) as "receiver present, mouse not responding": switched
  off, or asleep after a few seconds without movement, which the receiver cannot tell apart.
  OpenRazer returns `-ETIMEDOUT` for it (or-mouse). rigbat fails the poll with "mouse is asleep or
  off (receiver reported a timeout)", whatever the echoed command, and the supervisor shows the
  device as unreachable with its last reading.
- Whether a receiver ever answers `0x02` with a zero level while its mouse is off is not pinned by
  any source. rigbat would show 0 %.

## Handle policy

Request/response: the mouse answers only after a request. The source holds its hidraw handle for
its lifetime, as CONTRIBUTING's contract 8 asks of such a device, and never reads input reports
from it. hidraw stops queuing reports for a handle once its queue is full (hidraw), so the held
handle costs a fixed amount however fast the mouse reports.

## Coexistence with OpenRazer

- OpenRazer's `razermouse` driver binds the HID devices of these ids and still creates their
  hidraw nodes (`hid_hw_start(hdev, HID_CONNECT_DEFAULT)` in or-mouse). It serialises its own
  exchanges with a per-device mutex (or-mouse, `razer_send_payload`); a second program on hidraw
  is outside that mutex, so the two interleave: one side's request replaces the other's before the
  reply is read. pb-razer describes the same with Windows RGB software sending frames many times a
  second.
- So rigbat does not write to a mouse that `razermouse` drives. When the HID device of the node is
  bound to `razermouse` (`DRIVER=razermouse` in its `uevent`), rigbat reads the driver's
  `charge_level` (raw 0–255, scaled as above) and `charge_status` (`0`/`1`) attributes instead,
  which go through the driver's mutex. The driver creates them on the interface whose protocol is
  mouse (or-mouse, `razer_mouse_probe`), so rigbat looks for them on every interface of the USB
  device. The check runs on every poll without an open hidraw handle, so loading or unloading
  OpenRazer switches the path without changing the device's identity. A node whose locator no
  longer matches the device is treated as reassigned, as on hidraw.
- The attributes are mode `0440` (or-mouse). OpenRazer's udev rule hands every attribute of a bound
  device to group `plugdev` (or-udev). A user outside `plugdev` reads `no access`.
- With the mouse off, `charge_level` fails with `ETIMEDOUT` (or-mouse, the timeout status above);
  rigbat reads that as asleep or off.
