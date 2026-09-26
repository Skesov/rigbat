//! Wireless headsets over `/dev/hidraw`: SteelSeries Arctis 1 and Arctis Nova 7,
//! Corsair Void, Logitech G533.
//!
//! Every family is request/response on one HID interface: write a query, read
//! input reports until the answer. A source holds its handle for its lifetime
//! and discards queued input before each query. Ids, requests, response
//! layouts and the source of each fact: `docs/headsets.md`.
//!
//! A node whose HID device already carries a kernel `power_supply` is skipped:
//! the sysfs backend reports that headset.

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
const READ_BUF_LEN: usize = 128;
/// Bounds the discard loop against a device that floods reports.
const MAX_QUEUED_REPORTS: usize = 256;

const HEADSET_OFF: &str =
    "headset is off or out of range (the dongle answered without battery data)";

// ── Device tables ────────────────────────────────────────────────────────────

const fn headset(product: u16, name: &'static str) -> HidrawModel {
    HidrawModel {
        product,
        name,
        kind: DeviceKind::Headset,
    }
}

pub static ARCTIS_1_FAMILY: HidrawFamily = HidrawFamily {
    vendor: 0x1038,
    models: &[
        headset(0x12B3, "SteelSeries Arctis 1 Wireless"),
        headset(0x12B6, "SteelSeries Arctis 1 Wireless Xbox"),
        headset(0x12D7, "SteelSeries Arctis 7X"),
        headset(0x12D5, "SteelSeries Arctis 7P"),
    ],
    interface: Some(3),
};

/// A firmware update moves a headset from a steps id to a percent id; both keep
/// one name so the device keeps its identity.
pub static ARCTIS_NOVA_FAMILY: HidrawFamily = HidrawFamily {
    vendor: 0x1038,
    models: &[
        headset(0x2202, "SteelSeries Arctis Nova 7"),
        headset(0x22A1, "SteelSeries Arctis Nova 7"),
        headset(0x227E, "SteelSeries Arctis Nova 7 Gen 2"),
        headset(0x2206, "SteelSeries Arctis Nova 7X"),
        headset(0x22A4, "SteelSeries Arctis Nova 7X"),
        headset(0x22A5, "SteelSeries Arctis Nova 7X"),
        headset(0x2258, "SteelSeries Arctis Nova 7X Gen 2"),
        headset(0x229E, "SteelSeries Arctis Nova 7X Gen 2"),
        headset(0x22AD, "SteelSeries Arctis Nova 7X Gen 2"),
        headset(0x223A, "SteelSeries Arctis Nova 7 Diablo IV"),
        headset(0x22A9, "SteelSeries Arctis Nova 7 Diablo IV"),
        headset(0x227A, "SteelSeries Arctis Nova 7 WoW Edition"),
        headset(0x220A, "SteelSeries Arctis Nova 7P"),
        headset(0x22A7, "SteelSeries Arctis Nova 7P"),
        headset(0x2298, "SteelSeries Arctis Nova 7P"),
    ],
    interface: Some(3),
};

/// Products of [`ARCTIS_NOVA_FAMILY`] that report the level in steps of 25%.
const NOVA_STEP_PRODUCTS: &[u16] = &[0x2202, 0x2206, 0x220A, 0x223A, 0x227A, 0x22A4];

pub static CORSAIR_VOID_FAMILY: HidrawFamily = HidrawFamily {
    vendor: 0x1B1C,
    models: &[
        headset(0x0A0C, "Corsair Void Wireless"),
        headset(0x0A2B, "Corsair Void Wireless"),
        headset(0x1B23, "Corsair Void Wireless"),
        headset(0x1B25, "Corsair Void Wireless"),
        headset(0x1B27, "Corsair Void Wireless"),
        headset(0x0A14, "Corsair Void Pro Wireless"),
        headset(0x0A16, "Corsair Void Pro Wireless"),
        headset(0x0A1A, "Corsair Void Pro Wireless"),
        headset(0x0A51, "Corsair Void Elite Wireless"),
        headset(0x0A55, "Corsair Void Elite Wireless"),
        headset(0x0A75, "Corsair Void Elite Wireless"),
    ],
    interface: Some(3),
};

pub static LOGITECH_G533_FAMILY: HidrawFamily = HidrawFamily {
    vendor: 0x046D,
    models: &[headset(0x0A66, "Logitech G533 Wireless")],
    interface: Some(3),
};

// ── Backend ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Arctis1,
    ArctisNova,
    CorsairVoid,
    HidppAdc,
}

/// One headset family: its device table and the exchange its devices speak.
#[derive(Clone, Copy)]
pub struct HeadsetBackend {
    name: &'static str,
    family: &'static HidrawFamily,
    protocol: Protocol,
}

pub const ARCTIS_1: HeadsetBackend = HeadsetBackend {
    name: "steelseries-arctis",
    family: &ARCTIS_1_FAMILY,
    protocol: Protocol::Arctis1,
};

pub const ARCTIS_NOVA: HeadsetBackend = HeadsetBackend {
    name: "steelseries-arctis-nova",
    family: &ARCTIS_NOVA_FAMILY,
    protocol: Protocol::ArctisNova,
};

pub const CORSAIR_VOID: HeadsetBackend = HeadsetBackend {
    name: "corsair-void",
    family: &CORSAIR_VOID_FAMILY,
    protocol: Protocol::CorsairVoid,
};

pub const LOGITECH_G533: HeadsetBackend = HeadsetBackend {
    name: "logitech-g533",
    family: &LOGITECH_G533_FAMILY,
    protocol: Protocol::HidppAdc,
};

#[async_trait::async_trait]
impl BatteryBackend for HeadsetBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn hidraw_family(&self) -> Option<&'static HidrawFamily> {
        Some(self.family)
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        let family = self.family;
        let devices = tokio::task::spawn_blocking(move || {
            discover_in(family, Path::new(hidraw::SYSFS_HIDRAW))
        })
        .await
        .context("spawn_blocking")??;
        let protocol = self.protocol;
        Ok(devices
            .into_iter()
            .map(|device| -> Box<dyn BatterySource> {
                Box::new(HeadsetSource {
                    info: device.info,
                    dev_path: device.dev_path,
                    identity: device.identity,
                    protocol,
                    link: None,
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
                "a kernel driver reports this headset's battery; left to the sysfs backend"
            );
        }
        !kernel
    });
    Ok(devices)
}

// ── Source ───────────────────────────────────────────────────────────────────

struct HeadsetSource {
    info: DeviceInfo,
    dev_path: PathBuf,
    identity: hidraw::NodeIdentity,
    protocol: Protocol,
    /// Kept across polls; dropped on any error so the next poll reopens by node name.
    link: Option<Link>,
}

struct Link {
    file: File,
    /// The HID++ feature index of ADC Measurement, valid for this handle only.
    adc_index: Option<u8>,
}

#[async_trait::async_trait]
impl BatterySource for HeadsetSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let dev_path = self.dev_path.clone();
        let identity = self.identity.clone();
        let protocol = self.protocol;
        let link = self.link.take();

        let (result, link) = tokio::task::spawn_blocking(move || {
            let mut link = link;
            let result = poll_device(&dev_path, &identity, protocol, &mut link);
            if result.is_err() {
                link = None;
            }
            (result, link)
        })
        .await
        .context("spawn_blocking")?;

        self.link = link;
        if let Ok(reading) = &result {
            tracing::debug!(device = %self.info.name, percent = reading.percent, "headset poll");
        }
        result
    }
}

fn poll_device(
    dev_path: &Path,
    identity: &hidraw::NodeIdentity,
    protocol: Protocol,
    link: &mut Option<Link>,
) -> anyhow::Result<BatteryReading> {
    if link.is_none() {
        let file = hidraw::open_verified(
            Path::new(hidraw::SYSFS_HIDRAW),
            dev_path,
            identity,
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK),
        )?;
        *link = Some(Link {
            file,
            adc_index: None,
        });
    }
    let link = link
        .as_mut()
        .context("handle unexpectedly empty after open")?;

    match protocol {
        Protocol::Arctis1 => exchange(&mut link.file, &ARCTIS_1_REQUEST, classify_arctis_1),
        Protocol::ArctisNova => {
            let scale = nova_scale(identity.product);
            exchange(&mut link.file, &NOVA_REQUEST, |report| {
                classify_nova(report, scale)
            })
        }
        Protocol::CorsairVoid => exchange(&mut link.file, &VOID_REQUEST, classify_void),
        Protocol::HidppAdc => {
            let index = match link.adc_index {
                Some(index) => index,
                None => {
                    let index = exchange(
                        &mut link.file,
                        &hidpp_request(HIDPP_ROOT_INDEX, ADC_MEASUREMENT.to_be_bytes()),
                        classify_root,
                    )?
                    .context("headset has no ADC Measurement feature (0x1F20)")?;
                    link.adc_index = Some(index);
                    index
                }
            };
            exchange(&mut link.file, &hidpp_request(index, [0, 0]), |report| {
                classify_adc(report, index)
            })
        }
    }
}

/// Writes `request` and reads until `classify` recognises the answer.
fn exchange<T>(
    file: &mut File,
    request: &[u8],
    classify: impl Fn(&[u8]) -> Reply<T>,
) -> anyhow::Result<T> {
    discard_queued(file)?;
    file.write_all(request).context("writing request")?;

    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut buf = [0u8; READ_BUF_LEN];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let remaining_ms = u16::try_from(remaining.as_millis()).unwrap_or(u16::MAX);
        if remaining_ms == 0 {
            anyhow::bail!("timeout waiting for the headset's answer");
        }
        let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, PollTimeout::from(remaining_ms)).context("poll()")? == 0 {
            anyhow::bail!("timeout waiting for the headset's answer");
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
            Reply::HeadsetOff => anyhow::bail!(HEADSET_OFF),
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

// ── Protocols (pure) ─────────────────────────────────────────────────────────

/// What one input report means to the exchange waiting on it.
#[derive(Debug, PartialEq, Eq)]
enum Reply<T> {
    Answer(T),
    /// The dongle answered, the headset behind it did not.
    HeadsetOff,
    /// Another report on the same interface. Keep reading.
    Unrelated,
}

const ARCTIS_1_REQUEST: [u8; 2] = [0x06, 0x12];
const ARCTIS_1_NOT_LINKED: u8 = 0x01;

fn classify_arctis_1(report: &[u8]) -> Reply<BatteryReading> {
    match *report {
        [0x06, 0x12, link, level, ..] => {
            if link == ARCTIS_1_NOT_LINKED {
                Reply::HeadsetOff
            } else {
                Reply::Answer(BatteryReading::new(level, ChargeState::Discharging))
            }
        }
        _ => Reply::Unrelated,
    }
}

/// Report ID 0, then the status query.
const NOVA_REQUEST: [u8; 2] = [0x00, 0xB0];
const NOVA_LINKED: u8 = 0x03;
const NOVA_POWER_OFF: u8 = 0x00;
const NOVA_STEP_PERCENT: u8 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NovaScale {
    Steps,
    Percent,
}

fn nova_scale(product: u16) -> NovaScale {
    if NOVA_STEP_PRODUCTS.contains(&product) {
        NovaScale::Steps
    } else {
        NovaScale::Percent
    }
}

fn classify_nova(report: &[u8], scale: NovaScale) -> Reply<BatteryReading> {
    let [0xB0, link, level, power, ..] = *report else {
        return Reply::Unrelated;
    };
    if link != NOVA_LINKED || power == NOVA_POWER_OFF {
        return Reply::HeadsetOff;
    }
    let state = match power {
        0x01 | 0x02 => ChargeState::Charging,
        _ => ChargeState::Discharging,
    };
    Reply::Answer(match scale {
        NovaScale::Steps => {
            BatteryReading::new_coarse(level.min(4).saturating_mul(NOVA_STEP_PERCENT), state)
        }
        NovaScale::Percent => BatteryReading::new(level, state),
    })
}

/// Output report `0xC9` asking for input report `0x64`.
const VOID_REQUEST: [u8; 2] = [0xC9, 0x64];
const VOID_LINKED: u8 = 177;
const VOID_MIC_BIT: u8 = 0x80;

fn classify_void(report: &[u8]) -> Reply<BatteryReading> {
    let [0x64, _, level, link, power, ..] = *report else {
        return Reply::Unrelated;
    };
    if link != VOID_LINKED {
        return Reply::HeadsetOff;
    }
    let percent = level & !VOID_MIC_BIT;
    match power {
        1..=3 => Reply::Answer(BatteryReading::new(percent, ChargeState::Discharging)),
        4 => Reply::Answer(BatteryReading::new(percent, ChargeState::Full)),
        // The level reads high while charging.
        5 => Reply::Answer(BatteryReading::new_coarse(percent, ChargeState::Charging)),
        _ => Reply::HeadsetOff,
    }
}

const HIDPP_SHORT: u8 = 0x10;
const HIDPP_LONG: u8 = 0x11;
const HIDPP_LONG_LEN: usize = 20;
const HIDPP_DEVICE: u8 = 0xFF;
const HIDPP_ERROR: u8 = 0xFF;
const HIDPP_ROOT_INDEX: u8 = 0x00;
/// Function 0 in the high nibble, rigbat's software id in the low one.
const HIDPP_FUNCTION: u8 = 0x0B;
const ADC_MEASUREMENT: u16 = 0x1F20;
const ADC_CHARGING: u8 = 0x03;
const ADC_FULL: u8 = 0x07;
const MIN_VOLTAGE_MV: u16 = 3000;

/// rigbat's own approximation of a single-cell Li-ion discharge curve under
/// light load, `(millivolts, percent)`, highest first.
const VOLTAGE_CURVE: [(u16, u8); 12] = [
    (4150, 100),
    (4050, 90),
    (3970, 80),
    (3900, 70),
    (3850, 60),
    (3810, 50),
    (3780, 40),
    (3750, 30),
    (3710, 20),
    (3670, 10),
    (3600, 5),
    (3450, 0),
];

fn hidpp_request(feature_index: u8, params: [u8; 2]) -> [u8; HIDPP_LONG_LEN] {
    let mut request = [0u8; HIDPP_LONG_LEN];
    request[0] = HIDPP_LONG;
    request[1] = HIDPP_DEVICE;
    request[2] = feature_index;
    request[3] = HIDPP_FUNCTION;
    request[4] = params[0];
    request[5] = params[1];
    request
}

/// The root feature's answer: the feature's index, `None` when the headset lacks it.
fn classify_root(report: &[u8]) -> Reply<Option<u8>> {
    match *report {
        [
            HIDPP_SHORT | HIDPP_LONG,
            HIDPP_DEVICE,
            HIDPP_ROOT_INDEX,
            HIDPP_FUNCTION,
            index,
            ..,
        ] => Reply::Answer((index != HIDPP_ROOT_INDEX && index != HIDPP_ERROR).then_some(index)),
        [
            HIDPP_SHORT | HIDPP_LONG,
            HIDPP_DEVICE,
            HIDPP_ERROR,
            HIDPP_ROOT_INDEX,
            HIDPP_FUNCTION,
            ..,
        ] => Reply::HeadsetOff,
        _ => Reply::Unrelated,
    }
}

fn classify_adc(report: &[u8], index: u8) -> Reply<BatteryReading> {
    match *report {
        [
            HIDPP_SHORT | HIDPP_LONG,
            HIDPP_DEVICE,
            HIDPP_ERROR,
            feature,
            HIDPP_FUNCTION,
            ..,
        ] if feature == index => Reply::HeadsetOff,
        [
            HIDPP_SHORT | HIDPP_LONG,
            HIDPP_DEVICE,
            feature,
            HIDPP_FUNCTION,
            high,
            low,
            status,
            ..,
        ] if feature == index => {
            let millivolts = u16::from_be_bytes([high, low]);
            if millivolts < MIN_VOLTAGE_MV {
                return Reply::HeadsetOff;
            }
            let state = match status {
                ADC_CHARGING => ChargeState::Charging,
                ADC_FULL => ChargeState::Full,
                _ => ChargeState::Discharging,
            };
            Reply::Answer(BatteryReading::new_coarse(
                voltage_to_percent(millivolts),
                state,
            ))
        }
        _ => Reply::Unrelated,
    }
}

/// Linear between the points of [`VOLTAGE_CURVE`], clamped at both ends.
fn voltage_to_percent(millivolts: u16) -> u8 {
    for pair in VOLTAGE_CURVE.windows(2) {
        let [(high_mv, high), (low_mv, low)] = *pair else {
            continue;
        };
        if millivolts >= high_mv {
            return high;
        }
        if millivolts >= low_mv {
            let gained = u32::from(high - low) * u32::from(millivolts - low_mv)
                / u32::from(high_mv - low_mv);
            return low + u8::try_from(gained).unwrap_or(high - low);
        }
    }
    0
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::hidraw::fake_sysfs::FakeSysfs;

    const ALL: [HeadsetBackend; 4] = [ARCTIS_1, ARCTIS_NOVA, CORSAIR_VOID, LOGITECH_G533];

    fn discharging(percent: u8) -> Reply<BatteryReading> {
        Reply::Answer(BatteryReading::new(percent, ChargeState::Discharging))
    }

    // Device tables

    #[test]
    fn every_model_is_a_headset() {
        for backend in ALL {
            for model in backend.family.models {
                assert_eq!(model.kind, DeviceKind::Headset, "{}", model.name);
            }
        }
    }

    #[test]
    fn backend_names_are_unique() {
        let names: std::collections::BTreeSet<_> = ALL.iter().map(|b| b.name).collect();
        assert_eq!(names.len(), ALL.len());
    }

    #[test]
    fn every_step_product_is_a_nova_model() {
        for product in NOVA_STEP_PRODUCTS {
            assert!(
                ARCTIS_NOVA_FAMILY
                    .models
                    .iter()
                    .any(|m| m.product == *product),
                "{product:04x}"
            );
        }
    }

    #[test]
    fn nova_scale_follows_the_product() {
        assert_eq!(nova_scale(0x2202), NovaScale::Steps);
        assert_eq!(nova_scale(0x22A1), NovaScale::Percent);
    }

    // Discovery against a fake sysfs tree

    #[test]
    fn discover_in_finds_each_family_on_its_battery_interface() {
        let sysfs = FakeSysfs::new("headsets-families");
        for (i, backend) in ALL.iter().enumerate() {
            let product = backend.family.models[0].product;
            sysfs.add(
                &format!("hidraw{}", 2 * i),
                3,
                backend.family.vendor,
                product,
                "",
            );
            sysfs.add(
                &format!("hidraw{}", 2 * i + 1),
                0,
                backend.family.vendor,
                product,
                "",
            );
        }
        for (i, backend) in ALL.iter().enumerate() {
            let found = discover_in(backend.family, &sysfs.class()).unwrap();
            assert_eq!(found.len(), 1, "{}", backend.name);
            assert_eq!(
                found[0].dev_path,
                PathBuf::from(format!("/dev/hidraw{}", 2 * i))
            );
            assert_eq!(found[0].info.name, backend.family.models[0].name);
            assert_eq!(found[0].info.kind, DeviceKind::Headset);
        }
    }

    #[test]
    fn discover_in_leaves_a_kernel_reported_headset_to_sysfs() {
        let sysfs = FakeSysfs::new("headsets-kernel");
        sysfs.add("hidraw1", 3, 0x1038, 0x12B6, "");
        sysfs.add("hidraw2", 3, 0x1038, 0x12B3, "");
        sysfs.add_power_supply("hidraw1", "steelseries_headset_battery_hidraw1");
        let found = discover_in(&ARCTIS_1_FAMILY, &sysfs.class()).unwrap();
        let names: Vec<_> = found.iter().map(|d| d.info.name.as_str()).collect();
        assert_eq!(names, ["SteelSeries Arctis 1 Wireless"]);
    }

    #[test]
    fn an_empty_power_supply_directory_is_not_a_kernel_battery() {
        let sysfs = FakeSysfs::new("headsets-empty-supply");
        sysfs.add("hidraw1", 3, 0x1B1C, 0x0A14, "");
        std::fs::create_dir_all(sysfs.class().join("hidraw1/device/power_supply")).unwrap();
        assert_eq!(
            discover_in(&CORSAIR_VOID_FAMILY, &sysfs.class())
                .unwrap()
                .len(),
            1
        );
    }

    // Arctis 1

    #[test]
    fn arctis_1_reads_the_level() {
        assert_eq!(
            classify_arctis_1(&[0x06, 0x12, 0x03, 0x4B, 0, 0, 0, 0]),
            discharging(75)
        );
    }

    #[test]
    fn arctis_1_headset_off() {
        assert_eq!(
            classify_arctis_1(&[0x06, 0x12, 0x01, 0x00, 0, 0, 0, 0]),
            Reply::HeadsetOff
        );
    }

    #[test]
    fn arctis_1_ignores_other_reports() {
        assert_eq!(
            classify_arctis_1(&[0x06, 0x35, 0x03, 0x4B]),
            Reply::Unrelated
        );
        assert_eq!(classify_arctis_1(&[0x06, 0x12, 0x03]), Reply::Unrelated);
    }

    // Arctis Nova 7

    #[test]
    fn nova_reads_steps_as_coarse_quarters() {
        // The capture in docs/headsets.md (aarol): b0 03 04 03.
        assert_eq!(
            classify_nova(&[0xB0, 0x03, 0x04, 0x03], NovaScale::Steps),
            Reply::Answer(BatteryReading::new_coarse(100, ChargeState::Discharging))
        );
        assert_eq!(
            classify_nova(&[0xB0, 0x03, 0x02, 0x01], NovaScale::Steps),
            Reply::Answer(BatteryReading::new_coarse(50, ChargeState::Charging))
        );
    }

    #[test]
    fn nova_reads_a_percent() {
        assert_eq!(
            classify_nova(&[0xB0, 0x03, 0x3F, 0x03], NovaScale::Percent),
            discharging(63)
        );
        assert_eq!(
            classify_nova(&[0xB0, 0x03, 0x3F, 0x02], NovaScale::Percent),
            Reply::Answer(BatteryReading::new(63, ChargeState::Charging))
        );
    }

    #[test]
    fn nova_headset_off() {
        assert_eq!(
            classify_nova(&[0xB0, 0x03, 0x04, 0x00], NovaScale::Steps),
            Reply::HeadsetOff
        );
        assert_eq!(
            classify_nova(&[0xB0, 0x02, 0x04, 0x03], NovaScale::Steps),
            Reply::HeadsetOff
        );
    }

    #[test]
    fn nova_ignores_other_reports() {
        assert_eq!(
            classify_nova(&[0xB7, 0x03, 0x04, 0x03], NovaScale::Steps),
            Reply::Unrelated
        );
        assert_eq!(
            classify_nova(&[0xB0, 0x03, 0x04], NovaScale::Steps),
            Reply::Unrelated
        );
    }

    // Corsair Void

    #[test]
    fn void_masks_the_mic_bit() {
        assert_eq!(
            classify_void(&[0x64, 0x00, 0x80 | 55, 177, 1]),
            discharging(55)
        );
    }

    #[test]
    fn void_reads_charging_as_coarse_and_full() {
        assert_eq!(
            classify_void(&[0x64, 0x00, 90, 177, 5]),
            Reply::Answer(BatteryReading::new_coarse(90, ChargeState::Charging))
        );
        assert_eq!(
            classify_void(&[0x64, 0x00, 100, 177, 4]),
            Reply::Answer(BatteryReading::new(100, ChargeState::Full))
        );
    }

    #[test]
    fn void_headset_off() {
        assert_eq!(classify_void(&[0x64, 0x00, 0, 51, 0]), Reply::HeadsetOff);
        assert_eq!(classify_void(&[0x64, 0x00, 40, 177, 0]), Reply::HeadsetOff);
    }

    #[test]
    fn void_ignores_other_reports() {
        assert_eq!(classify_void(&[0x66, 0x00, 40, 177, 1]), Reply::Unrelated);
        assert_eq!(classify_void(&[0x64, 0x00, 40, 177]), Reply::Unrelated);
    }

    // Logitech HID++

    #[test]
    fn hidpp_request_is_a_long_report_to_the_headset() {
        let request = hidpp_request(HIDPP_ROOT_INDEX, ADC_MEASUREMENT.to_be_bytes());
        assert_eq!(request.len(), 20);
        assert_eq!(request[..6], [0x11, 0xFF, 0x00, 0x0B, 0x1F, 0x20]);
        assert!(request[6..].iter().all(|&b| b == 0));
    }

    #[test]
    fn root_answers_the_feature_index() {
        let mut reply = [0u8; 20];
        reply[..5].copy_from_slice(&[0x11, 0xFF, 0x00, 0x0B, 0x07]);
        assert_eq!(classify_root(&reply), Reply::Answer(Some(0x07)));
        reply[4] = 0;
        assert_eq!(classify_root(&reply), Reply::Answer(None));
    }

    #[test]
    fn root_error_means_headset_off() {
        assert_eq!(
            classify_root(&[0x11, 0xFF, 0xFF, 0x00, 0x0B, 0x05, 0]),
            Reply::HeadsetOff
        );
    }

    #[test]
    fn root_ignores_another_software_id() {
        assert_eq!(
            classify_root(&[0x11, 0xFF, 0x00, 0x01, 0x07, 0, 0]),
            Reply::Unrelated
        );
    }

    #[test]
    fn adc_reads_voltage_and_status() {
        // 3810 mV = 0x0EE2.
        assert_eq!(
            classify_adc(&[0x11, 0xFF, 0x07, 0x0B, 0x0E, 0xE2, 0x01], 0x07),
            Reply::Answer(BatteryReading::new_coarse(50, ChargeState::Discharging))
        );
        assert_eq!(
            classify_adc(&[0x11, 0xFF, 0x07, 0x0B, 0x0E, 0xE2, 0x03], 0x07),
            Reply::Answer(BatteryReading::new_coarse(50, ChargeState::Charging))
        );
        assert_eq!(
            classify_adc(&[0x11, 0xFF, 0x07, 0x0B, 0x10, 0x68, 0x07], 0x07),
            Reply::Answer(BatteryReading::new_coarse(100, ChargeState::Full))
        );
    }

    #[test]
    fn adc_error_or_no_voltage_means_headset_off() {
        assert_eq!(
            classify_adc(&[0x11, 0xFF, 0xFF, 0x07, 0x0B, 0x05, 0], 0x07),
            Reply::HeadsetOff
        );
        assert_eq!(
            classify_adc(&[0x11, 0xFF, 0x07, 0x0B, 0x00, 0x00, 0x01], 0x07),
            Reply::HeadsetOff
        );
    }

    #[test]
    fn adc_ignores_another_feature() {
        assert_eq!(
            classify_adc(&[0x11, 0xFF, 0x08, 0x0B, 0x0E, 0xE2, 0x01], 0x07),
            Reply::Unrelated
        );
        assert_eq!(
            classify_adc(&[0x11, 0xFF, 0xFF, 0x08, 0x0B, 0x05, 0], 0x07),
            Reply::Unrelated
        );
    }

    #[test]
    fn voltage_to_percent_follows_the_curve() {
        assert_eq!(voltage_to_percent(4300), 100);
        assert_eq!(voltage_to_percent(4150), 100);
        assert_eq!(voltage_to_percent(3830), 55);
        assert_eq!(voltage_to_percent(3810), 50);
        assert_eq!(voltage_to_percent(3450), 0);
        assert_eq!(voltage_to_percent(3000), 0);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn in_range(reply: &Reply<BatteryReading>) -> bool {
            match reply {
                Reply::Answer(reading) => reading.percent <= 100,
                _ => true,
            }
        }

        proptest! {
            #[test]
            fn classifiers_never_panic_and_stay_in_range(
                report in prop::collection::vec(any::<u8>(), 0..70),
                index in any::<u8>(),
            ) {
                prop_assert!(in_range(&classify_arctis_1(&report)));
                prop_assert!(in_range(&classify_nova(&report, NovaScale::Steps)));
                prop_assert!(in_range(&classify_nova(&report, NovaScale::Percent)));
                prop_assert!(in_range(&classify_void(&report)));
                prop_assert!(in_range(&classify_adc(&report, index)));
                let _ = classify_root(&report);
            }

            #[test]
            fn nova_steps_are_coarse_and_percent_is_not(
                level in any::<u8>(),
                power in 1u8..=3,
                tail in prop::collection::vec(any::<u8>(), 0..60),
            ) {
                let mut report = vec![0xB0, NOVA_LINKED, level, power];
                report.extend(tail);
                let Reply::Answer(steps) = classify_nova(&report, NovaScale::Steps) else {
                    return Err(TestCaseError::fail("steps: no answer"));
                };
                let Reply::Answer(percent) = classify_nova(&report, NovaScale::Percent) else {
                    return Err(TestCaseError::fail("percent: no answer"));
                };
                prop_assert!(steps.coarse);
                prop_assert_eq!(steps.percent % NOVA_STEP_PERCENT, 0);
                prop_assert!(!percent.coarse);
                prop_assert_eq!(percent.percent, level.min(100));
            }

            #[test]
            fn void_percent_ignores_the_mic_bit(level in 0u8..=100, power in 1u8..=4) {
                let down = classify_void(&[0x64, 0, level, VOID_LINKED, power]);
                let up = classify_void(&[0x64, 0, level | VOID_MIC_BIT, VOID_LINKED, power]);
                prop_assert_eq!(down, up);
            }

            #[test]
            fn voltage_to_percent_is_monotonic(a in any::<u16>(), b in any::<u16>()) {
                let (low, high) = if a <= b { (a, b) } else { (b, a) };
                prop_assert!(voltage_to_percent(low) <= voltage_to_percent(high));
                prop_assert!(voltage_to_percent(high) <= 100);
            }

            #[test]
            fn adc_answers_only_its_own_feature(
                index in 1u8..0xFF,
                other in 1u8..0xFF,
                millivolts in MIN_VOLTAGE_MV..=u16::MAX,
                status in any::<u8>(),
            ) {
                let [high, low] = millivolts.to_be_bytes();
                let report = [HIDPP_LONG, HIDPP_DEVICE, other, HIDPP_FUNCTION, high, low, status];
                let reply = classify_adc(&report, index);
                prop_assert_eq!(matches!(reply, Reply::Answer(_)), other == index);
            }
        }
    }
}
