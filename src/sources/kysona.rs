//! Kysona-protocol mice over `/dev/hidraw`: Kysona M600, VXE Dragonfly R1 Pro,
//! ATK VXE R1 SE+, Pulsar.
//!
//! Request/response on USB interface 1: write a 17-byte output report, read
//! input reports until the answer. The source holds its handle for its lifetime
//! and discards queued input before each request. Ids, requests, reply layout
//! and the source of each fact: `docs/kysona.md`.
//!
//! A node whose HID device already carries a kernel `power_supply` is skipped:
//! the sysfs backend reports that mouse.

use std::{
    fs::File,
    io::{ErrorKind, Read as _, Write as _},
    os::{fd::AsFd as _, unix::fs::OpenOptionsExt as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

use super::{
    BatteryBackend, BatterySource, Context,
    hidraw::{self, HidrawDevice, HidrawFamily, HidrawModel},
};

const ANSWER_TIMEOUT: Duration = Duration::from_millis(1000);
const READ_BUF_LEN: usize = 64;
/// Bounds the discard loop against a device that floods reports.
const MAX_QUEUED_REPORTS: usize = 256;

const MOUSE_OFF: &str = "mouse is asleep or off (dongle reported it offline)";

// ── Device table ─────────────────────────────────────────────────────────────

const KYSONA: u16 = 0x3554;
const ATK_DONGLE: u16 = 0x373B;

const fn mouse(vendor: u16, product: u16, name: &'static str) -> HidrawModel {
    HidrawModel {
        vendor,
        product,
        name,
        kind: DeviceKind::Mouse,
    }
}

/// Cable and dongle of one mouse share a name; the locator tells them apart.
pub static FAMILY: HidrawFamily = HidrawFamily {
    models: &[
        mouse(KYSONA, 0xF507, "Pulsar Mouse"),
        mouse(KYSONA, 0xF508, "Pulsar Mouse"),
        mouse(KYSONA, 0xF509, "Pulsar Mouse"),
        mouse(KYSONA, 0xF57C, "Kysona M600"),
        mouse(KYSONA, 0xF57D, "Kysona M600"),
        mouse(KYSONA, 0xF58A, "VXE Dragonfly R1 Pro"),
        mouse(KYSONA, 0xF58C, "VXE Dragonfly R1 Pro"),
        mouse(KYSONA, 0xF58F, "ATK VXE R1 SE+"),
        mouse(ATK_DONGLE, 0x1085, "ATK VXE R1 SE+"),
    ],
    interface: Some(1),
};

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct KysonaBackend;

#[async_trait::async_trait]
impl BatteryBackend for KysonaBackend {
    fn name(&self) -> &'static str {
        "kysona"
    }

    fn hidraw_family(&self) -> Option<&'static HidrawFamily> {
        Some(&FAMILY)
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        let devices =
            tokio::task::spawn_blocking(|| discover_in(&FAMILY, Path::new(hidraw::SYSFS_HIDRAW)))
                .await
                .context("spawn_blocking")??;
        Ok(devices
            .into_iter()
            .map(|device| -> Box<dyn BatterySource> {
                Box::new(KysonaSource {
                    info: device.info,
                    dev_path: device.dev_path,
                    identity: device.identity,
                    handle: None,
                })
            })
            .collect())
    }
}

fn discover_in(family: &HidrawFamily, sysfs_root: &Path) -> anyhow::Result<Vec<HidrawDevice>> {
    let mut devices = family.discover_in(sysfs_root)?;
    devices.retain(|device| {
        let kernel = hidraw::kernel_reports_battery(sysfs_root, &device.dev_path);
        if kernel {
            tracing::debug!(
                node = %device.dev_path.display(),
                "a kernel driver reports this mouse's battery; left to the sysfs backend"
            );
        }
        !kernel
    });
    Ok(devices)
}

// ── Source ───────────────────────────────────────────────────────────────────

struct KysonaSource {
    info: DeviceInfo,
    dev_path: PathBuf,
    identity: hidraw::NodeIdentity,
    /// Kept across polls; dropped on any error so the next poll reopens by node name.
    handle: Option<File>,
}

#[async_trait::async_trait]
impl BatterySource for KysonaSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let dev_path = self.dev_path.clone();
        let identity = self.identity.clone();
        let handle = self.handle.take();

        let (result, handle) = tokio::task::spawn_blocking(move || {
            let mut handle = handle;
            let result = poll_device(&dev_path, &identity, &mut handle);
            if result.is_err() {
                handle = None;
            }
            (result, handle)
        })
        .await
        .context("spawn_blocking")?;

        self.handle = handle;
        if let Ok(reading) = &result {
            tracing::debug!(device = %self.info.name, percent = reading.percent, "kysona poll");
        }
        result
    }
}

fn poll_device(
    dev_path: &Path,
    identity: &hidraw::NodeIdentity,
    handle: &mut Option<File>,
) -> anyhow::Result<BatteryReading> {
    if handle.is_none() {
        let file = hidraw::open_verified(
            Path::new(hidraw::SYSFS_HIDRAW),
            dev_path,
            identity,
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK),
        )?;
        *handle = Some(file);
    }
    let file = handle
        .as_mut()
        .context("handle unexpectedly empty after open")?;

    exchange(file, Command::Online, classify_online)?;
    exchange(file, Command::Battery, classify_battery)
}

/// Writes the request for `command` and reads until `classify` recognises the answer.
fn exchange<T>(
    file: &mut File,
    command: Command,
    classify: impl Fn(&[u8]) -> Reply<T>,
) -> anyhow::Result<T> {
    discard_queued(file)?;
    file.write_all(&request(command))
        .context("writing request")?;

    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut buf = [0u8; READ_BUF_LEN];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let remaining_ms = u16::try_from(remaining.as_millis()).unwrap_or(u16::MAX);
        if remaining_ms == 0 {
            anyhow::bail!("timeout waiting for the mouse's answer");
        }
        let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, PollTimeout::from(remaining_ms)).context("poll()")? == 0 {
            anyhow::bail!("timeout waiting for the mouse's answer");
        }
        let revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if !revents.contains(PollFlags::POLLIN) {
            anyhow::bail!("unexpected poll events {revents:?}");
        }
        let n = match file.read(&mut buf) {
            Ok(0) => anyhow::bail!("EOF reading hidraw"),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e).context("reading hidraw"),
        };
        match classify(buf.get(..n).unwrap_or_default()) {
            Reply::Answer(answer) => return Ok(answer),
            Reply::MouseOff => anyhow::bail!(MOUSE_OFF),
            Reply::Invalid(reason) => anyhow::bail!("invalid reply: {reason}"),
            Reply::Unrelated => {}
        }
    }
}

/// Reads and drops input queued since the last exchange, so a stale report is
/// not taken for the answer.
fn discard_queued(file: &mut File) -> anyhow::Result<()> {
    let mut buf = [0u8; READ_BUF_LEN];
    for _ in 0..MAX_QUEUED_REPORTS {
        match file.read(&mut buf) {
            Ok(0) => anyhow::bail!("EOF reading hidraw"),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e).context("discarding queued reports"),
        }
    }
    Ok(())
}

// ── Protocol (pure) ──────────────────────────────────────────────────────────

const REPORT_ID: u8 = 0x08;
const REPORT_LEN: usize = 17;
const CHECKSUM_BASE: u8 = 0x55;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Online,
    Battery,
}

impl Command {
    const fn byte(self) -> u8 {
        match self {
            Self::Online => 0x03,
            Self::Battery => 0x04,
        }
    }
}

/// What one input report means to the exchange waiting on it.
#[derive(Debug, PartialEq, Eq)]
enum Reply<T> {
    Answer(T),
    /// The dongle answered, the mouse behind it is not connected.
    MouseOff,
    /// The answer to this request, but its bytes cannot be a reading.
    Invalid(&'static str),
    /// Another report on the same interface. Keep reading.
    Unrelated,
}

impl<T> Reply<T> {
    fn and_then<U>(self, f: impl FnOnce(T) -> Reply<U>) -> Reply<U> {
        match self {
            Self::Answer(answer) => f(answer),
            Self::MouseOff => Reply::MouseOff,
            Self::Invalid(reason) => Reply::Invalid(reason),
            Self::Unrelated => Reply::Unrelated,
        }
    }
}

/// `0x55` minus the sum of every byte but the last, modulo 256.
fn checksum(report: &[u8; REPORT_LEN]) -> u8 {
    report
        .iter()
        .take(REPORT_LEN - 1)
        .fold(CHECKSUM_BASE, |sum, &byte| sum.wrapping_sub(byte))
}

fn request(command: Command) -> [u8; REPORT_LEN] {
    let mut report = [0u8; REPORT_LEN];
    report[0] = REPORT_ID;
    report[1] = command.byte();
    report[REPORT_LEN - 1] = checksum(&report);
    report
}

/// The reply to `command` with its checksum checked.
fn checked_reply(report: &[u8], command: Command) -> Reply<[u8; REPORT_LEN]> {
    let Ok(report) = <[u8; REPORT_LEN]>::try_from(report) else {
        return Reply::Unrelated;
    };
    if report[0] != REPORT_ID || report[1] != command.byte() {
        return Reply::Unrelated;
    }
    if report[REPORT_LEN - 1] != checksum(&report) {
        return Reply::Invalid("checksum mismatch");
    }
    Reply::Answer(report)
}

fn classify_online(report: &[u8]) -> Reply<()> {
    checked_reply(report, Command::Online).and_then(|report| match report[6] {
        1 => Reply::Answer(()),
        0 => Reply::MouseOff,
        _ => Reply::Invalid("online flag out of range"),
    })
}

fn classify_battery(report: &[u8]) -> Reply<BatteryReading> {
    checked_reply(report, Command::Battery).and_then(|report| {
        let state = match report[7] {
            0 => ChargeState::Discharging,
            1 => ChargeState::Charging,
            _ => return Reply::Invalid("charging flag out of range"),
        };
        match report[6] {
            percent @ 0..=100 => Reply::Answer(BatteryReading::new(percent, state)),
            _ => Reply::Invalid("percent out of range"),
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::hidraw::fake_sysfs::FakeSysfs;

    /// A reply to `command` with `payload` from byte 6 on and a valid checksum.
    fn reply(command: Command, payload: &[u8]) -> [u8; REPORT_LEN] {
        let mut report = [0u8; REPORT_LEN];
        report[0] = REPORT_ID;
        report[1] = command.byte();
        report[6..6 + payload.len()].copy_from_slice(payload);
        report[REPORT_LEN - 1] = checksum(&report);
        report
    }

    // Requests

    #[test]
    fn requests_match_the_kernel_constants() {
        let mut battery = [0u8; REPORT_LEN];
        battery[..2].copy_from_slice(&[0x08, 0x04]);
        battery[16] = 0x49;
        assert_eq!(request(Command::Battery), battery);

        let mut online = [0u8; REPORT_LEN];
        online[..2].copy_from_slice(&[0x08, 0x03]);
        online[16] = 0x4A;
        assert_eq!(request(Command::Online), online);
    }

    // Replies

    #[test]
    fn battery_reads_percent_and_charging() {
        assert_eq!(
            classify_battery(&reply(Command::Battery, &[73, 0, 0x0F, 0xA0])),
            Reply::Answer(BatteryReading::new(73, ChargeState::Discharging))
        );
        assert_eq!(
            classify_battery(&reply(Command::Battery, &[40, 1])),
            Reply::Answer(BatteryReading::new(40, ChargeState::Charging))
        );
    }

    #[test]
    fn a_checksum_mismatch_is_not_a_reading() {
        let mut report = reply(Command::Battery, &[73, 0]);
        report[16] ^= 0x01;
        assert_eq!(
            classify_battery(&report),
            Reply::Invalid("checksum mismatch")
        );
        let mut report = reply(Command::Online, &[1]);
        report[6] = 0;
        assert_eq!(
            classify_online(&report),
            Reply::Invalid("checksum mismatch")
        );
    }

    #[test]
    fn battery_rejects_out_of_range_bytes() {
        assert!(matches!(
            classify_battery(&reply(Command::Battery, &[101, 0])),
            Reply::Invalid(_)
        ));
        assert!(matches!(
            classify_battery(&reply(Command::Battery, &[50, 2])),
            Reply::Invalid(_)
        ));
    }

    #[test]
    fn online_flag_means_connected_or_off() {
        assert_eq!(
            classify_online(&reply(Command::Online, &[1])),
            Reply::Answer(())
        );
        assert_eq!(
            classify_online(&reply(Command::Online, &[0])),
            Reply::MouseOff
        );
        assert!(matches!(
            classify_online(&reply(Command::Online, &[2])),
            Reply::Invalid(_)
        ));
    }

    #[test]
    fn other_reports_are_unrelated() {
        let mut event = reply(Command::Battery, &[0x40]);
        event[1] = 0x0A;
        assert_eq!(classify_battery(&event), Reply::Unrelated);
        assert_eq!(
            classify_battery(&reply(Command::Online, &[1])),
            Reply::Unrelated
        );
        assert_eq!(
            classify_online(&reply(Command::Battery, &[50, 0])),
            Reply::Unrelated
        );
        let short = reply(Command::Battery, &[50, 0]);
        assert_eq!(classify_battery(&short[..16]), Reply::Unrelated);
        let mut other_id = reply(Command::Battery, &[50, 0]);
        other_id[0] = 0x07;
        assert_eq!(classify_battery(&other_id), Reply::Unrelated);
    }

    // Device table

    #[test]
    fn every_model_is_a_mouse_on_interface_1() {
        assert_eq!(FAMILY.interface, Some(1));
        for model in FAMILY.models {
            assert_eq!(model.kind, DeviceKind::Mouse, "{}", model.name);
        }
    }

    // Discovery against a fake sysfs tree

    #[test]
    fn discover_in_finds_both_vendor_ids_on_interface_1() {
        let sysfs = FakeSysfs::new("kysona-vendors");
        sysfs.add("hidraw0", 0, KYSONA, 0xF508, "");
        sysfs.add("hidraw1", 1, KYSONA, 0xF508, "");
        sysfs.add("hidraw2", 1, ATK_DONGLE, 0x1085, "");
        sysfs.add("hidraw3", 1, ATK_DONGLE, 0xF508, "");
        let devices = discover_in(&FAMILY, &sysfs.class()).unwrap();
        assert!(devices.iter().all(|d| d.info.kind == DeviceKind::Mouse));
        let mut found: Vec<_> = devices
            .into_iter()
            .map(|d| (d.dev_path, d.info.name))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                (PathBuf::from("/dev/hidraw1"), "Pulsar Mouse".to_owned()),
                (PathBuf::from("/dev/hidraw2"), "ATK VXE R1 SE+".to_owned()),
            ]
        );
    }

    #[test]
    fn discover_in_leaves_a_kernel_reported_mouse_to_sysfs() {
        let sysfs = FakeSysfs::new("kysona-kernel");
        sysfs.add("hidraw1", 1, KYSONA, 0xF57C, "");
        sysfs.add("hidraw2", 1, KYSONA, 0xF58A, "");
        sysfs.add_power_supply("hidraw1", "kysona-hidraw1-battery");
        let found = discover_in(&FAMILY, &sysfs.class()).unwrap();
        let names: Vec<_> = found.iter().map(|d| d.info.name.as_str()).collect();
        assert_eq!(names, ["VXE Dragonfly R1 Pro"]);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn checksum_round_trips(
                body in any::<[u8; REPORT_LEN - 1]>(),
                wrong in 1u8..=u8::MAX,
            ) {
                let mut report = [0u8; REPORT_LEN];
                report[..REPORT_LEN - 1].copy_from_slice(&body);
                report[0] = REPORT_ID;
                report[1] = Command::Battery.byte();
                report[REPORT_LEN - 1] = checksum(&report);
                prop_assert_eq!(checked_reply(&report, Command::Battery), Reply::Answer(report));
                report[REPORT_LEN - 1] = report[REPORT_LEN - 1].wrapping_add(wrong);
                prop_assert_eq!(
                    checked_reply(&report, Command::Battery),
                    Reply::Invalid("checksum mismatch")
                );
            }

            #[test]
            fn parsers_never_panic_on_a_full_report(report in any::<[u8; REPORT_LEN]>()) {
                if let Reply::Answer(reading) = classify_battery(&report) {
                    prop_assert!(reading.percent <= 100);
                }
                let _ = classify_online(&report);
            }

            #[test]
            fn parsers_never_panic_on_any_length(report in prop::collection::vec(any::<u8>(), 0..70)) {
                let _ = classify_battery(&report);
                let _ = classify_online(&report);
            }
        }
    }
}
