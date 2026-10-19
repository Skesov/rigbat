//! Razer wireless mice over `/dev/hidraw`, on the cable or through a receiver.
//!
//! Request/response on USB interface 0: send a 90-byte feature report, wait,
//! read the reply as a feature report. The source holds its handle for its
//! lifetime. Where OpenRazer's `razermouse` driver owns the mouse, the source
//! reads that driver's `charge_level` / `charge_status` attributes instead of
//! writing to the mouse itself. Ids, layout, delays and the source of each
//! fact: `docs/razer.md`.

use std::{
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context as _;

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

use super::{
    AccessDenied, BatteryBackend, BatterySource, Context,
    hid_feature::{get_feature, set_feature},
    hidraw::{self, HidrawFamily, HidrawModel},
};

const MOUSE_OFF: &str = "mouse is asleep or off (receiver reported a timeout)";

const OPENRAZER_DRIVER: &str = "razermouse";
const CHARGE_LEVEL: &str = "charge_level";
const CHARGE_STATUS: &str = "charge_status";

/// Reads after the first one while the mouse answers busy.
const BUSY_REREADS: usize = 3;

// ── Device table ─────────────────────────────────────────────────────────────

const RAZER: u16 = 0x1532;

const WAIT_MOUSE: Duration = Duration::from_micros(600);
const WAIT_RECEIVER: Duration = Duration::from_micros(31_000);
const WAIT_VIPER: Duration = Duration::from_micros(59_900);
const WAIT_ATHERIS: Duration = Duration::from_micros(400_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Power {
    Rechargeable,
    /// No charging circuit: OpenRazer answers "not charging" without asking.
    AaBattery,
}

#[derive(Debug)]
struct Model {
    product: u16,
    name: &'static str,
    transaction: u8,
    /// Between sending the request and reading the reply.
    wait: Duration,
    power: Power,
}

const fn rechargeable(product: u16, name: &'static str, transaction: u8, wait: Duration) -> Model {
    Model {
        product,
        name,
        transaction,
        wait,
        power: Power::Rechargeable,
    }
}

const fn aa(product: u16, name: &'static str, transaction: u8, wait: Duration) -> Model {
    Model {
        product,
        name,
        transaction,
        wait,
        power: Power::AaBattery,
    }
}

/// Cable and receiver of one mouse share a name; the locator tells them apart.
const MODELS: &[Model] = &[
    rechargeable(0x001F, "Razer Naga Epic", 0xFF, WAIT_MOUSE),
    rechargeable(0x0024, "Razer Mamba 2012", 0xFF, WAIT_MOUSE),
    rechargeable(0x0025, "Razer Mamba 2012", 0xFF, WAIT_MOUSE),
    rechargeable(0x0032, "Razer Ouroboros", 0xFF, WAIT_MOUSE),
    rechargeable(0x003E, "Razer Naga Epic Chroma", 0xFF, WAIT_MOUSE),
    rechargeable(0x003F, "Razer Naga Epic Chroma", 0xFF, WAIT_MOUSE),
    rechargeable(0x0044, "Razer Mamba", 0xFF, WAIT_MOUSE),
    rechargeable(0x0045, "Razer Mamba", 0xFF, WAIT_MOUSE),
    rechargeable(0x0059, "Razer Lancehead", 0x3F, WAIT_MOUSE),
    rechargeable(0x005A, "Razer Lancehead", 0x3F, WAIT_MOUSE),
    aa(0x0062, "Razer Atheris", 0x1F, WAIT_ATHERIS),
    rechargeable(0x006F, "Razer Lancehead Wireless", 0x1F, WAIT_RECEIVER),
    rechargeable(0x0070, "Razer Lancehead Wireless", 0x1F, WAIT_RECEIVER),
    rechargeable(0x0072, "Razer Mamba Wireless", 0x3F, WAIT_RECEIVER),
    rechargeable(0x0073, "Razer Mamba Wireless", 0x3F, WAIT_RECEIVER),
    rechargeable(0x0077, "Razer Pro Click", 0x1F, WAIT_RECEIVER),
    rechargeable(0x007A, "Razer Viper Ultimate", 0xFF, WAIT_VIPER),
    rechargeable(0x007B, "Razer Viper Ultimate", 0xFF, WAIT_VIPER),
    rechargeable(0x007C, "Razer DeathAdder V2 Pro", 0x3F, WAIT_VIPER),
    rechargeable(0x007D, "Razer DeathAdder V2 Pro", 0x3F, WAIT_VIPER),
    rechargeable(0x0080, "Razer Pro Click", 0x1F, WAIT_RECEIVER),
    aa(0x0083, "Razer Basilisk X HyperSpeed", 0xFF, WAIT_RECEIVER),
    rechargeable(0x0086, "Razer Basilisk Ultimate", 0x1F, WAIT_RECEIVER),
    rechargeable(0x0088, "Razer Basilisk Ultimate", 0x1F, WAIT_RECEIVER),
    rechargeable(0x008F, "Razer Naga Pro", 0x1F, WAIT_MOUSE),
    rechargeable(0x0090, "Razer Naga Pro", 0x1F, WAIT_RECEIVER),
    aa(0x0094, "Razer Orochi V2", 0x1F, WAIT_ATHERIS),
    rechargeable(0x009A, "Razer Pro Click Mini", 0x1F, WAIT_RECEIVER),
    aa(
        0x009C,
        "Razer DeathAdder V2 X HyperSpeed",
        0x1F,
        WAIT_RECEIVER,
    ),
    rechargeable(0x009E, "Razer Viper Mini SE", 0x1F, WAIT_VIPER),
    rechargeable(0x009F, "Razer Viper Mini SE", 0x1F, WAIT_VIPER),
    rechargeable(0x00A5, "Razer Viper V2 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00A6, "Razer Viper V2 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00A7, "Razer Naga V2 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00A8, "Razer Naga V2 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00AA, "Razer Basilisk V3 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00AB, "Razer Basilisk V3 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00AF, "Razer Cobra Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00B0, "Razer Cobra Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(
        0x00B3,
        "Razer HyperPolling Wireless Dongle",
        0x1F,
        WAIT_VIPER,
    ),
    aa(0x00B4, "Razer Naga V2 HyperSpeed", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00B6, "Razer DeathAdder V3 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00B7, "Razer DeathAdder V3 Pro", 0x1F, WAIT_RECEIVER),
    aa(0x00B8, "Razer Viper V3 HyperSpeed", 0x1F, WAIT_VIPER),
    aa(
        0x00B9,
        "Razer Basilisk V3 X HyperSpeed",
        0x1F,
        WAIT_RECEIVER,
    ),
    rechargeable(0x00BE, "Razer DeathAdder V4 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00BF, "Razer DeathAdder V4 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00C0, "Razer Viper V3 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00C1, "Razer Viper V3 Pro", 0x1F, WAIT_VIPER),
    rechargeable(0x00C2, "Razer DeathAdder V3 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00C3, "Razer DeathAdder V3 Pro", 0x1F, WAIT_RECEIVER),
    rechargeable(
        0x00C4,
        "Razer DeathAdder V3 HyperSpeed",
        0x1F,
        WAIT_RECEIVER,
    ),
    rechargeable(
        0x00C5,
        "Razer DeathAdder V3 HyperSpeed",
        0x1F,
        WAIT_RECEIVER,
    ),
    rechargeable(
        0x00C7,
        "Razer Pro Click V2 Vertical Edition",
        0x1F,
        WAIT_RECEIVER,
    ),
    rechargeable(
        0x00C8,
        "Razer Pro Click V2 Vertical Edition",
        0x1F,
        WAIT_RECEIVER,
    ),
    rechargeable(0x00CC, "Razer Basilisk V3 Pro 35K", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00CD, "Razer Basilisk V3 Pro 35K", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00D0, "Razer Pro Click V2", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00D1, "Razer Pro Click V2", 0x1F, WAIT_RECEIVER),
    aa(0x00D3, "Razer Basilisk Mobile", 0x1F, WAIT_RECEIVER),
    aa(0x00D4, "Razer Basilisk Mobile", 0x1F, WAIT_RECEIVER),
    rechargeable(
        0x00D6,
        "Razer Basilisk V3 Pro 35K Phantom Green Edition",
        0x1F,
        WAIT_RECEIVER,
    ),
    rechargeable(
        0x00D7,
        "Razer Basilisk V3 Pro 35K Phantom Green Edition",
        0x1F,
        WAIT_RECEIVER,
    ),
    rechargeable(0x00DA, "Razer Cobra HyperSpeed", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00DB, "Razer Cobra HyperSpeed", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00DE, "Razer Viper V3 Pro SE", 0x1F, WAIT_RECEIVER),
    rechargeable(0x00DF, "Razer Viper V3 Pro SE", 0x1F, WAIT_VIPER),
];

const PLACEHOLDER: HidrawModel = HidrawModel {
    vendor: RAZER,
    product: 0,
    name: "",
    kind: DeviceKind::Mouse,
};

/// [`MODELS`] as the shared discovery sees them, so the ids live in one table.
static HIDRAW_MODELS: [HidrawModel; MODELS.len()] = {
    let mut out = [PLACEHOLDER; MODELS.len()];
    let mut i = 0;
    while i < MODELS.len() {
        out[i] = HidrawModel {
            vendor: RAZER,
            product: MODELS[i].product,
            name: MODELS[i].name,
            kind: DeviceKind::Mouse,
        };
        i += 1;
    }
    out
};

pub static FAMILY: HidrawFamily = HidrawFamily {
    models: &HIDRAW_MODELS,
    interface: Some(0),
};

fn model(product: u16) -> Option<&'static Model> {
    MODELS.iter().find(|model| model.product == product)
}

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct RazerBackend;

#[async_trait::async_trait]
impl BatteryBackend for RazerBackend {
    fn name(&self) -> &'static str {
        "razer"
    }

    fn hidraw_family(&self) -> Option<&'static HidrawFamily> {
        Some(&FAMILY)
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        let devices = hidraw::discover(&FAMILY).await?;
        Ok(devices
            .into_iter()
            .filter_map(|device| -> Option<Box<dyn BatterySource>> {
                Some(Box::new(RazerSource {
                    model: model(device.identity.product)?,
                    info: device.info,
                    dev_path: device.dev_path,
                    identity: device.identity,
                    handle: None,
                }))
            })
            .collect())
    }
}

// ── Source ───────────────────────────────────────────────────────────────────

struct RazerSource {
    info: DeviceInfo,
    model: &'static Model,
    dev_path: PathBuf,
    identity: hidraw::NodeIdentity,
    /// Kept across polls; dropped on any error so the next poll reopens by node name.
    handle: Option<File>,
}

#[async_trait::async_trait]
impl BatterySource for RazerSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let dev_path = self.dev_path.clone();
        let identity = self.identity.clone();
        let model = self.model;
        let handle = self.handle.take();

        let (result, handle) = tokio::task::spawn_blocking(move || {
            let mut handle = handle;
            let result = poll_device(
                Path::new(hidraw::SYSFS_HIDRAW),
                &dev_path,
                &identity,
                model,
                &mut handle,
            );
            if result.is_err() {
                handle = None;
            }
            (result, handle)
        })
        .await
        .context("spawn_blocking")?;

        self.handle = handle;
        if let Ok(reading) = &result {
            tracing::debug!(device = %self.info.name, percent = reading.percent, "razer poll");
        }
        result
    }
}

fn poll_device(
    sysfs_root: &Path,
    dev_path: &Path,
    identity: &hidraw::NodeIdentity,
    model: &Model,
    handle: &mut Option<File>,
) -> anyhow::Result<BatteryReading> {
    if handle.is_none() {
        let node = dev_path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("no node name in {}", dev_path.display()))?;
        if let Some(dir) = openrazer_attributes(sysfs_root, node, identity)? {
            return read_openrazer(&dir, model);
        }
        let file = hidraw::open_verified(
            sysfs_root,
            dev_path,
            identity,
            std::fs::OpenOptions::new().read(true).write(true),
        )?;
        *handle = Some(file);
    }
    let file = handle
        .as_ref()
        .context("handle unexpectedly empty after open")?;

    let level = exchange(file, model, Query::Battery)?;
    let state = match model.power {
        Power::Rechargeable => charge_state(exchange(file, model, Query::Charging)?)?,
        Power::AaBattery => ChargeState::Discharging,
    };
    Ok(BatteryReading::new(percent(level), state))
}

/// Sends the request for `query` and reads its reply, again while the mouse is busy.
fn exchange(file: &File, model: &Model, query: Query) -> anyhow::Result<u8> {
    let request = request(model.transaction, query);
    set_feature(file, &request).context("sending feature report")?;
    for _ in 0..=BUSY_REREADS {
        std::thread::sleep(model.wait);
        let mut reply = [0u8; FEATURE_LEN];
        let n = get_feature(file, &mut reply).context("reading feature report")?;
        let reply = reply.get(..n).unwrap_or_default();
        if !crc_matches(reply) {
            tracing::debug!(?query, "razer reply crc does not match its bytes");
        }
        match parse_reply(reply, &request) {
            Reply::Value(value) => return Ok(value),
            Reply::Busy => {}
            Reply::MouseOff => anyhow::bail!(MOUSE_OFF),
            Reply::Invalid(reason) => anyhow::bail!("invalid reply: {reason}"),
        }
    }
    anyhow::bail!("mouse stayed busy")
}

// ── OpenRazer ────────────────────────────────────────────────────────────────

/// The directory with `razermouse`'s battery attributes when that driver owns
/// the node's HID device; `None` when another driver does.
fn openrazer_attributes(
    sysfs_root: &Path,
    node: &str,
    identity: &hidraw::NodeIdentity,
) -> anyhow::Result<Option<PathBuf>> {
    let device_dir = sysfs_root.join(node).join("device");
    let uevent_path = device_dir.join("uevent");
    let uevent = std::fs::read_to_string(&uevent_path)
        .with_context(|| format!("reading {}", uevent_path.display()))?;
    if hidraw::uevent_value(&uevent, "DRIVER") != Some(OPENRAZER_DRIVER) {
        return Ok(None);
    }
    if hidraw::stable_locator(&uevent, node) != identity.locator {
        return Err(anyhow::Error::new(hidraw::NodeReassigned {
            node: node.to_owned(),
        }));
    }
    let real = std::fs::canonicalize(&device_dir)
        .with_context(|| format!("canonicalizing {}", device_dir.display()))?;
    let usb_device = real
        .parent()
        .and_then(Path::parent)
        .with_context(|| format!("no USB device above {}", real.display()))?;
    let entries = std::fs::read_dir(usb_device)
        .with_context(|| format!("reading {}", usb_device.display()))?;
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|interface| interface.file_name().to_string_lossy().contains(':'))
        .filter_map(|interface| std::fs::read_dir(interface.path()).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|hid| hid.path())
        .filter(|hid| hid.join(CHARGE_LEVEL).is_file())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .next()
        .map(Some)
        .with_context(|| format!("{OPENRAZER_DRIVER} drives {node} but exposes no {CHARGE_LEVEL}"))
}

fn read_openrazer(dir: &Path, model: &Model) -> anyhow::Result<BatteryReading> {
    let level = read_attribute(&dir.join(CHARGE_LEVEL))?;
    let state = match model.power {
        Power::Rechargeable => charge_state(read_attribute(&dir.join(CHARGE_STATUS))?)?,
        Power::AaBattery => ChargeState::Discharging,
    };
    Ok(BatteryReading::new(percent(level), state))
}

fn read_attribute(path: &Path) -> anyhow::Result<u8> {
    let text = std::fs::read_to_string(path).map_err(|e| attribute_error(e, path))?;
    parse_attribute(&text).with_context(|| format!("parsing {}: {text:?}", path.display()))
}

/// The driver answers a read with the mouse's status: a timeout is the mouse being off.
fn attribute_error(e: std::io::Error, path: &Path) -> anyhow::Error {
    match e.kind() {
        ErrorKind::PermissionDenied => anyhow::Error::new(AccessDenied {
            path: path.to_owned(),
        }),
        ErrorKind::TimedOut => anyhow::anyhow!(MOUSE_OFF),
        _ => anyhow::Error::new(e).context(format!("reading {}", path.display())),
    }
}

fn parse_attribute(text: &str) -> Option<u8> {
    text.trim().parse().ok()
}

// ── Protocol (pure) ──────────────────────────────────────────────────────────

const REPORT_LEN: usize = 90;
/// Report id `0x00` first, then the report.
const FEATURE_LEN: usize = REPORT_LEN + 1;

const STATUS: usize = 0;
const TRANSACTION: usize = 1;
const REMAINING_PACKETS: std::ops::Range<usize> = 2..4;
const DATA_SIZE: usize = 5;
const COMMAND_CLASS: usize = 6;
const COMMAND_ID: usize = 7;
const VALUE: usize = 9;
const CRC: usize = 88;

const CLASS_POWER: u8 = 0x07;
const ARGUMENT_BYTES: u8 = 0x02;

const STATUS_BUSY: u8 = 0x01;
const STATUS_OK: u8 = 0x02;
const STATUS_FAILURE: u8 = 0x03;
const STATUS_TIMEOUT: u8 = 0x04;
const STATUS_NOT_SUPPORTED: u8 = 0x05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Query {
    Battery,
    Charging,
}

impl Query {
    const fn command_id(self) -> u8 {
        match self {
            Self::Battery => 0x80,
            Self::Charging => 0x84,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Value(u8),
    Busy,
    /// The receiver answered, the mouse behind it did not.
    MouseOff,
    Invalid(&'static str),
}

/// XOR of bytes 2..88 of the 90-byte report.
fn crc(report: &[u8]) -> u8 {
    report
        .iter()
        .take(CRC)
        .skip(REMAINING_PACKETS.start)
        .fold(0, |crc, &byte| crc ^ byte)
}

fn request(transaction: u8, query: Query) -> [u8; FEATURE_LEN] {
    let mut feature = [0u8; FEATURE_LEN];
    let report = &mut feature[1..];
    report[TRANSACTION] = transaction;
    report[DATA_SIZE] = ARGUMENT_BYTES;
    report[COMMAND_CLASS] = CLASS_POWER;
    report[COMMAND_ID] = query.command_id();
    report[CRC] = crc(report);
    feature
}

/// The 90-byte report of a feature buffer that holds exactly one, report id 0.
fn report_of(feature: &[u8]) -> Option<&[u8]> {
    match feature.split_first() {
        Some((0, report)) if report.len() == REPORT_LEN => Some(report),
        _ => None,
    }
}

fn crc_matches(feature: &[u8]) -> bool {
    report_of(feature).is_some_and(|report| report.get(CRC) == Some(&crc(report)))
}

/// What the reply in `feature` says about `request`.
fn parse_reply(feature: &[u8], request: &[u8; FEATURE_LEN]) -> Reply {
    let (Some(reply), Some(asked)) = (report_of(feature), report_of(request)) else {
        return Reply::Invalid("not a 90-byte report with id 0");
    };
    match reply[STATUS] {
        STATUS_BUSY => return Reply::Busy,
        STATUS_TIMEOUT => return Reply::MouseOff,
        _ => {}
    }
    let echoes = |field: std::ops::Range<usize>| reply.get(field.clone()) == asked.get(field);
    if !echoes(REMAINING_PACKETS)
        || !echoes(COMMAND_CLASS..COMMAND_CLASS + 1)
        || !echoes(COMMAND_ID..COMMAND_ID + 1)
    {
        return Reply::Invalid("reply to another command");
    }
    match reply[STATUS] {
        STATUS_OK => Reply::Value(reply[VALUE]),
        STATUS_FAILURE => Reply::Invalid("command failed"),
        STATUS_NOT_SUPPORTED => Reply::Invalid("command not supported"),
        _ => Reply::Invalid("unknown status"),
    }
}

/// The 0–255 level as a percent, rounded to nearest.
fn percent(raw: u8) -> u8 {
    let scaled = (u16::from(raw) * 100 + 127) / 255;
    u8::try_from(scaled).unwrap_or(100)
}

fn charge_state(flag: u8) -> anyhow::Result<ChargeState> {
    match flag {
        0 => Ok(ChargeState::Discharging),
        1 => Ok(ChargeState::Charging),
        _ => anyhow::bail!("charging flag {flag} out of range"),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::hidraw::fake_sysfs::FakeSysfs;

    /// The reply to `request` with `status` and `value`, crc filled in.
    fn reply(request: &[u8; FEATURE_LEN], status: u8, value: u8) -> [u8; FEATURE_LEN] {
        let mut feature = *request;
        let report = &mut feature[1..];
        report[STATUS] = status;
        report[VALUE] = value;
        report[CRC] = crc(report);
        feature
    }

    // Requests

    #[test]
    fn requests_match_the_documented_bytes() {
        let mut battery = [0u8; FEATURE_LEN];
        battery[2] = 0x1F;
        battery[6] = 0x02;
        battery[7] = 0x07;
        battery[8] = 0x80;
        battery[89] = 0x85;
        assert_eq!(request(0x1F, Query::Battery), battery);

        let charging = request(0xFF, Query::Charging);
        assert_eq!(charging[2], 0xFF);
        assert_eq!(charging[8], 0x84);
        assert_eq!(charging[89], 0x81);
        assert_eq!(charging[0], 0x00, "report id");
        assert_eq!(charging[90], 0x00, "reserved");
    }

    #[test]
    fn the_transaction_id_is_outside_the_crc() {
        assert_eq!(
            request(0x1F, Query::Battery)[89],
            request(0x3F, Query::Battery)[89]
        );
    }

    // Replies

    #[test]
    fn a_successful_reply_carries_argument_byte_1() {
        let asked = request(0x1F, Query::Battery);
        assert_eq!(
            parse_reply(&reply(&asked, STATUS_OK, 200), &asked),
            Reply::Value(200)
        );
    }

    #[test]
    fn busy_and_timeout_are_read_before_the_echo() {
        let asked = request(0x1F, Query::Battery);
        let mut busy = [0u8; FEATURE_LEN];
        busy[1] = STATUS_BUSY;
        assert_eq!(parse_reply(&busy, &asked), Reply::Busy);
        let mut off = [0u8; FEATURE_LEN];
        off[1] = STATUS_TIMEOUT;
        assert_eq!(parse_reply(&off, &asked), Reply::MouseOff);
        assert_eq!(
            parse_reply(&reply(&asked, STATUS_TIMEOUT, 0), &asked),
            Reply::MouseOff
        );
    }

    #[test]
    fn failure_statuses_are_not_readings() {
        let asked = request(0x1F, Query::Charging);
        for (status, reason) in [
            (STATUS_FAILURE, "command failed"),
            (STATUS_NOT_SUPPORTED, "command not supported"),
            (0x00, "unknown status"),
            (0x06, "unknown status"),
        ] {
            assert_eq!(
                parse_reply(&reply(&asked, status, 1), &asked),
                Reply::Invalid(reason)
            );
        }
    }

    #[test]
    fn a_reply_to_another_command_is_rejected() {
        let asked = request(0x1F, Query::Battery);
        let other = reply(&request(0x1F, Query::Charging), STATUS_OK, 1);
        assert_eq!(
            parse_reply(&other, &asked),
            Reply::Invalid("reply to another command")
        );
        let mut lighting = reply(&asked, STATUS_OK, 1);
        lighting[7] = 0x0F;
        assert_eq!(
            parse_reply(&lighting, &asked),
            Reply::Invalid("reply to another command")
        );
        let mut packets = reply(&asked, STATUS_OK, 1);
        packets[4] = 0x01;
        assert_eq!(
            parse_reply(&packets, &asked),
            Reply::Invalid("reply to another command")
        );
    }

    #[test]
    fn the_reply_transaction_id_is_not_checked() {
        let asked = request(0x1F, Query::Battery);
        let mut answer = reply(&asked, STATUS_OK, 77);
        answer[2] = 0x3F;
        assert_eq!(parse_reply(&answer, &asked), Reply::Value(77));
    }

    #[test]
    fn a_crc_mismatch_still_reads() {
        let asked = request(0x1F, Query::Battery);
        let mut answer = reply(&asked, STATUS_OK, 77);
        answer[89] ^= 0xFF;
        assert!(!crc_matches(&answer));
        assert_eq!(parse_reply(&answer, &asked), Reply::Value(77));
    }

    #[test]
    fn a_short_or_numbered_buffer_is_invalid() {
        let asked = request(0x1F, Query::Battery);
        let answer = reply(&asked, STATUS_OK, 77);
        assert!(matches!(
            parse_reply(&answer[..90], &asked),
            Reply::Invalid(_)
        ));
        let mut numbered = answer;
        numbered[0] = 0x01;
        assert!(matches!(parse_reply(&numbered, &asked), Reply::Invalid(_)));
        assert!(matches!(parse_reply(&[], &asked), Reply::Invalid(_)));
    }

    #[test]
    fn percent_scales_0_to_255() {
        assert_eq!(percent(0), 0);
        assert_eq!(percent(1), 0);
        assert_eq!(percent(2), 1);
        assert_eq!(percent(128), 50);
        assert_eq!(percent(254), 100);
        assert_eq!(percent(255), 100);
    }

    #[test]
    fn charging_flag_is_0_or_1() {
        assert_eq!(charge_state(0).unwrap(), ChargeState::Discharging);
        assert_eq!(charge_state(1).unwrap(), ChargeState::Charging);
        assert!(charge_state(2).is_err());
    }

    // Device table

    #[test]
    fn the_family_mirrors_the_model_table() {
        assert_eq!(FAMILY.interface, Some(0));
        assert_eq!(FAMILY.models.len(), MODELS.len());
        for (row, model) in FAMILY.models.iter().zip(MODELS) {
            assert_eq!(row.vendor, RAZER);
            assert_eq!(row.product, model.product);
            assert_eq!(row.name, model.name);
            assert_eq!(row.kind, DeviceKind::Mouse);
        }
    }

    #[test]
    fn every_model_has_a_known_transaction_id_and_wait() {
        for model in MODELS {
            assert!(
                [0x1F, 0x3F, 0xFF].contains(&model.transaction),
                "{:04x}",
                model.product
            );
            assert!(
                [WAIT_MOUSE, WAIT_RECEIVER, WAIT_VIPER, WAIT_ATHERIS].contains(&model.wait),
                "{:04x}",
                model.product
            );
            assert!(model.name.starts_with("Razer "), "{}", model.name);
        }
    }

    #[test]
    fn products_are_unique_and_sorted() {
        let products: Vec<u16> = MODELS.iter().map(|model| model.product).collect();
        assert!(
            products.windows(2).all(|pair| pair[0] < pair[1]),
            "{products:04x?}"
        );
    }

    #[test]
    fn the_aa_models_skip_the_charging_query() {
        let aa: Vec<u16> = MODELS
            .iter()
            .filter(|model| model.power == Power::AaBattery)
            .map(|model| model.product)
            .collect();
        assert_eq!(
            aa,
            [
                0x0062, 0x0083, 0x0094, 0x009C, 0x00B4, 0x00B8, 0x00B9, 0x00D3, 0x00D4
            ]
        );
    }

    #[test]
    fn interface_3_and_bluetooth_ids_are_left_out() {
        for product in [0x0095, 0x0096, 0x0099, 0x00CB] {
            assert!(model(product).is_none(), "{product:04x}");
        }
    }

    // Discovery against a fake sysfs tree

    #[test]
    fn discover_in_takes_interface_0_of_known_products() {
        let sysfs = FakeSysfs::new("razer-interface");
        sysfs.add("hidraw0", 0, RAZER, 0x00B7, "");
        sysfs.add("hidraw1", 1, RAZER, 0x00B7, "");
        sysfs.add("hidraw2", 2, RAZER, 0x00B7, "");
        sysfs.add("hidraw3", 0, RAZER, 0x0099, "");
        sysfs.add("hidraw4", 0, 0x1038, 0x00B7, "");
        let found: Vec<_> = FAMILY
            .discover_in(&sysfs.class())
            .unwrap()
            .into_iter()
            .map(|d| (d.dev_path, d.info.name, d.info.kind))
            .collect();
        assert_eq!(
            found,
            [(
                PathBuf::from("/dev/hidraw0"),
                "Razer DeathAdder V3 Pro".to_owned(),
                DeviceKind::Mouse
            )]
        );
    }

    /// Rebinds `node`'s HID device to `driver`, as `razer_mount` does.
    fn bind(sysfs: &FakeSysfs, node: &str, driver: &str) -> PathBuf {
        let hid = std::fs::canonicalize(sysfs.class().join(node).join("device")).unwrap();
        let uevent = std::fs::read_to_string(hid.join("uevent")).unwrap();
        let uevent = uevent.replace("DRIVER=hid-generic", &format!("DRIVER={driver}"));
        std::fs::write(hid.join("uevent"), uevent).unwrap();
        hid
    }

    fn identity(sysfs: &FakeSysfs) -> hidraw::NodeIdentity {
        FAMILY
            .match_node(&sysfs.class(), "hidraw0")
            .unwrap()
            .identity
    }

    fn write_attributes(hid: &Path, level: &str, status: &str) {
        std::fs::write(hid.join(CHARGE_LEVEL), level).unwrap();
        std::fs::write(hid.join(CHARGE_STATUS), status).unwrap();
    }

    #[test]
    fn a_node_on_another_driver_is_talked_to_directly() {
        let sysfs = FakeSysfs::new("razer-generic");
        sysfs.add("hidraw0", 0, RAZER, 0x00B7, "");
        assert_eq!(
            openrazer_attributes(&sysfs.class(), "hidraw0", &identity(&sysfs)).unwrap(),
            None
        );
    }

    #[test]
    fn openrazer_attributes_are_found_on_any_interface() {
        let sysfs = FakeSysfs::new("razer-openrazer-sibling");
        sysfs.add("hidraw0", 0, RAZER, 0x00B7, "");
        sysfs.add("hidraw1", 1, RAZER, 0x00B7, "");
        bind(&sysfs, "hidraw0", OPENRAZER_DRIVER);
        let mouse = bind(&sysfs, "hidraw1", OPENRAZER_DRIVER);
        write_attributes(&mouse, "255\n", "1\n");
        assert_eq!(
            openrazer_attributes(&sysfs.class(), "hidraw0", &identity(&sysfs)).unwrap(),
            Some(mouse)
        );
    }

    #[test]
    fn openrazer_without_attributes_is_an_error_not_a_hidraw_fallback() {
        let sysfs = FakeSysfs::new("razer-openrazer-bare");
        sysfs.add("hidraw0", 0, RAZER, 0x00B7, "");
        bind(&sysfs, "hidraw0", OPENRAZER_DRIVER);
        assert!(openrazer_attributes(&sysfs.class(), "hidraw0", &identity(&sysfs)).is_err());
    }

    #[test]
    fn a_recycled_node_under_openrazer_is_reassigned() {
        let sysfs = FakeSysfs::new("razer-openrazer-recycled");
        sysfs.add("hidraw0", 0, RAZER, 0x00B7, "");
        let hid = bind(&sysfs, "hidraw0", OPENRAZER_DRIVER);
        write_attributes(&hid, "128\n", "1\n");
        let other = hidraw::NodeIdentity {
            locator: "another-mouse".to_owned(),
            ..identity(&sysfs)
        };
        let err = openrazer_attributes(&sysfs.class(), "hidraw0", &other).unwrap_err();
        assert!(err.downcast_ref::<hidraw::NodeReassigned>().is_some());
    }

    #[test]
    fn poll_reads_openrazer_attributes_without_opening_the_node() {
        let sysfs = FakeSysfs::new("razer-openrazer-poll");
        sysfs.add("hidraw0", 0, RAZER, 0x00B7, "");
        let hid = bind(&sysfs, "hidraw0", OPENRAZER_DRIVER);
        write_attributes(&hid, "128\n", "1\n");
        let mut handle = None;
        let reading = poll_device(
            &sysfs.class(),
            Path::new("/nonexistent/hidraw0"),
            &identity(&sysfs),
            model(0x00B7).unwrap(),
            &mut handle,
        )
        .unwrap();
        assert_eq!(reading, BatteryReading::new(50, ChargeState::Charging));
        assert!(handle.is_none());
    }

    #[test]
    fn an_aa_mouse_under_openrazer_reads_no_charging_status() {
        let sysfs = FakeSysfs::new("razer-openrazer-aa");
        sysfs.add("hidraw0", 0, RAZER, 0x0094, "");
        let hid = bind(&sysfs, "hidraw0", OPENRAZER_DRIVER);
        std::fs::write(hid.join(CHARGE_LEVEL), "255\n").unwrap();
        let reading = read_openrazer(&hid, model(0x0094).unwrap()).unwrap();
        assert_eq!(reading, BatteryReading::new(100, ChargeState::Discharging));
    }

    #[test]
    fn attribute_errors_map_to_access_and_mouse_off() {
        let path = Path::new("/sys/x/charge_level");
        let denied = attribute_error(std::io::Error::from(ErrorKind::PermissionDenied), path);
        assert!(denied.downcast_ref::<AccessDenied>().is_some());
        let timeout = attribute_error(
            std::io::Error::from_raw_os_error(nix::libc::ETIMEDOUT),
            path,
        );
        assert_eq!(timeout.to_string(), MOUSE_OFF);
        let other = attribute_error(std::io::Error::from_raw_os_error(nix::libc::EIO), path);
        assert!(other.downcast_ref::<AccessDenied>().is_none());
        assert_ne!(other.to_string(), MOUSE_OFF);
    }

    #[test]
    fn attributes_parse_as_bytes() {
        assert_eq!(parse_attribute("255\n"), Some(255));
        assert_eq!(parse_attribute("0"), Some(0));
        assert_eq!(parse_attribute("256\n"), None);
        assert_eq!(parse_attribute(""), None);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn crc_round_trips(
                body in prop::collection::vec(any::<u8>(), REPORT_LEN),
                wrong in 1u8..=u8::MAX,
            ) {
                let mut feature = vec![0u8];
                feature.extend_from_slice(&body);
                feature[1 + CRC] = crc(&feature[1..]);
                prop_assert!(crc_matches(&feature));
                feature[1 + CRC] = feature[1 + CRC].wrapping_add(wrong);
                prop_assert!(!crc_matches(&feature));
            }

            #[test]
            fn parser_never_panics_on_any_length(
                feature in prop::collection::vec(any::<u8>(), 0..200),
            ) {
                let _ = crc_matches(&feature);
                let _ = parse_reply(&feature, &request(0x1F, Query::Battery));
            }

            #[test]
            fn every_full_reply_parses_without_panicking(
                body in prop::collection::vec(any::<u8>(), REPORT_LEN),
            ) {
                let mut feature = vec![0u8];
                feature.extend_from_slice(&body);
                let _ = parse_reply(&feature, &request(0x1F, Query::Charging));
            }

            #[test]
            fn percent_is_at_most_100_and_monotonic(raw in any::<u8>()) {
                prop_assert!(percent(raw) <= 100);
                prop_assert!(percent(raw) <= percent(raw.saturating_add(1)));
            }
        }
    }
}
