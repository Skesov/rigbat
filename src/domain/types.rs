use std::time::Instant;

use crate::domain::estimate::Estimate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Mouse,
    Keyboard,
    Headset,
    Controller,
    Other,
}

/// Guesses the device type from its display name using case-insensitive keyword substring match.
///
/// Match order: Mouse → Keyboard → Headset → Controller → Other.
/// The first matching category wins.
pub fn guess_kind(name: &str) -> DeviceKind {
    let lower = name.to_lowercase();

    const MOUSE_KEYWORDS: &[&str] = &[
        "mouse",
        "aerox",
        "rival",
        "viper",
        "deathadder",
        "basilisk",
        "mx anywhere",
        "mx master",
        "g pro",
        "m705",
        "m750",
    ];
    const KEYBOARD_KEYWORDS: &[&str] = &[
        "keyboard", "apex", "huntsman", "k70", "k95", "g915", "air75", "air96", "nuphy", "keychron",
    ];
    const HEADSET_KEYWORDS: &[&str] = &[
        "headphone",
        "headset",
        "earphone",
        "buds",
        "hitune",
        "arctis",
        "wh-",
        "wf-",
        "momentum",
        "qc",
    ];
    const CONTROLLER_KEYWORDS: &[&str] = &[
        "dualsense",
        "dualshock",
        "controller",
        "gamepad",
        "xbox",
        "joycon",
    ];

    if MOUSE_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return DeviceKind::Mouse;
    }
    if KEYBOARD_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return DeviceKind::Keyboard;
    }
    if HEADSET_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return DeviceKind::Headset;
    }
    if CONTROLLER_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return DeviceKind::Controller;
    }

    DeviceKind::Other
}

impl DeviceKind {
    /// Canonical string spelling of the variant. Part of the `--json` CLI contract
    /// (the "kind" field) — changing these values changes that output.
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::Mouse => "mouse",
            DeviceKind::Keyboard => "keyboard",
            DeviceKind::Headset => "headset",
            DeviceKind::Controller => "controller",
            DeviceKind::Other => "other",
        }
    }
}

/// Returns the freedesktop icon-theme name for a device kind.
///
/// The returned string can be used directly as a `gtk::Image` icon name.
pub fn freedesktop_icon_name(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Mouse => "input-mouse",
        DeviceKind::Keyboard => "input-keyboard",
        DeviceKind::Headset => "audio-headset",
        DeviceKind::Controller => "input-gaming",
        DeviceKind::Other => "battery",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Discharging,
    Charging,
    Full,
}

/// How rigbat reaches a device. Distinguishes otherwise same-named duplicates
/// (e.g. a mouse seen over both sysfs/HID++ and Bluetooth).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    Sysfs,
    Bluetooth,
    Hidraw,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Sysfs => "sysfs",
            Transport::Bluetooth => "bluetooth",
            Transport::Hidraw => "hidraw",
        }
    }
}

/// Stable identity of a device: the tuple that distinguishes two entries the
/// project deliberately does not deduplicate (e.g. one mouse seen over both
/// sysfs/HID++ and Bluetooth is shown twice; rigbat does not merge them).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId {
    pub name: String,
    pub transport: Transport,
    pub locator: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
    pub transport: Transport,
    /// Source-specific stable locator for debugging: BlueZ MAC, sysfs power_supply
    /// directory name, or hidraw node path. None if the source cannot provide one.
    pub locator: Option<String>,
}

impl DeviceInfo {
    pub fn id(&self) -> DeviceId {
        DeviceId {
            name: self.name.clone(),
            transport: self.transport,
            locator: self.locator.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub percent: u8, // invariant: 0..=100
    pub state: ChargeState,
}

impl BatteryReading {
    /// Creates a reading, constraining percent to range 0..=100.
    pub fn new(percent: u8, state: ChargeState) -> Self {
        Self {
            percent: percent.min(100),
            state,
        }
    }
}

/// How reachable a device currently is. `last_reading` is retained across all
/// three states, so a device that is asleep or switched off still shows the
/// charge it last reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Polling succeeds.
    Online,
    /// Still discovered, but the last few polls failed. Wireless peripherals
    /// drop telemetry packets as normal behaviour, so a single failure is not
    /// treated as a state change.
    Unreachable,
    /// Gone from discovery entirely: powered off, or switched to another host.
    Disconnected,
}

/// A device's presence and last-known reading, retained across polling gaps
/// and disconnects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceState {
    pub info: DeviceInfo,
    /// Last successful reading. Survives going Unreachable or Disconnected.
    pub last_reading: Option<BatteryReading>,
    /// When `last_reading` was taken. `None` if the device has never answered.
    /// `Instant`, not `SystemTime`: the only consumer is a relative age
    /// ("2h ago"), a monotonic clock cannot be thrown off by an NTP step or a
    /// suspend/resume jump, and nothing here is persisted across restarts.
    pub last_seen: Option<Instant>,
    pub presence: Presence,
    /// Remaining-time estimate derived from this device's recorded
    /// percent-change history (kept in the supervisor, not here — see
    /// `push_reading` in `app::supervisor`).
    pub estimate: Estimate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guess_kind_mouse() {
        assert_eq!(guess_kind("MX Anywhere 3"), DeviceKind::Mouse);
    }

    #[test]
    fn guess_kind_keyboard() {
        assert_eq!(guess_kind("NuPhy Air75 V2-2"), DeviceKind::Keyboard);
    }

    #[test]
    fn guess_kind_headset() {
        assert_eq!(guess_kind("UGREEN HiTune Max5"), DeviceKind::Headset);
    }

    #[test]
    fn guess_kind_controller() {
        assert_eq!(guess_kind("DualSense"), DeviceKind::Controller);
    }

    #[test]
    fn guess_kind_other() {
        assert_eq!(guess_kind("Unknown Thing"), DeviceKind::Other);
    }

    #[test]
    fn guess_kind_case_insensitive() {
        assert_eq!(guess_kind("AEROX"), DeviceKind::Mouse);
    }

    #[test]
    fn freedesktop_icon_name_mouse() {
        assert_eq!(freedesktop_icon_name(DeviceKind::Mouse), "input-mouse");
    }

    #[test]
    fn freedesktop_icon_name_keyboard() {
        assert_eq!(
            freedesktop_icon_name(DeviceKind::Keyboard),
            "input-keyboard"
        );
    }

    #[test]
    fn freedesktop_icon_name_headset() {
        assert_eq!(freedesktop_icon_name(DeviceKind::Headset), "audio-headset");
    }

    #[test]
    fn freedesktop_icon_name_controller() {
        assert_eq!(
            freedesktop_icon_name(DeviceKind::Controller),
            "input-gaming"
        );
    }

    #[test]
    fn freedesktop_icon_name_other() {
        assert_eq!(freedesktop_icon_name(DeviceKind::Other), "battery");
    }

    #[test]
    fn device_kind_as_str_exhaustive() {
        assert_eq!(DeviceKind::Mouse.as_str(), "mouse");
        assert_eq!(DeviceKind::Keyboard.as_str(), "keyboard");
        assert_eq!(DeviceKind::Headset.as_str(), "headset");
        assert_eq!(DeviceKind::Controller.as_str(), "controller");
        assert_eq!(DeviceKind::Other.as_str(), "other");
    }

    fn info(name: &str, transport: Transport) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            kind: DeviceKind::Other,
            transport,
            locator: None,
        }
    }

    #[test]
    fn device_id_differs_by_transport() {
        let sysfs = info("mouse", Transport::Sysfs);
        let bluetooth = info("mouse", Transport::Bluetooth);
        assert_ne!(sysfs.id(), bluetooth.id());
    }

    #[test]
    fn device_id_equal_for_identical_inputs_and_hashes_equal() {
        let a = info("mouse", Transport::Sysfs);
        let b = info("mouse", Transport::Sysfs);
        assert_eq!(a.id(), b.id());

        let mut set = std::collections::HashSet::new();
        set.insert(a.id());
        set.insert(b.id());
        assert_eq!(set.len(), 1);
    }
}
