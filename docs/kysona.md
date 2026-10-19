# Kysona-protocol mice: protocol notes

Facts behind `src/sources/kysona.rs`: USB ids, the battery interface, the requests, the reply
layout, and how the dongle says "mouse off". Each fact names its source. The code is written from
this note, not from the sources.

Several mouse brands (Kysona, VXE/ATK, Pulsar) ship the same wire protocol; the thread under
pulsar-v1 notes it has no public name. rigbat calls it after the first mainline driver.

The sources are GPL-2.0 (the kernel and the patches posted for it). rigbat is MIT OR Apache-2.0,
so only facts come from them — ids, byte positions, byte values — never code, comments or table
text.

None of this is verified on real hardware: the maintainer owns none of these mice.

## Sources

| Key        | Source                                                                                                                                                                          |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| K-kysona   | Linux [`drivers/hid/hid-kysona.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/drivers/hid/hid-kysona.c) and `hid-ids.h` (GPL-2.0)                  |
| pulsar-v1  | [PATCH: HID: pulsar: add driver for Pulsar gaming mice](https://lkml.iu.edu/hypermail/linux/kernel/2603.2/07112.html), Nikolas Koesling, 2026-03-18 (GPL-2.0)                   |
| atk-lkml   | [Re: PATCH: HID: pulsar: add driver](https://lkml.iu.edu/hypermail/linux/kernel/2603.2/08402.html), 2026-03-19 — ATK VXE R1 SE+ ids, same protocol                              |
| pulsar-rev | [Re: PATCH: HID: pulsar: add driver](https://lkml.iu.edu/hypermail/linux/kernel/2603.2/09542.html), 2026-03-19 — the author on what K-kysona lacks (reply checksum not checked) |
| hid-pulsar | [hid-pulsar](https://gitlab.com/NikolasK-source/hid-pulsar), the out-of-tree (DKMS) driver that grew from pulsar-v1, `hid-ids.h` at commit `b3d97447` (GPL-2.0)                 |

## Ids

All on USB interface 1 (K-kysona, pulsar-v1).

| Vendor   | Product  | rigbat name          | Source               | Note                                         |
| -------- | -------- | -------------------- | -------------------- | -------------------------------------------- |
| `0x3554` | `0xF507` | Pulsar Mouse         | pulsar-v1            | cable                                        |
| `0x3554` | `0xF508` | Pulsar Mouse         | pulsar-v1            | 1 kHz dongle                                 |
| `0x3554` | `0xF509` | Pulsar Mouse         | pulsar-v1            | 4 kHz dongle                                 |
| `0x3554` | `0xF57C` | Kysona M600          | K-kysona             | dongle                                       |
| `0x3554` | `0xF57D` | Kysona M600          | K-kysona             | cable                                        |
| `0x3554` | `0xF58A` | VXE Dragonfly R1 Pro | K-kysona             | dongle                                       |
| `0x3554` | `0xF58C` | VXE Dragonfly R1 Pro | K-kysona             | cable                                        |
| `0x3554` | `0xF58F` | ATK VXE R1 SE+       | atk-lkml, hid-pulsar | cable                                        |
| `0x373B` | `0x1085` | ATK VXE R1 SE+       | atk-lkml, hid-pulsar | dongle; the only id under a second vendor id |

The three Pulsar ids are shared by the X2 V2, X2H, X2A and Xlite V3 (pulsar-v1): the model comes
from a separate info request, not from the product id. rigbat does not send that request, so it
names all three "Pulsar Mouse".

## Kernel coverage

- K-kysona binds `0xF57C`, `0xF57D`, `0xF58A` and `0xF58C` on interface 1 and registers a
  `power_supply` named `kysona-<uniq>-battery` on the HID device.
- hid-pulsar, when installed, binds all nine ids and registers a `power_supply` on the HID device
  too.

The backend skips a hidraw node whose HID device already has a `power_supply` child, so a driver
that reports the battery wins and the mouse is not listed twice. Without such a driver — an older
kernel, or an id no driver lists — rigbat reads the mouse itself.

## Requests

- Output report id `0x08`, 17 bytes including the id (K-kysona, pulsar-v1).
- Byte 1 is the command: `0x03` online status, `0x04` battery (K-kysona, pulsar-v1). Bytes 2–15
  are zero.
- Byte 16 is a checksum: `0x55` minus the sum of bytes 0–15, modulo 256 (pulsar-v1). The two
  constant requests in K-kysona, `08 03 00…00 4A` and `08 04 00…00 49`, agree with it.
- K-kysona sends the request as a SET_REPORT output report. rigbat writes the same 17 bytes to
  `/dev/hidrawN`; whether the mice accept that path is unverified.
- K-kysona asks for online status first, then the battery (K-kysona). rigbat does the same.

## Replies

- A 17-byte input report: byte 0 is `0x08`, byte 1 echoes the command (K-kysona, pulsar-v1).
- Byte 16 follows the request checksum rule (pulsar-v1 checks it; pulsar-rev notes that K-kysona
  does not). rigbat fails the poll on a mismatch instead of reading the bytes as a level.
- Online status: byte 6 is `1` while the mouse is connected, `0` when it is not (pulsar-v1;
  K-kysona reads byte 6 as the online flag). pulsar-v1 rejects any other value; rigbat does too.
- Battery: byte 6 is the percent, byte 7 is `1` while charging (K-kysona; pulsar-v1 reads it as
  "power connected"). pulsar-v1 rejects a percent above 100 or a byte 7 above 1; rigbat does too.
  Bytes 8–9 carry the voltage in mV, big-endian (K-kysona); rigbat does not use it.
- The mouse also sends unprompted reports with command `0x0A` (power events, pulsar-v1). rigbat
  ignores any report whose command byte is not the one it asked for.

## Offline signalling

When the dongle reports online `0`, the poll fails at once with "mouse is asleep or off (dongle
reported it offline)", and no battery request is sent. The supervisor then shows the device as
unreachable, keeping its last reading.

## Handle policy

Request/response: the source holds the handle for its lifetime and, before each request, reads
and discards input already queued (the `0x0A` events above), bounded to a fixed number of
reports.
