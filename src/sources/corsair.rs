//! Corsair wireless mice and keyboards over `/dev/hidraw` ("bragi"), on the
//! cable or through a Slipstream receiver.
//!
//! Request/response on USB interface 1: write a 65-byte GET of a property, read
//! the 64-byte reply. A receiver is asked which paired device is connected,
//! then that device's battery. The source holds its handle for its lifetime and
//! discards queued input before each request. Ids, requests, reply layout and
//! the source of each fact: `docs/corsair.md`.

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
    hidraw::{self, HidrawFamily, HidrawModel},
};

const ANSWER_TIMEOUT: Duration = Duration::from_millis(1000);
const READ_BUF_LEN: usize = 64;
/// Bounds the discard loop against a device that floods reports.
const MAX_QUEUED_REPORTS: usize = 256;

const NONE_CONNECTED: &str = "device is asleep or off (receiver lists no connected device)";
const PAIRED_ERROR: &str = "device is asleep or off (receiver reported an error for it)";

// ── Device table ─────────────────────────────────────────────────────────────

const CORSAIR: u16 = 0x1B1C;

const fn mouse(product: u16, name: &'static str) -> HidrawModel {
    HidrawModel {
        vendor: CORSAIR,
        product,
        name,
        kind: DeviceKind::Mouse,
    }
}

const fn keyboard(product: u16, name: &'static str) -> HidrawModel {
    HidrawModel {
        vendor: CORSAIR,
        product,
        name,
        kind: DeviceKind::Keyboard,
    }
}

/// Products that are Slipstream receivers; every other row is a device on its cable.
const RECEIVERS: [u16; 7] = [0x1BDC, 0x1BA6, 0x2B00, 0x1B7F, 0x1B81, 0x1B65, 0x2B07];

/// Cable and receiver of one model share a name; the locator tells them apart.
pub static FAMILY: HidrawFamily = HidrawFamily {
    models: &[
        mouse(0x1BDC, "Corsair Slipstream Receiver"),
        mouse(0x1BA6, "Corsair Slipstream Receiver"),
        mouse(0x2B00, "Corsair Slipstream Receiver"),
        mouse(0x1B7F, "Corsair Dark Core RGB Pro SE"),
        mouse(0x1B81, "Corsair Dark Core RGB Pro"),
        mouse(0x1B65, "Corsair Harpoon RGB Wireless"),
        keyboard(0x2B07, "Corsair K65 Plus Wireless"),
        mouse(0x1B4C, "Corsair Ironclaw RGB Wireless"),
        mouse(0x2B32, "Corsair Ironclaw Wireless SE"),
        mouse(0x1B5E, "Corsair Harpoon RGB Wireless"),
        mouse(0x2B2E, "Corsair Harpoon V2 Wireless"),
        mouse(0x1B7E, "Corsair Dark Core RGB Pro SE"),
        mouse(0x1B80, "Corsair Dark Core RGB Pro"),
        mouse(0x1BB2, "Corsair Darkstar RGB Wireless"),
        mouse(0x1BB5, "Corsair M65 RGB Ultra Wireless"),
        mouse(0x1BD6, "Corsair M65 RGB Ultra Wireless"),
        mouse(0x1BF1, "Corsair M75 Wireless"),
        mouse(0x2B08, "Corsair M75 Wireless"),
        mouse(0x1BF2, "Corsair M75 Air Wireless"),
        mouse(0x1BB8, "Corsair Nightsabre Wireless"),
        mouse(0x1B98, "Corsair Sabre RGB Pro Wireless"),
        mouse(0x1BDB, "Corsair Scimitar RGB Elite Wireless"),
        mouse(0x2B22, "Corsair Scimitar Elite Wireless SE"),
        keyboard(0x1B6E, "Corsair K57 RGB Wireless"),
        keyboard(0x2B10, "Corsair K65 Plus Wireless"),
        keyboard(0x2B11, "Corsair K65 Plus Wireless"),
        keyboard(0x2B23, "Corsair K65 Plus Wireless"),
        keyboard(0x2B02, "Corsair K70 Core TKL Wireless"),
    ],
    interface: Some(1),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Cable,
    Receiver,
}

fn link_of(product: u16) -> Link {
    if RECEIVERS.contains(&product) {
        Link::Receiver
    } else {
        Link::Cable
    }
}

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct CorsairBackend;

#[async_trait::async_trait]
impl BatteryBackend for CorsairBackend {
    fn name(&self) -> &'static str {
        "corsair"
    }

    fn hidraw_family(&self) -> Option<&'static HidrawFamily> {
        Some(&FAMILY)
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        let devices = hidraw::discover(&FAMILY).await?;
        Ok(devices
            .into_iter()
            .map(|device| -> Box<dyn BatterySource> {
                Box::new(CorsairSource {
                    link: link_of(device.identity.product),
                    info: device.info,
                    dev_path: device.dev_path,
                    identity: device.identity,
                    handle: None,
                })
            })
            .collect())
    }
}

// ── Source ───────────────────────────────────────────────────────────────────

struct CorsairSource {
    info: DeviceInfo,
    link: Link,
    dev_path: PathBuf,
    identity: hidraw::NodeIdentity,
    /// Kept across polls; dropped on any error so the next poll reopens by node name.
    handle: Option<File>,
}

#[async_trait::async_trait]
impl BatterySource for CorsairSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let dev_path = self.dev_path.clone();
        let identity = self.identity.clone();
        let link = self.link;
        let handle = self.handle.take();

        let (result, handle) = tokio::task::spawn_blocking(move || {
            let mut handle = handle;
            let result = poll_device(&dev_path, &identity, link, &mut handle);
            if result.is_err() {
                handle = None;
            }
            (result, handle)
        })
        .await
        .context("spawn_blocking")?;

        self.handle = handle;
        if let Ok(reading) = &result {
            tracing::debug!(device = %self.info.name, percent = reading.percent, "corsair poll");
        }
        result
    }
}

fn poll_device(
    dev_path: &Path,
    identity: &hidraw::NodeIdentity,
    link: Link,
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

    match link {
        Link::Cable => {
            let level = get(file, Target::Cable, Property::BatteryLevel)?.map_err(|code| {
                anyhow::anyhow!("device refused the battery request: 0x{code:02X}")
            })?;
            let status = get(file, Target::Cable, Property::BatteryStatus)?.ok();
            reading(level, status)
        }
        Link::Receiver => {
            let subdevices =
                get(file, Target::Receiver, Property::Subdevices)?.map_err(|code| {
                    anyhow::anyhow!("receiver refused the paired-device request: 0x{code:02X}")
                })?;
            let paired = first_connected(subdevices).context(NONE_CONNECTED)?;
            let target = Target::Paired(paired);
            let level = get(file, target, Property::BatteryLevel)?
                .map_err(|_| anyhow::anyhow!(PAIRED_ERROR))?;
            let status = get(file, target, Property::BatteryStatus)?.ok();
            reading(level, status)
        }
    }
}

fn reading(level: u32, status: Option<u32>) -> anyhow::Result<BatteryReading> {
    let percent =
        level_percent(level).map_err(|reason| anyhow::anyhow!("invalid reply: {reason}"))?;
    Ok(BatteryReading::new(percent, charge_state(status)))
}

/// Sends a GET of `property` to `target` and waits for its reply: the value, or
/// the device's non-zero error code.
fn get(file: &mut File, target: Target, property: Property) -> anyhow::Result<Result<u32, u8>> {
    discard_queued(file)?;
    file.write_all(&request(target, property))
        .context("writing request")?;

    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut buf = [0u8; READ_BUF_LEN];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let remaining_ms = u16::try_from(remaining.as_millis()).unwrap_or(u16::MAX);
        if remaining_ms == 0 {
            anyhow::bail!("timeout waiting for the device's answer");
        }
        let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, PollTimeout::from(remaining_ms)).context("poll()")? == 0 {
            anyhow::bail!("timeout waiting for the device's answer");
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
        match parse_reply(buf.get(..n).unwrap_or_default(), target) {
            Reply::Value(value) => return Ok(Ok(value)),
            Reply::Error(code) => return Ok(Err(code)),
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

/// Report id 0 (unnumbered reports) plus a 64-byte report.
const REQUEST_LEN: usize = 1 + READ_BUF_LEN;
const ADDRESS_BASE: u8 = 0x08;
const DEVICE_ID_MASK: u8 = 0x07;
const GET: u8 = 0x02;
const LEVEL_MAX: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Property {
    BatteryLevel,
    BatteryStatus,
    Subdevices,
}

impl Property {
    const fn byte(self) -> u8 {
        match self {
            Self::BatteryLevel => 0x0F,
            Self::BatteryStatus => 0x10,
            Self::Subdevices => 0x36,
        }
    }
}

/// A paired device's id on its receiver, 1 to 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PairedId(u8);

/// Who a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// The device on its cable.
    Cable,
    /// The receiver itself.
    Receiver,
    /// A device paired to the receiver.
    Paired(PairedId),
}

impl Target {
    const fn address(self) -> u8 {
        match self {
            Self::Cable | Self::Receiver => ADDRESS_BASE,
            Self::Paired(PairedId(id)) => ADDRESS_BASE | id,
        }
    }

    /// The device id a receiver's reply carries in byte 0; no source pins byte 0 on a cable.
    const fn reply_id(self) -> Option<u8> {
        match self {
            Self::Cable => None,
            Self::Receiver => Some(0),
            Self::Paired(PairedId(id)) => Some(id),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Value(u32),
    Error(u8),
    /// Not the reply to this request. Keep reading.
    Unrelated,
}

fn request(target: Target, property: Property) -> [u8; REQUEST_LEN] {
    let mut report = [0u8; REQUEST_LEN];
    report[1] = target.address();
    report[2] = GET;
    report[3] = property.byte();
    report
}

fn parse_reply(report: &[u8], target: Target) -> Reply {
    let [first, command, error, b0, b1, b2, ..] = *report else {
        return Reply::Unrelated;
    };
    if command != GET {
        return Reply::Unrelated;
    }
    if let Some(id) = target.reply_id()
        && first & DEVICE_ID_MASK != id
    {
        return Reply::Unrelated;
    }
    if error != 0 {
        return Reply::Error(error);
    }
    Reply::Value(u32::from_le_bytes([b0, b1, b2, 0]))
}

/// The lowest-numbered paired device whose bit is set in the receiver's bitfield.
fn first_connected(bitfield: u32) -> Option<PairedId> {
    (1..=DEVICE_ID_MASK)
        .find(|&id| bitfield & (1 << id) != 0)
        .map(PairedId)
}

/// Tenths of a percent to a percent; 0 is not a reading.
fn level_percent(level: u32) -> Result<u8, &'static str> {
    match level {
        0 => Err("battery level 0"),
        1..=LEVEL_MAX => u8::try_from(level / 10).map_err(|_| "battery level out of range"),
        _ => Err("battery level above 100 %"),
    }
}

fn charge_state(status: Option<u32>) -> ChargeState {
    match status {
        Some(1) => ChargeState::Charging,
        Some(3) => ChargeState::Full,
        _ => ChargeState::Discharging,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::hidraw::fake_sysfs::FakeSysfs;

    fn reply(first: u8, error: u8, value: &[u8]) -> [u8; READ_BUF_LEN] {
        let mut report = [0u8; READ_BUF_LEN];
        report[0] = first;
        report[1] = GET;
        report[2] = error;
        report[3..3 + value.len()].copy_from_slice(value);
        report
    }

    // Requests

    #[test]
    fn requests_follow_the_documented_framing() {
        let mut cable = [0u8; REQUEST_LEN];
        cable[..4].copy_from_slice(&[0x00, 0x08, 0x02, 0x0F]);
        assert_eq!(request(Target::Cable, Property::BatteryLevel), cable);

        let mut subdevices = [0u8; REQUEST_LEN];
        subdevices[..4].copy_from_slice(&[0x00, 0x08, 0x02, 0x36]);
        assert_eq!(request(Target::Receiver, Property::Subdevices), subdevices);

        let mut paired = [0u8; REQUEST_LEN];
        paired[..4].copy_from_slice(&[0x00, 0x09, 0x02, 0x10]);
        assert_eq!(
            request(Target::Paired(PairedId(1)), Property::BatteryStatus),
            paired
        );
        assert_eq!(Target::Paired(PairedId(3)).address(), 0x0B);
    }

    // Replies

    #[test]
    fn a_reply_carries_a_little_endian_value() {
        assert_eq!(
            parse_reply(&reply(0x08, 0, &[0xAC, 0x03]), Target::Cable),
            Reply::Value(940)
        );
        assert_eq!(
            parse_reply(&reply(0x08, 0, &[0x01, 0x02, 0x03]), Target::Receiver),
            Reply::Value(0x03_02_01)
        );
    }

    #[test]
    fn a_non_zero_error_byte_is_an_error() {
        assert_eq!(
            parse_reply(
                &reply(0x09, 0x05, &[0xAC, 0x03]),
                Target::Paired(PairedId(1))
            ),
            Reply::Error(0x05)
        );
    }

    #[test]
    fn a_receiver_reply_for_another_device_is_unrelated() {
        let paired = Target::Paired(PairedId(2));
        assert_eq!(
            parse_reply(&reply(0x0A, 0, &[0x10]), paired),
            Reply::Value(0x10)
        );
        assert_eq!(
            parse_reply(&reply(0x09, 0, &[0x10]), paired),
            Reply::Unrelated
        );
        assert_eq!(
            parse_reply(&reply(0x09, 0, &[0x02]), Target::Receiver),
            Reply::Unrelated
        );
        assert_eq!(
            parse_reply(&reply(0x0B, 0, &[0x02]), Target::Cable),
            Reply::Value(0x02)
        );
    }

    #[test]
    fn other_reports_are_unrelated() {
        let mut notification = reply(0x08, 0, &[0x02]);
        notification[1] = 0x01;
        assert_eq!(
            parse_reply(&notification, Target::Receiver),
            Reply::Unrelated
        );
        assert_eq!(
            parse_reply(&reply(0x08, 0, &[0x02])[..5], Target::Receiver),
            Reply::Unrelated
        );
    }

    #[test]
    fn the_first_connected_paired_device_is_the_lowest_set_bit() {
        assert_eq!(first_connected(0x02), Some(PairedId(1)));
        assert_eq!(first_connected(0x0C), Some(PairedId(2)));
        assert_eq!(first_connected(0x08), Some(PairedId(3)));
        assert_eq!(first_connected(0x80), Some(PairedId(7)));
        assert_eq!(first_connected(0x00), None);
        assert_eq!(first_connected(0x01), None, "bit 0 is the receiver");
        assert_eq!(first_connected(0x100), None);
    }

    #[test]
    fn the_level_is_in_tenths_of_a_percent() {
        assert_eq!(level_percent(940), Ok(94));
        assert_eq!(level_percent(1000), Ok(100));
        assert_eq!(level_percent(5), Ok(0));
        assert!(level_percent(0).is_err());
        assert!(level_percent(1001).is_err());
    }

    #[test]
    fn status_maps_charging_and_full_only() {
        assert_eq!(charge_state(Some(1)), ChargeState::Charging);
        assert_eq!(charge_state(Some(2)), ChargeState::Discharging);
        assert_eq!(charge_state(Some(3)), ChargeState::Full);
        assert_eq!(charge_state(Some(0)), ChargeState::Discharging);
        assert_eq!(charge_state(None), ChargeState::Discharging);
    }

    // Device table

    #[test]
    fn every_receiver_is_a_row_and_every_row_is_on_interface_1() {
        assert_eq!(FAMILY.interface, Some(1));
        for product in RECEIVERS {
            assert!(
                FAMILY.models.iter().any(|m| m.product == product),
                "receiver 0x{product:04X} has no row"
            );
        }
        for model in FAMILY.models {
            assert_eq!(model.vendor, CORSAIR, "{}", model.name);
            assert!(
                matches!(model.kind, DeviceKind::Mouse | DeviceKind::Keyboard),
                "{}",
                model.name
            );
        }
    }

    // Discovery against a fake sysfs tree

    #[test]
    fn discover_in_finds_receivers_and_cables_on_interface_1() {
        let sysfs = FakeSysfs::new("corsair-rows");
        sysfs.add("hidraw0", 0, CORSAIR, 0x1BDC, "");
        sysfs.add("hidraw1", 1, CORSAIR, 0x1BDC, "");
        sysfs.add("hidraw2", 2, CORSAIR, 0x1BDC, "");
        sysfs.add("hidraw3", 1, CORSAIR, 0x1B4C, "");
        sysfs.add("hidraw4", 1, CORSAIR, 0x2B07, "");
        sysfs.add("hidraw5", 1, CORSAIR, 0x0A14, "");
        let devices = FAMILY.discover_in(&sysfs.class()).unwrap();
        let mut found: Vec<_> = devices
            .into_iter()
            .map(|d| {
                (
                    d.dev_path,
                    d.info.name,
                    d.info.kind,
                    link_of(d.identity.product),
                )
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            found,
            [
                (
                    PathBuf::from("/dev/hidraw1"),
                    "Corsair Slipstream Receiver".to_owned(),
                    DeviceKind::Mouse,
                    Link::Receiver,
                ),
                (
                    PathBuf::from("/dev/hidraw3"),
                    "Corsair Ironclaw RGB Wireless".to_owned(),
                    DeviceKind::Mouse,
                    Link::Cable,
                ),
                (
                    PathBuf::from("/dev/hidraw4"),
                    "Corsair K65 Plus Wireless".to_owned(),
                    DeviceKind::Keyboard,
                    Link::Receiver,
                ),
            ]
        );
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn target() -> impl Strategy<Value = Target> {
            prop_oneof![
                Just(Target::Cable),
                Just(Target::Receiver),
                (1u8..=7).prop_map(|id| Target::Paired(PairedId(id))),
            ]
        }

        proptest! {
            #[test]
            fn parsers_never_panic_on_any_length(
                report in prop::collection::vec(any::<u8>(), 0..70),
                target in target(),
            ) {
                if let Reply::Value(value) = parse_reply(&report, target) {
                    prop_assert!(value < 1 << 24);
                }
            }

            #[test]
            fn a_valid_level_is_a_percent(level in any::<u32>()) {
                if let Ok(percent) = level_percent(level) {
                    prop_assert!(percent <= 100);
                }
            }

            #[test]
            fn first_connected_is_a_set_bit_in_range(bitfield in any::<u32>()) {
                if let Some(PairedId(id)) = first_connected(bitfield) {
                    prop_assert!((1..=7).contains(&id));
                    prop_assert!(bitfield & (1 << id) != 0);
                    prop_assert_eq!(bitfield & ((1 << id) - 2), 0);
                }
            }

            #[test]
            fn every_paired_address_is_routed_back_to_it(id in 1u8..=7) {
                let target = Target::Paired(PairedId(id));
                let report = reply(target.address(), 0, &[0x2A]);
                prop_assert_eq!(parse_reply(&report, target), Reply::Value(0x2A));
            }
        }
    }
}
