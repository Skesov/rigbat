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
//! Discovery: [`FAMILY`] through `hidraw::discover`. DInput exposes a single HID
//! interface, so no interface filter is applied — unlike SteelSeries, which has
//! several interfaces on the same device and must pick the battery-reporting one.

use std::{io::Read as _, os::unix::fs::OpenOptionsExt as _, path::PathBuf};

use anyhow::Context as _;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

use super::{BatteryBackend, BatterySource, Context, hidraw};

// ── Protocol constants ────────────────────────────────────────────────────────

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

pub static FAMILY: hidraw::HidrawFamily = hidraw::HidrawFamily {
    vendor: 0x2DC8,
    models: &[hidraw::HidrawModel {
        product: 0x6012,
        name: "8BitDo Ultimate 2 Wireless",
        kind: DeviceKind::Controller,
    }],
    interface: None,
};

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct EightBitDoBackend;

struct EightBitDoSource {
    info: DeviceInfo,
    dev_path: PathBuf, // /dev/hidrawN
    identity: hidraw::NodeIdentity,
}

#[async_trait::async_trait]
impl BatteryBackend for EightBitDoBackend {
    fn name(&self) -> &'static str {
        "eightbitdo"
    }

    fn hidraw_family(&self) -> Option<&'static hidraw::HidrawFamily> {
        Some(&FAMILY)
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        let devices = hidraw::discover(&FAMILY).await?;
        Ok(devices
            .into_iter()
            .map(|device| -> Box<dyn BatterySource> {
                Box::new(EightBitDoSource {
                    info: device.info,
                    dev_path: device.dev_path,
                    identity: device.identity,
                })
            })
            .collect())
    }
}

// ── Source ───────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl BatterySource for EightBitDoSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let path = self.dev_path.clone();
        let identity = self.identity.clone();

        // Blocking I/O: nix::poll() parks the thread, so run it off the async
        // runtime. Nothing is written to the device, so an aborted task leaves
        // no handle behind that needs to survive to the next poll (unlike
        // steelseries.rs, which passes an open handle in and back out).
        let reading = tokio::task::spawn_blocking(move || poll_device(&path, &identity))
            .await
            .context("spawn_blocking")??;

        tracing::debug!(device = %self.info.name, percent = reading.percent, "eightbitdo poll");
        Ok(reading)
    }
}

/// Opens the node, waits for one streaming report, parses it, and closes the
/// handle again — see the module doc comment for why this does not hold the
/// handle open across polls.
fn poll_device(
    dev_path: &std::path::Path,
    identity: &hidraw::NodeIdentity,
) -> anyhow::Result<BatteryReading> {
    use std::os::fd::AsFd as _;

    let mut file = hidraw::open_verified(
        std::path::Path::new(hidraw::SYSFS_HIDRAW),
        dev_path,
        identity,
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK),
    )?;

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
fn parse_battery_report(buf: &[u8]) -> Option<BatteryReading> {
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

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn accepts_only_a_full_report_and_stays_in_range(
                buf in prop::collection::vec(any::<u8>(), 0..70),
            ) {
                let parsed = parse_battery_report(&buf);
                prop_assert_eq!(
                    parsed.is_some(),
                    buf.len() >= MIN_REPORT_LEN && buf[0] == REPORT_ID
                );
                if let Some(r) = parsed {
                    prop_assert!(r.percent <= 100);
                    let charging = buf[BATTERY_BYTE_OFFSET] & 0x80 != 0;
                    prop_assert_eq!(r.state == ChargeState::Charging, charging);
                }
            }

            #[test]
            fn round_trips_every_percentage(percent in 0u8..=100, charging in any::<bool>()) {
                let buf = captured_report(percent | if charging { 0x80 } else { 0 });
                let state = if charging { ChargeState::Charging } else { ChargeState::Discharging };
                prop_assert_eq!(
                    parse_battery_report(&buf),
                    Some(BatteryReading::new(percent, state))
                );
            }
        }
    }
}
