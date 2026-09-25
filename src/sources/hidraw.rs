//! Shared discovery for the backends that talk to `/dev/hidraw*`.
//!
//! A hidraw backend declares a [`HidrawFamily`] — vendor, model table, battery
//! interface — and this module walks `/sys/class/hidraw`, matches each node's
//! `device/uevent` against it, and hands back the matched nodes. The same
//! family drives `rigbat doctor`'s permission check and the udev rule drift
//! test, so a model exists in exactly one table. The sysfs backend reuses
//! `stable_locator` against the same file, reached through
//! `<power_supply>/device`, because a HID-backed power supply is the same
//! device seen from the other side.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use nix::sys::stat::{fstat, major, minor};

use crate::domain::{DeviceInfo, DeviceKind, Transport};
use crate::sources::AccessDenied;

pub const SYSFS_HIDRAW: &str = "/sys/class/hidraw";

/// A `/dev/hidraw*` node a backend recognised as one of its devices.
pub struct HidrawDevice {
    pub info: DeviceInfo,
    pub dev_path: PathBuf,
    /// Who owned the node when discovery matched it — checked again at every open.
    pub identity: NodeIdentity,
}

/// One supported model: its USB product id and how rigbat names it.
/// `name` is part of the device's identity and of config keys — never rename one.
pub struct HidrawModel {
    pub product: u16,
    pub name: &'static str,
    pub kind: DeviceKind,
}

/// The devices one hidraw backend supports.
pub struct HidrawFamily {
    pub vendor: u16,
    pub models: &'static [HidrawModel],
    /// The USB interface that carries battery data; `None` accepts any, for a
    /// device with a single HID interface.
    pub interface: Option<u8>,
}

impl HidrawFamily {
    /// `(vendor, product)` of every model, as the udev rule must list them.
    #[cfg(test)]
    pub fn usb_ids(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.models.iter().map(|model| (self.vendor, model.product))
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

    /// The device behind `<sysfs_root>/<node_name>`. Err when the node is not
    /// one of this family's battery interfaces or cannot be read.
    pub fn match_node(&self, sysfs_root: &Path, node_name: &str) -> anyhow::Result<HidrawDevice> {
        let device_dir = sysfs_root.join(node_name).join("device");
        let uevent_path = device_dir.join("uevent");
        let uevent = std::fs::read_to_string(&uevent_path)
            .with_context(|| format!("reading {}", uevent_path.display()))?;
        let identity = NodeIdentity::from_uevent(&uevent, node_name)
            .with_context(|| format!("no parsable HID_ID in {}", uevent_path.display()))?;

        if identity.vendor != self.vendor {
            anyhow::bail!("vendor 0x{:04X} != 0x{:04X}", identity.vendor, self.vendor);
        }
        let model = self
            .models
            .iter()
            .find(|model| model.product == identity.product)
            .with_context(|| format!("product 0x{:04X} not in device table", identity.product))?;

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

/// [`HidrawFamily::discover_in`] on the live sysfs, off the async runtime: the
/// walk is blocking `std::fs`, and how long a sysfs read takes is the kernel's business.
pub async fn discover(family: &'static HidrawFamily) -> anyhow::Result<Vec<HidrawDevice>> {
    tokio::task::spawn_blocking(|| family.discover_in(Path::new(SYSFS_HIDRAW)))
        .await
        .context("spawn_blocking")?
}

/// The USB interface number from a canonical sysfs path: the last segment of
/// the form `<port>:1.N` gives N. Example: `"/sys/devices/…/7-1.1:1.3/…"` → `Some(3)`.
pub fn parse_usb_interface(real_path: &str) -> Option<u8> {
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
    pub fn from_uevent(uevent: &str, node_name: &str) -> Option<Self> {
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
    let node = dev_path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("no node name in {}", dev_path.display()))?;
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
pub fn parse_hid_id(s: &str) -> Option<(u16, u16)> {
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

    /// `class/hidrawN/device` links into `devices/…/1-1:1.<iface>/…`, as on a real system.
    struct FakeSysfs {
        root: PathBuf,
    }

    impl FakeSysfs {
        fn new(test_name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "rigbat-hidraw-discover-{test_name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("class")).expect("create fake sysfs");
            Self { root }
        }

        fn class(&self) -> PathBuf {
            self.root.join("class")
        }

        fn add(&self, node: &str, iface: u8, vendor: u16, product: u16, uniq: &str) {
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
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

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
            format!("{vendor:?}").contains("vendor 0x046D"),
            "{vendor:?}"
        );
        assert!(
            format!("{product:?}").contains("product 0x1853"),
            "{product:?}"
        );
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
