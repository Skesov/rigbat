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

impl DeviceKind {
    pub const ALL: [DeviceKind; 5] = [
        DeviceKind::Mouse,
        DeviceKind::Keyboard,
        DeviceKind::Headset,
        DeviceKind::Controller,
        DeviceKind::Other,
    ];

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

impl ChargeState {
    pub const ALL: [ChargeState; 3] = [
        ChargeState::Discharging,
        ChargeState::Charging,
        ChargeState::Full,
    ];
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
    pub const ALL: [Transport; 3] = [Transport::Sysfs, Transport::Bluetooth, Transport::Hidraw];

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
    /// `push_history_point` in `app::supervisor`).
    pub estimate: Estimate,
}

impl DeviceState {
    /// Whether this device still has something to say on a status bar. An
    /// `Online` or `NoAccess` device always does; any other needs a reading
    /// no older than `max_age`:
    ///
    /// - a wireless dongle stays enumerated while its mouse is off, and never
    ///   answers: an icon for it is an empty outline that never meant anything.
    /// - "88%, two days ago" is not a battery level.
    ///
    /// Display only: the device stays in the roster and keeps being polled.
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
        DeviceState::test("mouse")
            .transport(Transport::Hidraw)
            .presence(presence)
            .reading(reading)
            .seen(seen)
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

    #[test]
    fn freedesktop_icon_names() {
        for (kind, icon) in [
            (DeviceKind::Mouse, "input-mouse"),
            (DeviceKind::Keyboard, "input-keyboard"),
            (DeviceKind::Headset, "audio-headset"),
            (DeviceKind::Controller, "input-gaming"),
            (DeviceKind::Other, "battery"),
        ] {
            assert_eq!(freedesktop_icon_name(kind), icon);
        }
    }

    #[test]
    fn battery_reading_clamps_over_100() {
        let r = BatteryReading::new(150, ChargeState::Discharging);
        assert_eq!(r.percent, 100);
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
