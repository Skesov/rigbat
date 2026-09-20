//! 8BitDo Ultimate 2 Wireless battery source — DInput mode only.
//!
//! Protocol: vendor 0x2dc8, product 0x6012 (DInput mode, entered by holding `B`
//! while powering on undocked). XInput mode (`2dc8:310b`, the default) and Switch
//! mode report no usable battery — see `docs/adding-a-device.md` / `README.md`.
//!
//! Unlike `steelseries.rs`, this device is read-only and streaming: input report
//! id `0x01`, 34 bytes, pushed continuously at 1000 Hz. There is no query to write
//! and no response to wait for — a poll just reads the next report off the wire.
//! Battery sits in byte 14: bit 7 is the charging flag, bits 0-6 are the
//! percentage directly (not bucketed). Firmware v1.02 sends 12-byte reports with
//! no battery data; v1.03+ sends 34 — a short report means old firmware, not a
//! broken device.
//!
//! This is the stream-only case of `CLAUDE.md`'s handle policy (see "Key
//! decisions" there), not the request/response case `steelseries.rs` follows:
//! open, read one report, close — do not hold the handle across polls.
//!
//! Discovery: `/sys/class/hidraw/hidrawN/device/uevent` contains
//! `HID_ID=0003:VVVVVVVV:PPPPPPPP`, matched against `DEVICES` the same way the
//! SteelSeries backend does. DInput exposes a single HID interface, so no
//! interface filter is applied here — unlike SteelSeries, which has several
//! interfaces on the same device and must pick the battery-reporting one.

use std::{io::Read as _, os::unix::fs::OpenOptionsExt as _, path::PathBuf};

use anyhow::Context as _;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind, Transport};

use super::{BatteryBackend, BatterySource, hidraw};

// ── Protocol constants ────────────────────────────────────────────────────────

const VENDOR_ID: u16 = 0x2DC8;

/// Input report id carrying the 34-byte streaming report with battery data.
const REPORT_ID: u8 = 0x01;

/// Minimum report length carrying battery data (firmware v1.03+). Shorter
/// reports (firmware v1.02, 12 bytes) have no battery byte.
const MIN_REPORT_LEN: usize = 34;

/// Offset of the battery byte in the input report: bit 7 charging, bits 0-6 percent.
const BATTERY_BYTE_OFFSET: usize = 14;

/// How long to wait for the next streaming report before giving up. The
/// controller pushes reports every ~1 ms at 1000 Hz, so 200 ms is two orders
/// of magnitude of slack over a healthy device; low enough that a
/// switched-off or out-of-range controller does not stall a discovery sweep.
const POLL_TIMEOUT_MS: u16 = 200;

// ── Device table ─────────────────────────────────────────────────────────────

struct EightBitDoDevice {
    product_id: u16,
    name: &'static str,
    kind: DeviceKind,
}

const DEVICES: &[EightBitDoDevice] = &[EightBitDoDevice {
    product_id: 0x6012,
    name: "8BitDo Ultimate 2 Wireless",
    kind: DeviceKind::Controller,
}];

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct EightBitDoBackend;

pub struct EightBitDoSource {
    info: DeviceInfo,
    dev_path: PathBuf, // /dev/hidrawN
}

#[async_trait::async_trait]
impl BatteryBackend for EightBitDoBackend {
    fn name(&self) -> &'static str {
        "eightbitdo"
    }

    async fn discover(
        &self,
        _ctx: &crate::discovery::Context,
    ) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        discover_inner()
    }
}

fn discover_inner() -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
    let hidraw_root = std::path::Path::new("/sys/class/hidraw");

    if !hidraw_root.exists() {
        return Ok(Vec::new());
    }

    let mut sources: Vec<Box<dyn BatterySource>> = Vec::new();

    let entries = std::fs::read_dir(hidraw_root).context("reading /sys/class/hidraw")?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("skipping hidraw entry: {e}");
                continue;
            }
        };

        // try_node returns Err for non-matching nodes — this is normal, silently skip.
        let _ = try_node(&entry.file_name().to_string_lossy(), &mut sources);
    }

    Ok(sources)
}

/// Attempts to add a hidrawN node to the list of sources.
/// Returns Err if the node does not match or an error occurs — the caller skips it.
fn try_node(node_name: &str, sources: &mut Vec<Box<dyn BatterySource>>) -> anyhow::Result<()> {
    let uevent_path = format!("/sys/class/hidraw/{node_name}/device/uevent");
    let uevent =
        std::fs::read_to_string(&uevent_path).with_context(|| format!("reading {uevent_path}"))?;

    let hid_id_value = hidraw::uevent_value(&uevent, "HID_ID")
        .with_context(|| format!("HID_ID not found in {uevent_path}"))?;

    let (vendor, product) = hidraw::parse_hid_id(hid_id_value)
        .with_context(|| format!("parsing HID_ID={hid_id_value}"))?;

    if vendor != VENDOR_ID {
        anyhow::bail!("vendor 0x{vendor:04X} != 0x{VENDOR_ID:04X}");
    }

    let device_desc = DEVICES
        .iter()
        .find(|d| d.product_id == product)
        .with_context(|| format!("product 0x{product:04X} not in device table"))?;

    let dev_path = PathBuf::from(format!("/dev/{node_name}"));

    sources.push(Box::new(EightBitDoSource {
        info: DeviceInfo {
            name: device_desc.name.to_owned(),
            kind: device_desc.kind,
            transport: Transport::Hidraw,
            locator: Some(hidraw::stable_locator(&uevent, node_name)),
        },
        dev_path,
    }));

    Ok(())
}

// ── Source ───────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl BatterySource for EightBitDoSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let path = self.dev_path.clone();

        // Blocking I/O: nix::poll() parks the thread, so run it off the async
        // runtime. Nothing is written to the device, so an aborted task leaves
        // no handle behind that needs to survive to the next poll (unlike
        // steelseries.rs, which passes an open handle in and back out).
        let reading = tokio::task::spawn_blocking(move || poll_device(&path))
            .await
            .context("spawn_blocking")??;

        tracing::debug!(device = %self.info.name, percent = reading.percent, "eightbitdo poll");
        Ok(reading)
    }
}

/// Opens the node, waits for one streaming report, parses it, and closes the
/// handle again — see the module doc comment for why this does not hold the
/// handle open across polls.
fn poll_device(dev_path: &std::path::Path) -> anyhow::Result<BatteryReading> {
    use std::os::fd::AsFd as _;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dev_path)
        .with_context(|| format!("opening {}", dev_path.display()))?;

    let mut poll_fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
    let ready = poll(&mut poll_fds, PollTimeout::from(POLL_TIMEOUT_MS)).context("poll()")?;

    if ready == 0 {
        anyhow::bail!(
            "controller is off or out of range (no report from {} within {POLL_TIMEOUT_MS}ms)",
            dev_path.display()
        );
    }

    let revents = poll_fds[0].revents().unwrap_or(PollFlags::empty());
    if !revents.contains(PollFlags::POLLIN) {
        anyhow::bail!(
            "unexpected poll events {:?} from {}",
            revents,
            dev_path.display()
        );
    }

    let mut buf = [0u8; 64];
    let n = file.read(&mut buf).context("reading hidraw report")?;
    if n == 0 {
        anyhow::bail!("EOF reading from {}", dev_path.display());
    }

    parse_battery_report(&buf[..n]).ok_or_else(|| {
        anyhow::anyhow!(
            "report from {} is not a 34-byte battery report (old firmware sends 12-byte reports with no battery data)",
            dev_path.display()
        )
    })
}

// ── Pure functions ────────────────────────────────────────────────────────────

/// Parses a streaming input report into a battery reading.
///
/// Requires `buf[0] == REPORT_ID` and `buf.len() >= MIN_REPORT_LEN`; a shorter
/// or differently-tagged report yields `None` rather than a guessed reading.
/// `buf[14]` bit 7 is the charging flag, bits 0-6 are the percentage directly
/// (not bucketed like SteelSeries' 5%-step protocol). A fully charged
/// controller still sitting in the dock reports 100% with the charging bit
/// set; that maps to `ChargeState::Charging`, not `Discharging` (which would
/// be a lie) or `Full` (which would imply it is not connected).
pub fn parse_battery_report(buf: &[u8]) -> Option<BatteryReading> {
    if buf.len() < MIN_REPORT_LEN || buf[0] != REPORT_ID {
        return None;
    }

    let raw = buf[BATTERY_BYTE_OFFSET];
    let charging = (raw & 0x80) != 0;
    let percent = raw & 0x7F;

    let state = if charging {
        ChargeState::Charging
    } else {
        ChargeState::Discharging
    };

    Some(BatteryReading::new(percent, state))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // parse_battery_report

    fn captured_report(byte14: u8) -> [u8; 34] {
        // 01 0f 7f 7f 7f 7f 00 00 00 00 00 00 00 00 <b14> b9 ff 58 ... (padded)
        let mut buf = [0u8; 34];
        buf[0] = REPORT_ID;
        buf[1] = 0x0f;
        buf[2..6].copy_from_slice(&[0x7f; 4]);
        buf[14] = byte14;
        buf
    }

    #[test]
    fn real_captured_report_is_65_percent_discharging() {
        // Captured live: byte 14 = 0x41 -> charging=0, 65%.
        let buf = captured_report(0x41);
        let r = parse_battery_report(&buf).unwrap();
        assert_eq!(r.percent, 65);
        assert_eq!(r.state, ChargeState::Discharging);
    }

    #[test]
    fn charging_bit_set_yields_charging() {
        let buf = captured_report(0x80 | 30);
        let r = parse_battery_report(&buf).unwrap();
        assert_eq!(r.percent, 30);
        assert_eq!(r.state, ChargeState::Charging);
    }

    #[test]
    fn full_charge_with_charging_bit_is_charging_at_100() {
        let buf = captured_report(0x80 | 100);
        let r = parse_battery_report(&buf).unwrap();
        assert_eq!(r.percent, 100);
        assert_eq!(r.state, ChargeState::Charging);
    }

    #[test]
    fn short_report_returns_none() {
        let buf = [REPORT_ID; 12];
        assert_eq!(parse_battery_report(&buf), None);
    }

    #[test]
    fn wrong_report_id_returns_none() {
        let mut buf = captured_report(0x41);
        buf[0] = 0x02;
        assert_eq!(parse_battery_report(&buf), None);
    }

    #[test]
    fn empty_report_returns_none() {
        assert_eq!(parse_battery_report(&[]), None);
    }
}
