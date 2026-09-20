//! Shared sysfs helpers for the backends that identify a device through its
//! HID `uevent`.
//!
//! The two hidraw backends find their devices the same way — walk
//! `/sys/class/hidraw`, read each node's `device/uevent`, match the vendor and
//! product — so the parsing lives here rather than once per backend. The sysfs
//! backend reuses `stable_locator` against the same file, reached through
//! `<power_supply>/device`, because a HID-backed power supply is the same
//! device seen from the other side.

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

    let vendor = u32::from_str_radix(vendor_str.trim(), 16).ok()? as u16;
    let product = u32::from_str_radix(product_str.trim(), 16).ok()? as u16;

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

    // Captured from /sys/class/hidraw/hidraw11/device/uevent (8BitDo Ultimate 2,
    // a device that reports a serial) and hidraw6 (SteelSeries Aerox 5, which
    // reports none).
    const WITH_UNIQ: &str = "DRIVER=hid-generic\n\
         HID_ID=0003:00002DC8:00006013\n\
         HID_NAME=8BitDo Ultimate 2\n\
         HID_PHYS=usb-0000:13:00.0-1.2/input0\n\
         HID_UNIQ=350857A671\n";
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
    fn stable_locator_prefers_the_serial() {
        assert_eq!(stable_locator(WITH_UNIQ, "hidraw11"), "350857A671");
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
}
