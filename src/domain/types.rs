use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::estimate::Estimate;
use crate::domain::time::BootTime;
use crate::i18n::{Lang, fl, loader};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Mouse,
    Keyboard,
    Headset,
    Controller,
    Other,
}

/// Guesses the device type from its display name using case-insensitive keyword
/// substring match, plus whole-word match for earbud names.
///
/// Match order: Mouse → Keyboard → Headset → Controller → Other.
/// The first matching category wins.
pub fn guess_kind(name: &str) -> DeviceKind {
    let lower = name.to_lowercase();

    // A category word states what the device is; a product-line word only
    // hints, and vendors reuse a line across categories — Logitech ships both
    // a "G Pro" mouse and a "G Pro X" headset. Categories must therefore be
    // matched first.
    const GENERIC: &[(&str, DeviceKind)] = &[
        ("mouse", DeviceKind::Mouse),
        ("keyboard", DeviceKind::Keyboard),
        ("headset", DeviceKind::Headset),
        ("headphone", DeviceKind::Headset),
        ("earphone", DeviceKind::Headset),
        ("earbud", DeviceKind::Headset),
        ("buds", DeviceKind::Headset),
        ("controller", DeviceKind::Controller),
        ("gamepad", DeviceKind::Controller),
    ];

    // Product lines and model numbers. Only consulted when no category word
    // appears, so a line name can never override an explicit one.
    const MODELS: &[(&str, DeviceKind)] = &[
        ("aerox", DeviceKind::Mouse),
        ("rival", DeviceKind::Mouse),
        ("viper", DeviceKind::Mouse),
        ("deathadder", DeviceKind::Mouse),
        ("basilisk", DeviceKind::Mouse),
        ("mx anywhere", DeviceKind::Mouse),
        ("mx master", DeviceKind::Mouse),
        ("g pro", DeviceKind::Mouse),
        ("m705", DeviceKind::Mouse),
        ("m750", DeviceKind::Mouse),
        ("apex", DeviceKind::Keyboard),
        ("huntsman", DeviceKind::Keyboard),
        ("k70", DeviceKind::Keyboard),
        ("k95", DeviceKind::Keyboard),
        ("g915", DeviceKind::Keyboard),
        ("air75", DeviceKind::Keyboard),
        ("air96", DeviceKind::Keyboard),
        ("nuphy", DeviceKind::Keyboard),
        ("keychron", DeviceKind::Keyboard),
        ("hitune", DeviceKind::Headset),
        ("arctis", DeviceKind::Headset),
        ("wh-", DeviceKind::Headset),
        ("wf-", DeviceKind::Headset),
        ("momentum", DeviceKind::Headset),
        ("qc", DeviceKind::Headset),
        ("dualsense", DeviceKind::Controller),
        ("dualshock", DeviceKind::Controller),
        ("xbox", DeviceKind::Controller),
        ("joycon", DeviceKind::Controller),
        ("8bitdo", DeviceKind::Controller),
    ];

    for (kw, kind) in GENERIC {
        if lower.contains(kw) {
            return *kind;
        }
    }
    // Whole words only: "ear" sits inside "Nearby", "Gear", "Clear".
    const HEADSET_WORDS: &[&str] = &[
        "ear",
        "buds",
        "earbuds",
        "airpods",
        "headphone",
        "headphones",
        "earphone",
    ];
    if lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| HEADSET_WORDS.contains(&word))
    {
        return DeviceKind::Headset;
    }
    for (kw, kind) in MODELS {
        if lower.contains(kw) {
            return *kind;
        }
    }

    DeviceKind::Other
}

impl DeviceKind {
    /// Canonical string spelling of the variant. Part of the `--json` CLI contract
    /// (the "kind" field) and of stored inventory rows — never translate; see `label`.
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::Mouse => "mouse",
            DeviceKind::Keyboard => "keyboard",
            DeviceKind::Headset => "headset",
            DeviceKind::Controller => "controller",
            DeviceKind::Other => "other",
        }
    }

    pub fn label(self, lang: Lang) -> String {
        let l = loader(lang);
        match self {
            DeviceKind::Mouse => fl!(l, "kind-mouse"),
            DeviceKind::Keyboard => fl!(l, "kind-keyboard"),
            DeviceKind::Headset => fl!(l, "kind-headset"),
            DeviceKind::Controller => fl!(l, "kind-controller"),
            DeviceKind::Other => fl!(l, "kind-other"),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeState {
    Discharging,
    Charging,
    Full,
}

/// How rigbat reaches a device. Distinguishes otherwise same-named duplicates
/// (e.g. a mouse seen over both sysfs/HID++ and Bluetooth).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Sysfs,
    Bluetooth,
    Hidraw,
}

impl Transport {
    /// Also shown untranslated in the UI: these are technology names.
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
    /// Source-specific locator that survives a replug: BlueZ MAC, sysfs
    /// power_supply directory name, or — for hidraw — the device's serial or
    /// USB path (`sources::hidraw::stable_locator`), never the `hidrawN` node
    /// name, which the kernel reassigns in enumeration order. None if the
    /// source cannot provide one.
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
    /// `percent` stands in for a level band, so no remaining-time estimate uses it.
    pub coarse: bool,
}

impl BatteryReading {
    /// Creates a reading, constraining percent to range 0..=100.
    pub fn new(percent: u8, state: ChargeState) -> Self {
        Self {
            percent: percent.min(100),
            state,
            coarse: false,
        }
    }

    pub fn new_coarse(percent: u8, state: ChargeState) -> Self {
        Self {
            coarse: true,
            ..Self::new(percent, state)
        }
    }
}

/// How reachable a device currently is. `last_reading` is retained across all
/// states, so a device that is asleep or switched off still shows the
/// charge it last reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// Polling succeeds.
    Online,
    /// Still discovered, but the last few polls failed. Wireless peripherals
    /// drop telemetry packets as normal behaviour, so a single failure is not
    /// treated as a state change.
    Unreachable,
    /// Gone from discovery entirely: powered off, or switched to another host.
    Disconnected,
    /// Discovered, but this user may not open it (udev rule missing).
    NoAccess,
}

/// One poll's result, for the one-shot surfaces that keep no running presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollOutcome {
    Reading(BatteryReading),
    /// The device did not answer.
    Failed,
    /// The device cannot be opened by this user.
    NoAccess,
}

impl PollOutcome {
    pub fn reading(self) -> Option<BatteryReading> {
        match self {
            Self::Reading(r) => Some(r),
            Self::Failed | Self::NoAccess => None,
        }
    }

    pub fn presence(self) -> Presence {
        match self {
            Self::Reading(_) => Presence::Online,
            Self::Failed => Presence::Unreachable,
            Self::NoAccess => Presence::NoAccess,
        }
    }
}

/// Every tracked device, as the supervisor publishes it to the surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub devices: Vec<DeviceState>,
}

/// A device's presence and last-known reading, retained across polling gaps
/// and disconnects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceState {
    pub info: DeviceInfo,
    /// Last successful reading. Survives going Unreachable or Disconnected.
    pub last_reading: Option<BatteryReading>,
    /// When `last_reading` was taken. `None` if the device has never answered.
    /// `BootTime`, not `SystemTime`: the only consumer is a relative age
    /// ("2h ago"), the boot clock cannot be thrown off by an NTP step and
    /// keeps counting through suspend, and nothing here is persisted.
    pub last_seen: Option<BootTime>,
    pub presence: Presence,
    /// Remaining-time estimate derived from this device's recorded
    /// percent-change history (kept in the supervisor, not here — see
    /// `push_reading` in `app::supervisor`).
    pub estimate: Estimate,
}

impl DeviceState {
    /// Whether this device still has something to say on a status bar.
    ///
    /// An `Online` device always does. One that is not online is showing a
    /// memory, and a memory has a shelf life:
    ///
    /// - no reading at all means nothing to display. A wireless dongle stays
    ///   enumerated while its mouse is switched off, so the device is
    ///   discovered, polled and never answers — an icon for it is an empty
    ///   battery outline that has never meant anything.
    /// - a reading older than `max_age` is no longer worth a slot. "88%, two
    ///   days ago" is not a battery level, it is a fact about last Tuesday.
    ///
    /// The device stays in the roster and keeps being polled either way, so it
    /// returns the moment it answers again. This governs display only.
    ///
    /// A `NoAccess` device always is: the user has a setup problem to fix.
    pub fn is_currently_informative(&self, now: BootTime, max_age: Duration) -> bool {
        if matches!(self.presence, Presence::Online | Presence::NoAccess) {
            return true;
        }
        match (self.last_reading, self.last_seen) {
            (Some(_), Some(seen)) => now.saturating_duration_since(seen) <= max_age,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn state_at(
        presence: Presence,
        reading: Option<BatteryReading>,
        seen: Option<BootTime>,
    ) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                name: "mouse".to_owned(),
                kind: DeviceKind::Mouse,
                transport: Transport::Hidraw,
                locator: None,
            },
            last_reading: reading,
            last_seen: seen,
            presence,
            estimate: Estimate::Unknown,
        }
    }

    #[test]
    fn online_device_is_always_informative() {
        let now = BootTime::TEST_NOW;
        let state = state_at(Presence::Online, None, None);
        assert!(state.is_currently_informative(now, DAY));
    }

    #[test]
    fn offline_device_that_never_answered_is_not_informative() {
        let now = BootTime::TEST_NOW;
        for presence in [Presence::Unreachable, Presence::Disconnected] {
            let state = state_at(presence, None, None);
            assert!(!state.is_currently_informative(now, DAY));
        }
    }

    #[test]
    fn no_access_device_is_informative_without_a_reading() {
        let state = state_at(Presence::NoAccess, None, None);
        assert!(state.is_currently_informative(BootTime::TEST_NOW, DAY));
    }

    #[test]
    fn poll_outcome_maps_to_presence_and_reading() {
        let r = BatteryReading::new(40, ChargeState::Discharging);
        assert_eq!(PollOutcome::Reading(r).presence(), Presence::Online);
        assert_eq!(PollOutcome::Reading(r).reading(), Some(r));
        assert_eq!(PollOutcome::Failed.presence(), Presence::Unreachable);
        assert_eq!(PollOutcome::NoAccess.presence(), Presence::NoAccess);
        assert_eq!(PollOutcome::NoAccess.reading(), None);
    }

    /// `--json` and the dashboard IPC print this spelling.
    #[test]
    fn no_access_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(Presence::NoAccess).expect("serialize"),
            "no_access"
        );
    }

    #[test]
    fn offline_device_keeps_a_recent_reading() {
        let now = BootTime::TEST_NOW;
        let seen = now.checked_sub(Duration::from_secs(3600));
        let state = state_at(
            Presence::Unreachable,
            Some(BatteryReading::new(88, ChargeState::Discharging)),
            seen,
        );
        assert!(state.is_currently_informative(now, DAY));
    }

    #[test]
    fn offline_device_loses_a_reading_older_than_the_cap() {
        let now = BootTime::TEST_NOW;
        let seen = now.checked_sub(DAY + Duration::from_secs(1));
        let state = state_at(
            Presence::Unreachable,
            Some(BatteryReading::new(88, ChargeState::Discharging)),
            seen,
        );
        assert!(!state.is_currently_informative(now, DAY));
    }

    /// Exactly at the cap still counts: the boundary belongs to the side that
    /// keeps showing something.
    #[test]
    fn offline_device_at_exactly_the_cap_is_still_informative() {
        let now = BootTime::TEST_NOW;
        let seen = now.checked_sub(DAY);
        let state = state_at(
            Presence::Unreachable,
            Some(BatteryReading::new(88, ChargeState::Discharging)),
            seen,
        );
        assert!(state.is_currently_informative(now, DAY));
    }

    #[test]
    fn host_suspend_counts_toward_the_cap() {
        let seen = BootTime::TEST_NOW;
        let awake = 20 * 3600;
        let suspended = 5 * 3600;
        let now = seen + Duration::from_secs(awake + suspended);
        let state = state_at(
            Presence::Unreachable,
            Some(BatteryReading::new(88, ChargeState::Discharging)),
            Some(seen),
        );
        assert!(!state.is_currently_informative(now, DAY));
    }

    use super::*;

    /// Logitech ships a "G Pro" mouse and a "G Pro X" headset, so the
    /// category word has to outrank the product line.
    #[test]
    fn guess_kind_category_word_beats_product_line() {
        assert_eq!(
            guess_kind("Logitech G Pro X Wireless Headset"),
            DeviceKind::Headset
        );
        assert_eq!(guess_kind("Logitech G Pro Wireless"), DeviceKind::Mouse);
    }

    #[test]
    fn guess_kind_category_word_beats_product_line_for_keyboards() {
        assert_eq!(guess_kind("NuPhy Air75 V2-2"), DeviceKind::Keyboard);
        assert_eq!(
            guess_kind("Some Air75 Gaming Mouse"),
            DeviceKind::Mouse,
            "an explicit category word must win over a keyboard model number"
        );
    }

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
    fn guess_kind_earbuds_are_headsets() {
        for name in [
            "Nothing Ear (2)",
            "Nothing Ear",
            "AirPods Pro",
            "Galaxy Buds+",
            "Sony Headphones",
        ] {
            assert_eq!(guess_kind(name), DeviceKind::Headset, "{name}");
        }
    }

    #[test]
    fn guess_kind_ear_inside_a_word_is_not_a_headset() {
        assert_eq!(guess_kind("Nearby Keyboard"), DeviceKind::Keyboard);
        assert_eq!(guess_kind("Nearby Tracker"), DeviceKind::Other);
        assert_eq!(guess_kind("Gear Tag"), DeviceKind::Other);
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
