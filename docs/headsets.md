# Wireless headsets: protocol notes

Facts behind `src/sources/headsets.rs`: USB ids, the battery interface, the request, the
response layout, and how the dongle says "charging" or "headset off". Each fact names its source.
The code is written from this note, not from the sources.

Several sources are GPL (HeadsetControl GPL-3.0, the kernel GPL-2.0). rigbat is
MIT OR Apache-2.0, so only facts come from them — ids, byte positions, byte values — never code,
comments or table text.

None of this is verified on real hardware: the maintainer owns none of these headsets.

## Sources

| Key       | Source                                                                                                                                                                                 |
| --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| HC        | [HeadsetControl](https://github.com/Sapd/HeadsetControl) at commit `25dadae`, `lib/devices/` (GPL-3.0)                                                                                 |
| K-arctis  | Linux [`drivers/hid/hid-steelseries-arctis.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/drivers/hid/hid-steelseries-arctis.c) and `hid-ids.h` (GPL-2.0) |
| K-void    | Linux [`drivers/hid/hid-corsair-void.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/drivers/hid/hid-corsair-void.c) (GPL-2.0)                             |
| K-hidpp   | Linux [`drivers/hid/hid-logitech-hidpp.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/drivers/hid/hid-logitech-hidpp.c) (GPL-2.0)                         |
| aarol     | [Creating a battery indicator app for my SteelSeries Arctis headset](https://aarol.dev/posts/arctis-hid/) — USB capture of an Arctis Nova 7                                            |
| G533-lkml | [PATCH v3: logitech-hidpp: add support for Logitech G533 headset](https://lkml.iu.edu/hypermail/linux/kernel/2007.0/03993.html)                                                        |
| phoronix  | [Arctis Nova 5X and Nova 7 supported by Linux 7.3](https://www.phoronix.com/news/SteelSeries-Nova-7-5X-2026)                                                                           |

## Common shape

- Every family below is request/response on one HID interface: write an output report, read
  input reports until the one that answers it. rigbat holds the handle for the source's
  lifetime (see `CLAUDE.md`, "Source port").
- Before each request rigbat reads and discards whatever input is already queued. The Corsair
  receiver sends its status report unprompted on button and mic events (K-void), so a held
  handle accumulates stale answers between polls.
- Byte positions below are as `read(2)` on `/dev/hidrawN` returns them: byte 0 is the report ID
  when the interface uses numbered reports.
- When the dongle answers but says the headset is off or out of range, the poll fails with
  "headset is off or out of range" at once, instead of waiting out the read timeout. The
  supervisor then shows the device as unreachable, keeping its last reading.

## Kernel coverage

Mainline Linux already exposes some of these headsets as a `power_supply` (scope `Device`),
which rigbat's sysfs backend reads:

- K-arctis: Arctis 1 Wireless Xbox, Arctis 7 (2018), Arctis 9, Arctis Nova 5X and the Arctis
  Nova 7 family. The Nova ids arrive in Linux 7.3 (phoronix).
- K-void: Corsair Void, Void Pro and Void Elite (wireless and wired).
- K-hidpp: Logitech G935.

The headsets backend skips a hidraw node whose HID device already has a `power_supply` child, so
a kernel that reports the battery wins and the headset is not listed twice. On an older kernel
the node has no such child and rigbat reads the headset itself.

## SteelSeries Arctis 1 family

- Vendor `0x1038`, interface 3 (HC; K-arctis: sync interface 3).
- Ids (HC): `0x12B3` Arctis 1 Wireless, `0x12B6` Arctis 1 Wireless Xbox, `0x12D7` Arctis 7X,
  `0x12D5` Arctis 7P. K-arctis lists `0x12B6`.
- Request: `06 12` (HC, K-arctis).
- Response: `06 12 <link> <level> …`, 8 bytes (HC, K-arctis).
  - `<link>` = `0x01`: headset not connected (HC, K-arctis).
  - `<level>`: percent, 0–100 (HC, K-arctis).
- Charging: not reported (HC).

## SteelSeries Arctis Nova 7 family

- Vendor `0x1038`, interface 3 (HC, aarol; K-arctis: sync interface 3, async interface 5).
- Request: `00 b0` — report ID 0, then `0xB0` (HC, aarol, K-arctis).
- Response: `b0 <link> <level> <power> …` (aarol, K-arctis, HC).
  - `<link>` = `0x03` while the headset is connected (aarol observed `03`; K-arctis).
  - `<power>`: `0x00` headset not connected, `0x01` charging, `0x03` discharging (aarol); HC also
    counts `0x02` as charging. rigbat reads `0x01`/`0x02` as charging.
  - rigbat treats the headset as off when `<link>` ≠ `0x03` or `<power>` = `0x00`.
- `<level>` scale depends on the product id (HC, K-arctis agree on the split):
  - Steps 0–4, each 25% (aarol: "a battery state of 2 could mean anything from 49% to 26%"):
    `0x2202` Nova 7, `0x2206` Nova 7X, `0x223A` Nova 7 Diablo IV, `0x227A` Nova 7 WoW Edition,
    `0x22A4` Nova 7X, `0x220A` Nova 7P. rigbat marks these readings coarse.
  - Percent 0–100, the ids the January 2026 firmware and the Gen 2 models report: `0x22A1`
    Nova 7, `0x227E` Nova 7 Gen 2, `0x2258`/`0x229E`/`0x22AD` Nova 7X Gen 2, `0x22A9` Nova 7
    Diablo IV, `0x22A5` Nova 7X, `0x22A7` and `0x2298` Nova 7P.
- A firmware update moves a headset from a steps id to a percent id. Both ids carry the same
  rigbat name, so the device keeps its identity and settings across the update.

## Corsair Void family

- Vendor `0x1B1C`, interface 3 (HC: usage page `0xFFC5`).
- Ids, wireless receivers only (K-void; HC lists a subset): Void Wireless `0x0A0C`, `0x0A2B`,
  `0x1B23`, `0x1B25`, `0x1B27`; Void Pro Wireless `0x0A14`, `0x0A16`, `0x0A1A`; Void Elite
  Wireless `0x0A51`, `0x0A55`, `0x0A75`. Wired Voids report no battery.
- Request: output report `c9 64` — report ID `0xC9`, then the ID of the report wanted, `0x64`
  (HC, K-void).
- Response: input report `0x64`, 5 bytes: `64 <buttons> <level> <link> <power>` (HC, K-void).
  - `<level>`: bit 7 is the mic boom position; bits 0–6 are the percent (HC, K-void).
  - `<link>` = 177 (`0xB1`) while the headset is connected; other values mean wired,
    initialising, lost or searching (K-void).
  - `<power>`: 0 disconnected, 1 normal, 2 low, 3 critical, 4 fully charged, 5 charging (K-void;
    HC reads 4 as charging too).
- The level reads high while charging (K-void), so rigbat marks a charging reading coarse.

## Logitech G533

- Vendor `0x046D`, product `0x0A66`, interface 3 (HC).
- HID++ 2.0 long reports: 20 bytes, report ID `0x11`, device index `0xFF` (HC, K-hidpp).
  Byte 2 is the feature index, byte 3 is `function << 4 | software id`, parameters follow.
- Battery comes from feature `0x1F20` ADC Measurement (G533-lkml, K-hidpp). HC addresses it at a
  fixed feature index; rigbat asks the root feature (index 0, function 0 `getFeature`, parameter
  the feature id) for the index instead, as K-hidpp does, and caches it with the open handle.
  - Root reply: byte 4 is the index; 0 means the feature is absent (K-hidpp).
  - ADC reply (function 0): bytes 4–5 are the battery voltage in mV, big-endian; byte 6 is the
    status: `0x01` discharging, `0x03` charging, `0x07` full (K-hidpp, HC).
- Error reply: byte 2 = `0xFF`, then the feature index and function byte of the failed request,
  then an error code. HC treats it as "headset off"; K-hidpp as "not online". rigbat treats it as
  off. HC also treats a voltage below its calibrated range as unavailable; rigbat treats a
  voltage under 3000 mV as off.
- Voltage to percent: rigbat's own piecewise-linear approximation of a single-cell Li-ion
  discharge curve under light load (4150 mV = 100%, 3810 mV = 50%, 3450 mV = 0%; the full table is
  `VOLTAGE_CURVE` in the source). It is an estimate, not a measurement, so these readings are
  coarse: no time-remaining estimate is drawn from them.

## Left out

| Model                                               | Why                                                                                                                                                                               |
| --------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| HyperX Cloud Alpha Wireless (`03f0:098d`)           | The protocol is known (`21 bb 0b` level, `21 bb 03` link, `21 bb 0c` charging), but every source opens the first enumerated HID interface; the interface number is not published. |
| HyperX Cloud Flight (`0951:16c4`, `0951:1723`)      | Reports a voltage with no published off signal; the only percent mapping is a fitted polynomial in HC.                                                                            |
| HyperX Cloud II Wireless (`03f0:0696`, `03f0:018b`) | No off signal in any source.                                                                                                                                                      |
| HyperX Cloud Stinger wireless                       | No source.                                                                                                                                                                        |
| Logitech G633/G635/G933/G733                        | HC matches them by usage page `0xFF43`, not by interface; the interface number is not published.                                                                                  |
| Logitech G935                                       | Covered by K-hidpp.                                                                                                                                                               |
| SteelSeries Arctis 7 (2017/2019), Arctis Pro        | Interface 5 and a two-step query (`06 14` link, `06 18` level); HC has no off signal and K-arctis covers only `0x12AD`. Candidate for a follow-up.                                |
| SteelSeries Arctis 9, Nova 5/5X, Nova Pro           | Different requests or response layouts per source; not pinned down.                                                                                                               |
| Corsair Virtuoso, HS80, Void Wireless V2            | Different protocol (HC `corsair_void_v2w`, `corsair_virtuoso_xt`); not pinned down.                                                                                               |
