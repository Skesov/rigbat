//! Shared discovery and I/O for the backends that talk to `/dev/hidraw*`.
//!
//! A hidraw backend declares a [`HidrawFamily`] — model table keyed by USB
//! vendor and product id, battery interface — and this module walks
//! `/sys/class/hidraw`, matches each node's `device/uevent` against it, and
//! hands back the matched nodes. The same family drives `rigbat doctor`'s
//! permission check and the udev rule drift test, so a model exists in
//! exactly one table. The sysfs backend reuses `stable_locator` against the
//! same file, reached through `<power_supply>/device`, because a HID-backed
//! power supply is the same device seen from the other side.

use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::{fd::AsFd, unix::fs::OpenOptionsExt as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::{fstat, major, minor};

use crate::domain::{BatteryReading, DeviceInfo, DeviceKind, Transport};
use crate::sources::{AccessDenied, BatterySource};

pub const SYSFS_HIDRAW: &str = "/sys/class/hidraw";

pub const VENDOR_STEELSERIES: u16 = 0x1038;
pub const VENDOR_CORSAIR: u16 = 0x1B1C;

/// A `/dev/hidraw*` node a backend recognised as one of its devices.
pub struct HidrawDevice {
    pub info: DeviceInfo,
    pub dev_path: PathBuf,
    /// Who owned the node when discovery matched it — checked again at every open.
    pub identity: NodeIdentity,
}

/// One supported model: its USB vendor and product id and how rigbat names it.
/// `name` is part of the device's identity and of config keys — never rename one.
pub struct HidrawModel {
    pub vendor: u16,
    pub product: u16,
    pub name: &'static str,
    pub kind: DeviceKind,
}

/// The devices one hidraw backend supports.
pub struct HidrawFamily {
    pub models: &'static [HidrawModel],
    /// The USB interface that carries battery data; `None` accepts any, for a
    /// device with a single HID interface.
    pub interface: Option<u8>,
}

impl HidrawFamily {
    /// `(vendor, product)` of every model, as the udev rule must list them.
    #[cfg(test)]
    pub fn usb_ids(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.models
            .iter()
            .map(|model| (model.vendor, model.product))
    }

    /// Every node under `sysfs_root` that is one of this family's devices.
    /// A missing root is an empty result, not an error: no hidraw driver, no devices.
    pub fn discover_in(&self, sysfs_root: &Path) -> anyhow::Result<Vec<HidrawDevice>> {
        if !sysfs_root.exists() {
            return Ok(Vec::new());
        }
        let entries = std::fs::read_dir(sysfs_root)
            .with_context(|| format!("reading {}", sysfs_root.display()))?;
        let mut devices = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!("skipping hidraw entry: {e}");
                    continue;
                }
            };
            if let Ok(device) = self.match_node(sysfs_root, &entry.file_name().to_string_lossy()) {
                devices.push(device);
            }
        }
        Ok(devices)
    }

    /// [`Self::discover_in`] without the nodes whose HID device already has a
    /// kernel `power_supply`: the sysfs backend reports those.
    pub fn discover_unclaimed_in(&self, sysfs_root: &Path) -> anyhow::Result<Vec<HidrawDevice>> {
        let mut devices = self.discover_in(sysfs_root)?;
        devices.retain(|device| {
            let kernel = kernel_reports_battery(sysfs_root, &device.dev_path);
            if kernel {
                tracing::debug!(
                    node = %device.dev_path.display(),
                    device = %device.info.name,
                    "a kernel driver reports this battery; left to the sysfs backend"
                );
            }
            !kernel
        });
        Ok(devices)
    }

    /// The device behind `<sysfs_root>/<node_name>`. Err when the node is not
    /// one of this family's battery interfaces or cannot be read.
    pub fn match_node(&self, sysfs_root: &Path, node_name: &str) -> anyhow::Result<HidrawDevice> {
        let device_dir = sysfs_root.join(node_name).join("device");
        let uevent_path = device_dir.join("uevent");
        let uevent = std::fs::read_to_string(&uevent_path)
            .with_context(|| format!("reading {}", uevent_path.display()))?;
        let identity = NodeIdentity::from_uevent(&uevent, node_name)
            .with_context(|| format!("no parsable HID_ID in {}", uevent_path.display()))?;

        let model = self
            .models
            .iter()
            .find(|model| model.vendor == identity.vendor && model.product == identity.product)
            .with_context(|| {
                format!(
                    "0x{:04X}:0x{:04X} not in device table",
                    identity.vendor, identity.product
                )
            })?;

        if let Some(wanted) = self.interface {
            let real = std::fs::canonicalize(&device_dir)
                .with_context(|| format!("canonicalizing {}", device_dir.display()))?;
            let real = real.to_string_lossy();
            let iface = parse_usb_interface(&real)
                .with_context(|| format!("parsing USB interface from {real}"))?;
            if iface != wanted {
                anyhow::bail!("interface {iface} != {wanted} (battery interface)");
            }
        }

        Ok(HidrawDevice {
            info: DeviceInfo {
                name: model.name.to_owned(),
                kind: model.kind,
                transport: Transport::Hidraw,
                locator: Some(identity.locator.clone()),
            },
            dev_path: PathBuf::from("/dev").join(node_name),
            identity,
        })
    }
}

/// [`HidrawFamily::discover_in`] on the live sysfs, off the async runtime: the walk is blocking `std::fs`.
pub async fn discover(family: &'static HidrawFamily) -> anyhow::Result<Vec<HidrawDevice>> {
    tokio::task::spawn_blocking(|| family.discover_in(Path::new(SYSFS_HIDRAW)))
        .await
        .context("spawn_blocking")?
}

/// [`HidrawFamily::discover_unclaimed_in`] on the live sysfs, off the async runtime.
pub async fn discover_unclaimed(
    family: &'static HidrawFamily,
) -> anyhow::Result<Vec<HidrawDevice>> {
    tokio::task::spawn_blocking(|| family.discover_unclaimed_in(Path::new(SYSFS_HIDRAW)))
        .await
        .context("spawn_blocking")?
}

/// Whether a kernel driver already registered a battery on the HID device behind `dev_path`.
fn kernel_reports_battery(sysfs_root: &Path, dev_path: &Path) -> bool {
    let Some(node) = dev_path.file_name() else {
        return false;
    };
    std::fs::read_dir(sysfs_root.join(node).join("device/power_supply"))
        .is_ok_and(|mut entries| entries.next().is_some())
}

/// The USB interface number from a canonical sysfs path: the last segment of
/// the form `<port>:1.N` gives N. Example: `"/sys/devices/…/7-1.1:1.3/…"` → `Some(3)`.
fn parse_usb_interface(real_path: &str) -> Option<u8> {
    real_path.split('/').rev().find_map(|seg| {
        // A segment with no ':' is not an interface, even if it reads "1.N".
        let (_, after_colon) = seg.rsplit_once(':')?;
        after_colon.strip_prefix("1.")?.parse::<u8>().ok()
    })
}

/// Ties a node to one physical device; the node number itself is recycled on replug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    pub vendor: u16,
    pub product: u16,
    pub locator: String,
}

impl NodeIdentity {
    fn from_uevent(uevent: &str, node_name: &str) -> Option<Self> {
        let (vendor, product) = parse_hid_id(uevent_value(uevent, "HID_ID")?)?;
        Some(Self {
            vendor,
            product,
            locator: stable_locator(uevent, node_name),
        })
    }
}

/// Distinct from an I/O error: retrying the path can never succeed, so the source is retired.
#[derive(Debug)]
pub struct NodeReassigned {
    pub node: String,
}

impl std::fmt::Display for NodeReassigned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "device node {} now belongs to another device", self.node)
    }
}

impl std::error::Error for NodeReassigned {}

/// Verifies after the open: a check made first leaves a window for a replug to swap the node.
pub fn open_verified(
    sysfs_root: &Path,
    dev_path: &Path,
    expected: &NodeIdentity,
    options: &OpenOptions,
) -> anyhow::Result<File> {
    let node = node_name(dev_path)?;
    let reassigned = || {
        anyhow::Error::new(NodeReassigned {
            node: node.to_owned(),
        })
    };
    let file = options.open(dev_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::Error::new(AccessDenied {
                path: dev_path.to_owned(),
            })
        } else {
            anyhow::Error::new(e).context(format!("opening {}", dev_path.display()))
        }
    })?;
    let node_dir = sysfs_root.join(node);

    let rdev = fstat(&file)
        .with_context(|| format!("fstat {}", dev_path.display()))?
        .st_rdev;
    let dev_file = node_dir.join("dev");
    let dev = std::fs::read_to_string(&dev_file)
        .with_context(|| format!("reading {}", dev_file.display()))?;
    let numbers = parse_dev_numbers(&dev)
        .with_context(|| format!("parsing {}: {dev:?}", dev_file.display()))?;
    if numbers != (major(rdev), minor(rdev)) {
        return Err(reassigned());
    }

    let uevent_path = node_dir.join("device/uevent");
    let uevent = std::fs::read_to_string(&uevent_path)
        .with_context(|| format!("reading {}", uevent_path.display()))?;
    if NodeIdentity::from_uevent(&uevent, node).as_ref() != Some(expected) {
        return Err(reassigned());
    }
    Ok(file)
}

/// `hidrawN` from `/dev/hidrawN`: the name of the node's sysfs directory.
pub fn node_name(dev_path: &Path) -> anyhow::Result<&str> {
    dev_path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("no node name in {}", dev_path.display()))
}

/// Parses a sysfs `dev` file (`MAJOR:MINOR`).
fn parse_dev_numbers(s: &str) -> Option<(u64, u64)> {
    let (major, minor) = s.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Value of `key` in a sysfs `uevent` body, or `None` when the key is absent
/// or present but empty. Empty is treated as absent because the kernel writes
/// `HID_UNIQ=` for every device that has no serial, and an empty locator
/// identifies nothing.
pub fn uevent_value<'a>(uevent: &'a str, key: &str) -> Option<&'a str> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .filter(|value| !value.is_empty())
}

/// Parses a `HID_ID=bus:vendor:product` line into `(vendor, product)`.
fn parse_hid_id(s: &str) -> Option<(u16, u16)> {
    let mut parts = s.splitn(3, ':');
    let _bus = parts.next()?;
    let vendor_str = parts.next()?;
    let product_str = parts.next()?;

    // Parsed as u16 directly: truncating a wider value with `as` would alias
    // `00011038` to vendor `1038` and match the wrong device.
    let vendor = u16::from_str_radix(vendor_str.trim(), 16).ok()?;
    let product = u16::from_str_radix(product_str.trim(), 16).ok()?;

    Some((vendor, product))
}

/// A locator that survives a replug.
///
/// `hidrawN` is assigned in enumeration order, so the node name changes when a
/// device is replugged or the machine boots with its dongles in a different
/// order. `DeviceId` treats the locator as identity, so an unstable one makes
/// one controller look like two: the retained reading is dropped and the
/// inventory grows a second row with a fresh "first seen".
///
/// Preference order:
/// 1. `HID_UNIQ` — the device's own serial or Bluetooth address, unique per
///    physical unit, so two identical controllers stay distinguishable.
/// 2. `HID_PHYS` — the USB topology path (`usb-0000:13:00.0-1.1/input3`).
///    Stable while the dongle stays in one port; moving it to another port
///    reads as a different device, which is the most a device with no serial
///    allows.
/// 3. The node name — no worse than what it replaces.
pub fn stable_locator(uevent: &str, node_name: &str) -> String {
    uevent_value(uevent, "HID_UNIQ")
        .or_else(|| uevent_value(uevent, "HID_PHYS"))
        .unwrap_or(node_name)
        .to_owned()
}

// ── Reading reports ──────────────────────────────────────────────────────────

/// Waits for the next input report until `deadline`: `Ok(Some(n))` when
/// `buf[..n]` holds one, `Ok(None)` when the deadline passed first.
pub fn next_report<D: Read + AsFd>(
    dev: &mut D,
    deadline: Instant,
    buf: &mut [u8],
) -> anyhow::Result<Option<usize>> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let remaining_ms = u16::try_from(remaining.as_millis()).unwrap_or(u16::MAX);
        if remaining_ms == 0 {
            return Ok(None);
        }
        let mut fds = [PollFd::new(dev.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, PollTimeout::from(remaining_ms)).context("poll()")? == 0 {
            return Ok(None);
        }
        let revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if !revents.contains(PollFlags::POLLIN) {
            anyhow::bail!("unexpected poll events {revents:?}");
        }
        match dev.read(buf) {
            Ok(0) => anyhow::bail!("EOF reading hidraw"),
            Ok(n) => return Ok(Some(n)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("reading hidraw"),
        }
    }
}

// ── Request / response ───────────────────────────────────────────────────────

const ANSWER_TIMEOUT: Duration = Duration::from_millis(1000);
/// Larger than any input report a request/response backend reads.
const REPORT_BUF_LEN: usize = 128;
/// Bounds the discard loop against a device that floods reports.
const MAX_QUEUED_REPORTS: usize = 256;

/// What one input report means to the exchange waiting on it.
pub enum Verdict<T> {
    Answer(T),
    /// The device or its dongle answered that there is no reading; ends the exchange.
    Reject(anyhow::Error),
    /// Another report on the same interface. Keep reading.
    Unrelated,
}

/// Drops queued input, writes `request`, then feeds each input report to
/// `classify` until it answers or rejects, or the answer timeout passes.
pub fn exchange<D, T>(
    dev: &mut D,
    request: &[u8],
    mut classify: impl FnMut(&[u8]) -> Verdict<T>,
) -> anyhow::Result<T>
where
    D: Read + Write + AsFd,
{
    discard_queued(dev)?;
    dev.write_all(request).context("writing request")?;
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut buf = [0u8; REPORT_BUF_LEN];
    while let Some(n) = next_report(dev, deadline, &mut buf)? {
        match classify(buf.get(..n).unwrap_or_default()) {
            Verdict::Answer(answer) => return Ok(answer),
            Verdict::Reject(reason) => return Err(reason),
            Verdict::Unrelated => {}
        }
    }
    anyhow::bail!("timeout waiting for the device's answer")
}

/// Reads and drops input queued since the last exchange, so a stale report is
/// not taken for the answer.
fn discard_queued<D: Read>(dev: &mut D) -> anyhow::Result<()> {
    let mut buf = [0u8; REPORT_BUF_LEN];
    for _ in 0..MAX_QUEUED_REPORTS {
        match dev.read(&mut buf) {
            Ok(0) => anyhow::bail!("EOF reading hidraw"),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e).context("discarding queued reports"),
        }
    }
    Ok(())
}

// ── A handle kept between polls ──────────────────────────────────────────────

/// The node a request/response source talks to and the handle it keeps
/// between polls. `H` is the open file plus whatever is valid for that one
/// handle only, reset on every reopen.
pub struct HandleSlot<H = File> {
    dev_path: PathBuf,
    identity: NodeIdentity,
    handle: Option<H>,
}

impl<H: From<File> + Send + 'static> HandleSlot<H> {
    fn new(dev_path: PathBuf, identity: NodeIdentity) -> Self {
        Self {
            dev_path,
            identity,
            handle: None,
        }
    }

    /// Runs `work` on the blocking pool with the handle, opening the node
    /// read-write and non-blocking when there is none. The handle is kept on
    /// `Ok` and dropped on `Err`, so the next poll reopens by node name.
    ///
    /// Aborting the calling task does not cancel `work`: it runs to completion
    /// (bounded by the exchange timeout) before the descriptor is dropped.
    pub async fn run<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(&mut H) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let dev_path = self.dev_path.clone();
        let identity = self.identity.clone();
        let handle = self.handle.take();
        let (result, handle) = tokio::task::spawn_blocking(move || {
            let mut handle = match handle {
                Some(handle) => handle,
                None => match open_read_write(&dev_path, &identity) {
                    Ok(file) => H::from(file),
                    Err(e) => return (Err(e), None),
                },
            };
            let result = work(&mut handle);
            let kept = result.is_ok().then_some(handle);
            (result, kept)
        })
        .await
        .context("spawn_blocking")?;
        self.handle = handle;
        result
    }
}

fn open_read_write(dev_path: &Path, identity: &NodeIdentity) -> anyhow::Result<File> {
    open_verified(
        Path::new(SYSFS_HIDRAW),
        dev_path,
        identity,
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK),
    )
}

/// How a request/response backend reads its device through a kept handle.
pub trait ReadBattery: Clone + Send + 'static {
    type Handle: From<File> + Send + 'static;

    fn read(&self, handle: &mut Self::Handle) -> anyhow::Result<BatteryReading>;
}

/// A source that holds its handle for its lifetime and reads through `P`.
pub struct HandleSource<P: ReadBattery> {
    info: DeviceInfo,
    slot: HandleSlot<P::Handle>,
    protocol: P,
}

impl<P: ReadBattery> HandleSource<P> {
    pub fn boxed(device: HidrawDevice, protocol: P) -> Box<dyn BatterySource> {
        Box::new(Self {
            info: device.info,
            slot: HandleSlot::new(device.dev_path, device.identity),
            protocol,
        })
    }
}

#[async_trait::async_trait]
impl<P: ReadBattery> BatterySource for HandleSource<P> {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let protocol = self.protocol.clone();
        let reading = self.slot.run(move |handle| protocol.read(handle)).await?;
        tracing::debug!(device = %self.info.name, percent = reading.percent, "hidraw poll");
        Ok(reading)
    }
}

#[cfg(test)]
pub mod fake_sysfs {
    use std::path::PathBuf;

    /// `class/hidrawN/device` links into `devices/…/1-1:1.<iface>/…`, as on a real system.
    pub struct FakeSysfs {
        pub root: PathBuf,
    }

    impl FakeSysfs {
        pub fn new(test_name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "rigbat-hidraw-discover-{test_name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("class")).expect("create fake sysfs");
            Self { root }
        }

        pub fn class(&self) -> PathBuf {
            self.root.join("class")
        }

        pub fn add(&self, node: &str, iface: u8, vendor: u16, product: u16, uniq: &str) {
            let device = self
                .root
                .join(format!("devices/usb1/1-1/1-1:1.{iface}/{node}-hid"));
            std::fs::create_dir_all(&device).expect("create fake HID device");
            let uevent = format!(
                "DRIVER=hid-generic\n\
                 HID_ID=0003:0000{vendor:04X}:0000{product:04X}\n\
                 HID_PHYS=usb-0000:13:00.0-1.1/input{iface}\n\
                 HID_UNIQ={uniq}\n"
            );
            std::fs::write(device.join("uevent"), uevent).expect("write uevent");
            let node_dir = self.class().join(node);
            std::fs::create_dir_all(&node_dir).expect("create fake hidraw node");
            std::os::unix::fs::symlink(&device, node_dir.join("device")).expect("link device");
        }

        /// A kernel driver's battery registered on `node`'s HID device.
        pub fn add_power_supply(&self, node: &str, name: &str) {
            let supply = self
                .class()
                .join(node)
                .join("device/power_supply")
                .join(name);
            std::fs::create_dir_all(supply).expect("create fake power_supply");
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(test)]
pub mod fake_device {
    use std::io::{Read, Write};
    use std::os::fd::{AsFd, BorrowedFd};
    use std::os::unix::net::UnixDatagram;

    /// Keeps report boundaries, as a hidraw node does.
    pub struct Datagrams(pub UnixDatagram);

    impl Read for Datagrams {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.recv(buf)
        }
    }

    impl Write for Datagrams {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.send(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl AsFd for Datagrams {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    /// The device end and a non-blocking host end with `reports` already queued.
    pub fn device_sending(reports: &[&[u8]]) -> (UnixDatagram, Datagrams) {
        let (device, host) = UnixDatagram::pair().expect("socket pair");
        host.set_nonblocking(true).expect("non-blocking host end");
        for report in reports {
            device.send(report).expect("queue a report");
        }
        (device, Datagrams(host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shape of a real `/sys/class/hidraw/hidrawN/device/uevent`: one device
    // that reports a serial (8BitDo Ultimate 2) and one that does not
    // (SteelSeries Aerox 5). Identifiers are placeholders — a serial or a MAC
    // captured from a real device names somebody's hardware.
    const WITH_UNIQ: &str = "DRIVER=hid-generic\n\
         HID_ID=0003:00002DC8:00006013\n\
         HID_NAME=8BitDo Ultimate 2\n\
         HID_PHYS=usb-0000:13:00.0-1.2/input0\n\
         HID_UNIQ=A1B2C3D4E5\n";
    const WITHOUT_UNIQ: &str = "DRIVER=hid-generic\n\
         HID_ID=0003:00001038:00001852\n\
         HID_NAME=SteelSeries SteelSeries Aerox 5 Wireless\n\
         HID_PHYS=usb-0000:13:00.0-1.1/input3\n\
         HID_UNIQ=\n";

    #[test]
    fn uevent_value_reads_a_key() {
        assert_eq!(
            uevent_value(WITH_UNIQ, "HID_ID"),
            Some("0003:00002DC8:00006013")
        );
    }

    #[test]
    fn uevent_value_treats_empty_as_absent() {
        assert_eq!(uevent_value(WITHOUT_UNIQ, "HID_UNIQ"), None);
    }

    #[test]
    fn uevent_value_does_not_match_a_longer_key() {
        assert_eq!(uevent_value("HID_IDX=1\n", "HID_ID"), None);
    }

    #[test]
    fn uevent_value_missing_key_is_none() {
        assert_eq!(uevent_value(WITH_UNIQ, "HID_NOTHING"), None);
    }

    #[test]
    fn parse_hid_id_valid() {
        assert_eq!(
            parse_hid_id("0003:00001038:00001852"),
            Some((0x1038, 0x1852))
        );
    }

    #[test]
    fn parse_hid_id_garbage_returns_none() {
        assert_eq!(parse_hid_id("not-a-hid-id"), None);
    }

    #[test]
    fn parse_hid_id_too_few_parts_returns_none() {
        assert_eq!(parse_hid_id("0003:00001038"), None);
    }

    #[test]
    fn parse_hid_id_invalid_hex_returns_none() {
        assert_eq!(parse_hid_id("0003:ZZZZZZZZ:00001852"), None);
    }

    #[test]
    fn parse_hid_id_rejects_an_id_wider_than_16_bits() {
        assert_eq!(parse_hid_id("0003:00011038:00001852"), None);
        assert_eq!(parse_hid_id("0003:00001038:00010000"), None);
    }

    #[test]
    fn stable_locator_prefers_the_serial() {
        assert_eq!(stable_locator(WITH_UNIQ, "hidraw11"), "A1B2C3D4E5");
    }

    #[test]
    fn stable_locator_falls_back_to_the_usb_path() {
        assert_eq!(
            stable_locator(WITHOUT_UNIQ, "hidraw6"),
            "usb-0000:13:00.0-1.1/input3"
        );
    }

    #[test]
    fn stable_locator_falls_back_to_the_node_name() {
        assert_eq!(stable_locator("DRIVER=hid-generic\n", "hidraw6"), "hidraw6");
    }

    #[test]
    fn stable_locator_survives_a_renumbered_node() {
        assert_eq!(
            stable_locator(WITH_UNIQ, "hidraw11"),
            stable_locator(WITH_UNIQ, "hidraw13")
        );
    }

    // ── open_verified against a fake sysfs tree ──────────────────────────────

    /// A regular file stands in for the node; its `st_rdev` is 0, so a matching `dev` reads `0:0`.
    struct FakeNode {
        root: PathBuf,
        sysfs: PathBuf,
        dev_path: PathBuf,
    }

    impl FakeNode {
        fn new(test_name: &str, uevent: &str, dev: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "rigbat-hidraw-test-{test_name}-{}",
                std::process::id()
            ));
            let sysfs = root.join("sys");
            let device_dir = sysfs.join("hidraw7/device");
            std::fs::create_dir_all(&device_dir).expect("create fake sysfs");
            std::fs::write(device_dir.join("uevent"), uevent).expect("write uevent");
            std::fs::write(sysfs.join("hidraw7/dev"), dev).expect("write dev");
            let dev_path = root.join("hidraw7");
            std::fs::write(&dev_path, b"").expect("write fake device node");
            Self {
                root,
                sysfs,
                dev_path,
            }
        }

        fn open(&self, expected: &NodeIdentity) -> anyhow::Result<File> {
            open_verified(
                &self.sysfs,
                &self.dev_path,
                expected,
                OpenOptions::new().read(true),
            )
        }
    }

    impl Drop for FakeNode {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn eightbitdo_identity() -> NodeIdentity {
        NodeIdentity::from_uevent(WITH_UNIQ, "hidraw7").expect("valid uevent")
    }

    #[test]
    fn node_identity_reads_vendor_product_and_locator() {
        assert_eq!(
            eightbitdo_identity(),
            NodeIdentity {
                vendor: 0x2DC8,
                product: 0x6013,
                locator: "A1B2C3D4E5".to_owned(),
            }
        );
    }

    #[test]
    fn open_verified_accepts_the_device_discovery_matched() {
        let node = FakeNode::new("match", WITH_UNIQ, "0:0\n");
        assert!(node.open(&eightbitdo_identity()).is_ok());
    }

    #[test]
    fn open_verified_rejects_a_node_now_owned_by_another_device() {
        let node = FakeNode::new("other-device", WITHOUT_UNIQ, "0:0\n");
        let err = node
            .open(&eightbitdo_identity())
            .expect_err("a different HID_ID must not open");
        assert!(err.is::<NodeReassigned>(), "{err:#}");
    }

    #[test]
    fn open_verified_rejects_the_same_model_with_another_serial() {
        let other_unit = WITH_UNIQ.replace("A1B2C3D4E5", "F6F7F8F9F0");
        let node = FakeNode::new("other-unit", &other_unit, "0:0\n");
        let err = node
            .open(&eightbitdo_identity())
            .expect_err("a different locator must not open");
        assert!(err.is::<NodeReassigned>(), "{err:#}");
    }

    #[test]
    fn open_verified_rejects_a_descriptor_that_is_not_the_sysfs_node() {
        let node = FakeNode::new("rdev", WITH_UNIQ, "243:7\n");
        let err = node
            .open(&eightbitdo_identity())
            .expect_err("st_rdev 0:0 differs from sysfs 243:7");
        assert!(err.is::<NodeReassigned>(), "{err:#}");
    }

    #[test]
    fn open_verified_reports_a_missing_node_as_an_ordinary_error() {
        let node = FakeNode::new("missing", WITH_UNIQ, "0:0\n");
        std::fs::remove_file(&node.dev_path).expect("remove fake device node");
        let err = node
            .open(&eightbitdo_identity())
            .expect_err("a vanished node cannot open");
        assert!(!err.is::<NodeReassigned>(), "{err:#}");
        assert!(!err.is::<AccessDenied>(), "{err:#}");
    }

    #[test]
    fn open_verified_reports_a_node_without_permission_as_access_denied() {
        use std::os::unix::fs::PermissionsExt as _;

        let node = FakeNode::new("denied", WITH_UNIQ, "0:0\n");
        std::fs::set_permissions(&node.dev_path, std::fs::Permissions::from_mode(0o000))
            .expect("chmod fake device node");
        // Root ignores file modes, so there is no denial to observe.
        if File::open(&node.dev_path).is_ok() {
            return;
        }
        let err = node
            .open(&eightbitdo_identity())
            .expect_err("a mode-000 node cannot open");
        let denied = err
            .downcast_ref::<AccessDenied>()
            .expect("permission denied is typed, not a string");
        assert_eq!(denied.path, node.dev_path);
    }

    #[test]
    fn parse_dev_numbers_reads_major_and_minor() {
        assert_eq!(parse_dev_numbers("243:7\n"), Some((243, 7)));
        assert_eq!(parse_dev_numbers("243"), None);
    }

    // ── discovery against a fake sysfs tree ──────────────────────────────────

    use super::fake_sysfs::FakeSysfs;

    use crate::sources::{eightbitdo, steelseries};

    fn summary(devices: &[HidrawDevice]) -> Vec<(String, Option<String>, PathBuf)> {
        let mut out: Vec<_> = devices
            .iter()
            .map(|d| {
                (
                    d.info.name.clone(),
                    d.info.locator.clone(),
                    d.dev_path.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn discover_in_keeps_each_backend_to_its_own_devices() {
        let sysfs = FakeSysfs::new("families");
        sysfs.add("hidraw1", 0, 0x1038, 0x1852, "");
        sysfs.add("hidraw3", 3, 0x1038, 0x1852, "");
        sysfs.add("hidraw5", 0, 0x2DC8, 0x6012, "A1B2C3D4E5");
        sysfs.add("hidraw7", 0, 0x046D, 0xC547, "");

        let mice = steelseries::FAMILY.discover_in(&sysfs.class()).unwrap();
        assert_eq!(
            summary(&mice),
            [(
                "SteelSeries Aerox 5 Wireless".to_owned(),
                Some("usb-0000:13:00.0-1.1/input3".to_owned()),
                PathBuf::from("/dev/hidraw3"),
            )]
        );
        assert_eq!(mice[0].info.kind, DeviceKind::Mouse);
        assert_eq!(mice[0].info.transport, Transport::Hidraw);

        let pads = eightbitdo::FAMILY.discover_in(&sysfs.class()).unwrap();
        assert_eq!(
            summary(&pads),
            [(
                "8BitDo Ultimate 2 Wireless".to_owned(),
                Some("A1B2C3D4E5".to_owned()),
                PathBuf::from("/dev/hidraw5"),
            )]
        );
        assert_eq!(pads[0].info.kind, DeviceKind::Controller);
        assert_eq!(
            pads[0].identity,
            NodeIdentity {
                vendor: 0x2DC8,
                product: 0x6012,
                locator: "A1B2C3D4E5".to_owned(),
            }
        );
    }

    #[test]
    fn match_node_rejects_another_interface_of_a_supported_model() {
        let sysfs = FakeSysfs::new("interface");
        sysfs.add("hidraw1", 0, 0x1038, 0x1852, "");
        let err = steelseries::FAMILY
            .match_node(&sysfs.class(), "hidraw1")
            .err()
            .expect("interface 0 is not the battery interface");
        assert!(format!("{err:#}").contains("interface 0 != 3"), "{err:#}");
    }

    #[test]
    fn match_node_without_an_interface_filter_accepts_any_interface() {
        let sysfs = FakeSysfs::new("any-interface");
        sysfs.add("hidraw2", 2, 0x2DC8, 0x6012, "");
        assert!(
            eightbitdo::FAMILY
                .match_node(&sysfs.class(), "hidraw2")
                .is_ok()
        );
    }

    #[test]
    fn match_node_rejects_another_vendor_and_an_unlisted_product() {
        let sysfs = FakeSysfs::new("vendor");
        sysfs.add("hidraw4", 3, 0x046D, 0x1852, "");
        sysfs.add("hidraw6", 3, 0x1038, 0x1853, "");
        let class = sysfs.class();
        let vendor = steelseries::FAMILY.match_node(&class, "hidraw4").err();
        let product = steelseries::FAMILY.match_node(&class, "hidraw6").err();
        assert!(
            format!("{vendor:?}").contains("0x046D:0x1852 not in device table"),
            "{vendor:?}"
        );
        assert!(
            format!("{product:?}").contains("0x1038:0x1853 not in device table"),
            "{product:?}"
        );
    }

    #[test]
    fn discover_unclaimed_in_leaves_a_kernel_reported_battery_to_sysfs() {
        let sysfs = FakeSysfs::new("unclaimed");
        sysfs.add("hidraw1", 3, 0x1038, 0x1852, "claimed");
        sysfs.add("hidraw2", 3, 0x1038, 0x1852, "plain");
        sysfs.add("hidraw3", 3, 0x1038, 0x1852, "empty-supply-dir");
        sysfs.add_power_supply("hidraw1", "steelseries_battery_hidraw1");
        std::fs::create_dir_all(sysfs.class().join("hidraw3/device/power_supply")).unwrap();
        let found = steelseries::FAMILY
            .discover_unclaimed_in(&sysfs.class())
            .unwrap();
        let mut locators: Vec<_> = found
            .iter()
            .filter_map(|d| d.info.locator.as_deref())
            .collect();
        locators.sort_unstable();
        assert_eq!(locators, ["empty-supply-dir", "plain"]);
    }

    #[test]
    fn discover_in_without_a_hidraw_class_finds_nothing() {
        let sysfs = FakeSysfs::new("no-class");
        let missing = sysfs.root.join("absent");
        assert!(
            steelseries::FAMILY
                .discover_in(&missing)
                .unwrap()
                .is_empty()
        );
    }

    // parse_usb_interface

    #[test]
    fn parse_usb_interface_extracts_interface_3() {
        assert_eq!(
            parse_usb_interface(
                "/sys/devices/pci0000:00/0000:00:14.0/usb7/7-1/7-1.1/7-1.1:1.3/0003:1038:1852.0018/hidraw/hidraw0"
            ),
            Some(3)
        );
    }

    #[test]
    fn parse_usb_interface_extracts_interface_0() {
        assert_eq!(
            parse_usb_interface(
                "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/hidraw/hidraw1"
            ),
            Some(0)
        );
    }

    #[test]
    fn parse_usb_interface_no_segment_returns_none() {
        assert_eq!(parse_usb_interface("/sys/devices/platform/hidraw2"), None);
    }

    #[test]
    fn parse_usb_interface_ignores_a_bare_segment_without_the_colon() {
        assert_eq!(parse_usb_interface("/sys/devices/1.0/hidraw/hidraw2"), None);
    }

    // ── reports, exchanges and the kept handle ───────────────────────────────

    use super::fake_device::device_sending;

    fn deadline_in(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn next_report_reads_one_report_at_a_time() {
        let (_device, mut host) = device_sending(&[&[1, 2, 3], &[4]]);
        let mut buf = [0u8; 8];
        assert_eq!(
            next_report(&mut host, deadline_in(100), &mut buf).unwrap(),
            Some(3)
        );
        assert_eq!(
            next_report(&mut host, deadline_in(100), &mut buf).unwrap(),
            Some(1)
        );
        assert_eq!(buf[0], 4);
    }

    #[test]
    fn next_report_from_a_silent_device_is_none_at_the_deadline() {
        let (_device, mut host) = device_sending(&[]);
        let mut buf = [0u8; 8];
        assert_eq!(
            next_report(&mut host, deadline_in(20), &mut buf).unwrap(),
            None
        );
    }

    fn classify(report: &[u8]) -> Verdict<u8> {
        match *report {
            [0xA0, value] => Verdict::Answer(value),
            [0xA0] => Verdict::Reject(anyhow::anyhow!("device is off")),
            _ => Verdict::Unrelated,
        }
    }

    /// A device with a stale answer queued that answers one request with `replies`.
    fn answering(
        replies: &'static [&'static [u8]],
    ) -> (
        std::thread::JoinHandle<Vec<u8>>,
        super::fake_device::Datagrams,
    ) {
        let (device, host) = device_sending(&[&[0xA0, 1]]);
        let responder = std::thread::spawn(move || {
            let mut request = [0u8; 16];
            let n = device.recv(&mut request).expect("a request");
            for reply in replies {
                device.send(reply).expect("send a reply");
            }
            request[..n].to_vec()
        });
        (responder, host)
    }

    #[test]
    fn exchange_skips_stale_and_unrelated_reports_until_the_answer() {
        let (responder, mut host) = answering(&[&[0x01, 0x02], &[0xA0, 42]]);
        assert_eq!(exchange(&mut host, &[0x10, 0x20], classify).unwrap(), 42);
        assert_eq!(responder.join().unwrap(), [0x10, 0x20]);
    }

    #[test]
    fn exchange_ends_at_a_rejection() {
        let (responder, mut host) = answering(&[&[0xA0], &[0xA0, 42]]);
        let err = exchange(&mut host, &[0x10], classify).unwrap_err();
        assert_eq!(err.to_string(), "device is off");
        responder.join().unwrap();
    }

    #[test]
    fn exchange_without_an_answer_times_out() {
        let (responder, mut host) = answering(&[&[0x01]]);
        let err = exchange(&mut host, &[0x10], classify).unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err:#}");
        responder.join().unwrap();
    }

    fn slot_holding(file: File) -> HandleSlot {
        HandleSlot {
            dev_path: PathBuf::from("/nonexistent/hidraw0"),
            identity: eightbitdo_identity(),
            handle: Some(file),
        }
    }

    #[tokio::test]
    async fn a_handle_slot_keeps_the_handle_on_ok_and_drops_it_on_err() {
        let mut slot = slot_holding(File::open("/dev/null").unwrap());
        assert_eq!(slot.run(|_| Ok(7)).await.unwrap(), 7);
        assert!(slot.handle.is_some());
        let failed: anyhow::Result<()> = slot.run(|_| anyhow::bail!("boom")).await;
        assert!(failed.is_err());
        assert!(slot.handle.is_none());
        let reopen = slot.run(|_| Ok(())).await.unwrap_err();
        assert!(
            format!("{reopen:#}").contains("/nonexistent/hidraw0"),
            "{reopen:#}"
        );
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn parsers_never_panic_on_arbitrary_text(text in any::<String>(), key in any::<String>()) {
                let _ = uevent_value(&text, &key);
                let _ = parse_hid_id(&text);
                let _ = stable_locator(&text, &key);
            }

            #[test]
            fn uevent_value_finds_the_key_among_others(
                others in prop::collection::vec(("[A-Z_]{1,10}", "[^\r\n]{0,20}"), 0..8),
                key in "[A-Z_]{1,10}",
                value in "[^\r\n]{0,20}",
                at in any::<prop::sample::Index>(),
            ) {
                let mut lines: Vec<String> = others
                    .into_iter()
                    .filter(|(k, _)| *k != key)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                lines.insert(at.index(lines.len() + 1), format!("{key}={value}"));
                let uevent = lines.join("\n");

                let expected = (!value.is_empty()).then_some(value.as_str());
                prop_assert_eq!(uevent_value(&uevent, &key), expected);
            }

            #[test]
            fn parse_hid_id_round_trips_and_rejects_ids_wider_than_16_bits(
                bus in any::<u16>(),
                vendor in any::<u32>(),
                product in any::<u32>(),
            ) {
                let parsed = parse_hid_id(&format!("{bus:04X}:{vendor:08X}:{product:08X}"));
                let expected = u16::try_from(vendor).ok().zip(u16::try_from(product).ok());
                prop_assert_eq!(parsed, expected);
            }

            #[test]
            fn stable_locator_is_never_empty(uevent in any::<String>(), node in ".+") {
                prop_assert!(!stable_locator(&uevent, &node).is_empty());
            }

            #[test]
            fn parse_usb_interface_never_panics(path in any::<String>()) {
                let _ = parse_usb_interface(&path);
            }

            #[test]
            fn parse_usb_interface_needs_a_config_colon(path in "(/[0-9.-]{1,5}){0,6}") {
                prop_assert_eq!(parse_usb_interface(&path), None);
            }

            #[test]
            fn parse_usb_interface_round_trips_a_sysfs_path(
                bus in 1u8..=16,
                port in "[1-9](\\.[1-9]){0,3}",
                iface in any::<u8>(),
                vendor in any::<u16>(),
                product in any::<u16>(),
                seq in any::<u16>(),
                node in any::<u16>(),
            ) {
                let path = format!(
                    "/sys/devices/pci0000:00/0000:00:14.0/usb{bus}/{bus}-{port}/\
                     {bus}-{port}:1.{iface}/0003:{vendor:04X}:{product:04X}.{seq:04X}/\
                     hidraw/hidraw{node}"
                );
                prop_assert_eq!(parse_usb_interface(&path), Some(iface));
            }
        }
    }
}
