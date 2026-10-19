# Corsair wireless mice and keyboards (bragi): protocol notes

Facts behind `src/sources/corsair.rs`: USB ids, the battery interface, the request, the reply
layout, how a Slipstream receiver addresses the device paired to it, and how it says "device off".
Each fact names its source. The code is written from this note, not from the sources.

Corsair's current mice and keyboards speak one protocol over USB, both on the cable and through a
Slipstream receiver; ckb-next calls it "bragi". Corsair Void headsets are a different protocol,
covered in [headsets.md](headsets.md).

The sources are GPL-2.0 (ckb-next) and GPL-3.0 (OpenLinkHub). rigbat is MIT OR Apache-2.0, so only
facts come from them — ids, byte positions, byte values — never code, comments or table text.

None of this is verified on real hardware: the maintainer owns none of these devices.

## Sources

ckb-next at commit `833ab509`, OpenLinkHub at commit `c8c1d2a9`.

| Key         | Source                                                                                                                                                                                                                                         |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ckb-proto   | ckb-next [`src/daemon/bragi_proto.h`](https://github.com/ckb-next/ckb-next/blob/833ab50951e230674bda02e8448ef6ef365dfd81/src/daemon/bragi_proto.h)                                                                                             |
| ckb-ids     | ckb-next [`src/daemon/usb.h`](https://github.com/ckb-next/ckb-next/blob/833ab50951e230674bda02e8448ef6ef365dfd81/src/daemon/usb.h) (`IS_DONGLE`, `USES_BRAGI`)                                                                                 |
| ckb-common  | ckb-next [`src/daemon/bragi_common.c`](https://github.com/ckb-next/ckb-next/blob/833ab50951e230674bda02e8448ef6ef365dfd81/src/daemon/bragi_common.c)                                                                                           |
| ckb-usb     | ckb-next [`src/daemon/usb_bragi.c`](https://github.com/ckb-next/ckb-next/blob/833ab50951e230674bda02e8448ef6ef365dfd81/src/daemon/usb_bragi.c)                                                                                                 |
| ckb-keymap  | ckb-next [`src/daemon/keymap.c`](https://github.com/ckb-next/ckb-next/blob/833ab50951e230674bda02e8448ef6ef365dfd81/src/daemon/keymap.c)                                                                                                       |
| ckb-device  | ckb-next [`src/daemon/device_bragi.c`](https://github.com/ckb-next/ckb-next/blob/833ab50951e230674bda02e8448ef6ef365dfd81/src/daemon/device_bragi.c)                                                                                           |
| ckb-notif   | ckb-next [`src/daemon/bragi_notification.c`](https://github.com/ckb-next/ckb-next/blob/833ab50951e230674bda02e8448ef6ef365dfd81/src/daemon/bragi_notification.c)                                                                               |
| ckb-gui     | ckb-next [`src/gui/batterysystemtrayicon.h`](https://github.com/ckb-next/ckb-next/blob/833ab50951e230674bda02e8448ef6ef365dfd81/src/gui/batterysystemtrayicon.h)                                                                               |
| olh-devices | OpenLinkHub [`src/devices/devices.go`](https://github.com/jurkovic-nikola/OpenLinkHub/blob/c8c1d2a99a56aa1d907e43137d127c2048f41ba5/src/devices/devices.go)                                                                                    |
| olh-slip    | OpenLinkHub [`src/devices/slipstream/slipstream.go`](https://github.com/jurkovic-nikola/OpenLinkHub/blob/c8c1d2a99a56aa1d907e43137d127c2048f41ba5/src/devices/slipstream/slipstream.go)                                                        |
| olh-k65d    | OpenLinkHub [`src/devices/k65plusWdongle/k65plusWdongle.go`](https://github.com/jurkovic-nikola/OpenLinkHub/blob/c8c1d2a99a56aa1d907e43137d127c2048f41ba5/src/devices/k65plusWdongle/k65plusWdongle.go)                                        |
| olh-cable   | OpenLinkHub `src/devices/<model>WU/<model>WU.go`, one per cable model below (for example [`ironclawWU.go`](https://github.com/jurkovic-nikola/OpenLinkHub/blob/c8c1d2a99a56aa1d907e43137d127c2048f41ba5/src/devices/ironclawWU/ironclawWU.go)) |

## Ids

Vendor `0x1B1C`. Every id below is on USB interface 1 (olh-devices: the interface column of each
row). olh-devices lists them in decimal; the table gives hex.

Receivers:

| Product  | rigbat name                  | Source                | Kind     |
| -------- | ---------------------------- | --------------------- | -------- |
| `0x1BDC` | Corsair Slipstream Receiver  | olh-devices, olh-slip | mouse    |
| `0x1BA6` | Corsair Slipstream Receiver  | olh-devices, ckb-ids  | mouse    |
| `0x2B00` | Corsair Slipstream Receiver  | olh-devices, olh-slip | mouse    |
| `0x1B7F` | Corsair Dark Core RGB Pro SE | olh-devices, ckb-ids  | mouse    |
| `0x1B81` | Corsair Dark Core RGB Pro    | olh-devices, ckb-ids  | mouse    |
| `0x1B65` | Corsair Harpoon RGB Wireless | olh-devices, ckb-ids  | mouse    |
| `0x2B07` | Corsair K65 Plus Wireless    | olh-devices, olh-k65d | keyboard |

ckb-next lists `0x1BA6`, `0x1B7F`, `0x1B81` and `0x1B65` as bragi receivers (ckb-ids). olh-devices
hands the first six to its Slipstream driver (olh-slip) and `0x2B07` to its K65 Plus receiver
driver (olh-k65d), which uses the same framing.

Wireless devices on the cable, each handled by an olh-cable driver that reads the battery with the
request below:

| Product  | rigbat name                         | Kind     |
| -------- | ----------------------------------- | -------- |
| `0x1B4C` | Corsair Ironclaw RGB Wireless       | mouse    |
| `0x2B32` | Corsair Ironclaw Wireless SE        | mouse    |
| `0x1B5E` | Corsair Harpoon RGB Wireless        | mouse    |
| `0x2B2E` | Corsair Harpoon V2 Wireless         | mouse    |
| `0x1B7E` | Corsair Dark Core RGB Pro SE        | mouse    |
| `0x1B80` | Corsair Dark Core RGB Pro           | mouse    |
| `0x1BB2` | Corsair Darkstar RGB Wireless       | mouse    |
| `0x1BB5` | Corsair M65 RGB Ultra Wireless      | mouse    |
| `0x1BD6` | Corsair M65 RGB Ultra Wireless      | mouse    |
| `0x1BF1` | Corsair M75 Wireless                | mouse    |
| `0x2B08` | Corsair M75 Wireless                | mouse    |
| `0x1BF2` | Corsair M75 Air Wireless            | mouse    |
| `0x1BB8` | Corsair Nightsabre Wireless         | mouse    |
| `0x1B98` | Corsair Sabre RGB Pro Wireless      | mouse    |
| `0x1BDB` | Corsair Scimitar RGB Elite Wireless | mouse    |
| `0x2B22` | Corsair Scimitar Elite Wireless SE  | mouse    |
| `0x1B6E` | Corsair K57 RGB Wireless            | keyboard |
| `0x2B10` | Corsair K65 Plus Wireless           | keyboard |
| `0x2B11` | Corsair K65 Plus Wireless           | keyboard |
| `0x2B23` | Corsair K65 Plus Wireless           | keyboard |
| `0x2B02` | Corsair K70 Core TKL Wireless       | keyboard |

A receiver and the cable of the same model share a name; the locator tells them apart.

### Left out

- `0x2B1B` (Slipstream V2 receiver), `0x2B1C` (Nightsword V2), Vanguard 96 / 99 Air: their
  olh-devices drivers use a four-byte header, not the framing below.
- `0x2B2A`, `0x2B34` (Sabre V2 Pro receivers), `0x2B28`, `0x2B29` (Sabre V2 Pro on the cable),
  `0x1B51` (Dark Core RGB SE receiver): their drivers send no battery request of this shape.
- `0x1BAB` (K100 Air), `0x1BB6` (K70 Pro Mini), `0x2B04` (MAKR 75) on the cable: their drivers
  use 1024- or 512-byte reports. Paired to a Slipstream receiver they go through its 64-byte
  reports and are covered.
- `0x1B66` (Ironclaw RGB Wireless receiver), `0x1B62` (K57 receiver): ckb-next lists them, but
  olh-devices does not, so their battery interface is not pinned.

## Request

- olh-slip and olh-cable write 65 bytes: byte 0 is report id `0x00` (the device uses unnumbered
  reports), byte 1 the address, byte 2 the command, byte 3 the property, the rest zero. They read
  a 64-byte reply.
- ckb-common sends the same packet without the report id: `0x08` (`BRAGI_MAGIC`, ckb-proto), GET
  `0x02`, the property, zeros.
- The two agree: ckb-next's magic is OpenLinkHub's address byte. For a device behind a receiver,
  ckb-usb ORs the paired device's id (1–7) into that byte and sends the packet to the receiver;
  olh-slip writes `0x08 + n` for paired device `n`. For n < 8 the two are the same byte. The
  receiver itself and a device on the cable are `0x08` (olh-cable).
- Properties (ckb-proto): `0x0F` battery level, `0x10` battery status, `0x36` subdevice bitfield.

## Reply

- Byte 1 echoes the command (ckb-common compares it with the request), byte 2 is an error code,
  `0` for success (ckb-common). `0x05` means "not supported" (ckb-proto).
- The value is little-endian from byte 3: ckb-common reads three bytes, olh-slip two. rigbat reads
  three.
- On a receiver, the low three bits of byte 0 name the paired device the reply comes from, `0`
  for the receiver itself (ckb-keymap routes a reply by them). rigbat ignores a reply whose bits
  do not name the device it asked. On the cable it does not check byte 0: no source pins it there.
- Battery level is in tenths of a percent: ckb-device and olh-slip divide it by 10. olh-slip
  discards a level of `0`; rigbat treats it, and anything above 1000, as no reading.
- Battery status: ckb-device stores the value as read, and ckb-gui reads it as `1` charging,
  `2` discharging, `3` fully charged, `0` unknown. rigbat shows `1` as charging, `3` as full and
  anything else, or an error, as discharging.

## Paired devices

- Property `0x36` on the receiver (address `0x08`) is a bitfield: bit `n` (1–7) set means paired
  device `n` is connected (ckb-device asks for it when the receiver starts; ckb-common creates one
  device per set bit and drops one whose bit is clear).
- The receiver announces changes with a notification carrying the same bitfield in byte 4
  (ckb-notif); olh-slip reads it as `0x02` for a single paired device, `0x04` mouse, `0x08`
  keyboard, `0x0C` both. With one paired device olh-slip addresses `0x09`, matching bit 1; olh-k65d
  always addresses `0x09`. rigbat does not listen for the notification and asks for the bitfield
  on every poll instead.
- ckb-common reads the paired device's vendor and product id (properties `0x11`, `0x12`) to name
  it. rigbat names the device after the receiver row instead (see below) and does not ask.

## Offline signalling

A receiver poll asks for the bitfield first. With no bit set, or when the paired device answers
the battery request with a non-zero error code, the poll fails with "device is asleep or off
(receiver lists no connected device)" or "(receiver reported an error for it)". The supervisor then
shows the device as unreachable, keeping its last reading.

## One source per receiver

A receiver is one device in rigbat, named after its row, and reports the lowest-numbered connected
paired device. The paired device's own product id is not read at discovery: discovery would have to
talk to a receiver whose handle a running source already holds, and a device switched off during a
sweep would be retired as disconnected instead of shown as unreachable. A receiver with a mouse and
a keyboard paired shows the mouse (olh-slip: bit 2) only.

## Handle policy

Request/response: the source holds the handle for its lifetime and, before each request, reads
and discards input already queued, bounded to a fixed number of reports. rigbat never switches the
device to software mode, which both sources do first; whether a GET is answered in hardware mode
is unverified.
