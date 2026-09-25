use std::time::Duration;

use super::time::BootTime;
use super::types::{DeviceState, Presence};

/// How long a device that is not `Online` stays on a status surface after its
/// last reading.
///
/// Matches the supervisor's `DISCONNECTED_RETENTION`, so an icon never outlives
/// the roster entry behind it.
pub const RETAINED_ICON_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether a status surface shows the device: not hidden by the user, and
/// still saying something (`DeviceState::is_currently_informative`).
pub fn is_visible(device: &DeviceState, is_shown: impl Fn(&str) -> bool, now: BootTime) -> bool {
    is_shown(&device.info.name) && device.is_currently_informative(now, RETAINED_ICON_MAX_AGE)
}

/// Sort key for every device list: online devices first, then by name,
/// ignoring case. Use a stable sort so same-named devices keep their order.
pub fn roster_order(name: &str, presence: Presence) -> impl Ord + use<> {
    (presence != Presence::Online, name.to_lowercase())
}

/// The devices a status surface shows, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roster<'a>(Vec<&'a DeviceState>);

impl<'a> Roster<'a> {
    pub fn visible(
        devices: &'a [DeviceState],
        is_shown: impl Fn(&str) -> bool,
        now: BootTime,
    ) -> Self {
        let mut shown: Vec<&DeviceState> = devices
            .iter()
            .filter(|d| is_visible(d, &is_shown, now))
            .collect();
        shown.sort_by_cached_key(|d| roster_order(&d.info.name, d.presence));
        Self(shown)
    }

    pub fn devices(&self) -> &[&'a DeviceState] {
        &self.0
    }

    /// The device a single view (aggregate icon, `--waybar`) shows.
    ///
    /// The pin is by name and applies while a device of that name is visible;
    /// among devices sharing the name, or with no pin, the first in roster
    /// order wins, and that order puts online devices first.
    pub fn featured(&self, primary: Option<&str>) -> Option<&'a DeviceState> {
        primary
            .and_then(|name| self.0.iter().find(|d| d.info.name == name))
            .or_else(|| self.0.first())
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::{BatteryReading, ChargeState, DeviceInfo, DeviceKind, Transport};
    use crate::domain::{DeviceId, Estimate};

    fn device(
        name: &str,
        transport: Transport,
        presence: Presence,
        reading: Option<(u8, BootTime)>,
    ) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                name: name.to_owned(),
                kind: DeviceKind::Mouse,
                transport,
                locator: None,
            },
            last_reading: reading.map(|(p, _)| BatteryReading::new(p, ChargeState::Discharging)),
            last_seen: reading.map(|(_, seen)| seen),
            presence,
            estimate: Estimate::Unknown,
        }
    }

    fn online(name: &str, now: BootTime) -> DeviceState {
        device(name, Transport::Sysfs, Presence::Online, Some((50, now)))
    }

    fn all(_: &str) -> bool {
        true
    }

    fn names(roster: &Roster<'_>) -> Vec<String> {
        roster
            .devices()
            .iter()
            .map(|d| d.info.name.clone())
            .collect()
    }

    fn featured_id(
        devices: &[DeviceState],
        primary: Option<&str>,
        now: BootTime,
    ) -> Option<DeviceId> {
        Roster::visible(devices, all, now)
            .featured(primary)
            .map(|d| d.info.id())
    }

    // --- visibility -----------------------------------------------------------

    #[test]
    fn a_hidden_device_is_not_visible() {
        let now = BootTime::TEST_NOW;
        let mouse = online("mouse", now);
        assert!(is_visible(&mouse, all, now));
        assert!(!is_visible(&mouse, |n| n != "mouse", now));
    }

    #[test]
    fn a_device_that_never_answered_is_not_visible() {
        let now = BootTime::TEST_NOW;
        let dongle = device("dongle", Transport::Hidraw, Presence::Unreachable, None);
        assert!(!is_visible(&dongle, all, now));
    }

    #[test]
    fn a_retained_reading_is_visible_for_a_day_and_not_after() {
        let seen = BootTime::TEST_NOW;
        let keys = device(
            "keys",
            Transport::Hidraw,
            Presence::Unreachable,
            Some((88, seen)),
        );
        assert!(is_visible(&keys, all, seen + RETAINED_ICON_MAX_AGE));
        let later = seen + RETAINED_ICON_MAX_AGE + Duration::from_secs(1);
        assert!(!is_visible(&keys, all, later));
    }

    #[test]
    fn a_device_without_access_is_visible_without_a_reading() {
        let locked = device("locked", Transport::Hidraw, Presence::NoAccess, None);
        assert!(is_visible(&locked, all, BootTime::TEST_NOW));
    }

    // --- order ----------------------------------------------------------------

    #[test]
    fn roster_lists_online_devices_first_then_by_name_ignoring_case() {
        let now = BootTime::TEST_NOW;
        let devices = vec![
            device(
                "alpha",
                Transport::Sysfs,
                Presence::Unreachable,
                Some((10, now)),
            ),
            online("zebra", now),
            device("Locked", Transport::Hidraw, Presence::NoAccess, None),
            online("Mouse", now),
        ];
        let roster = Roster::visible(&devices, all, now);
        assert_eq!(names(&roster), ["Mouse", "zebra", "alpha", "Locked"]);
    }

    #[test]
    fn same_named_devices_keep_their_relative_order() {
        let now = BootTime::TEST_NOW;
        let devices = vec![
            device("MX", Transport::Sysfs, Presence::Online, Some((70, now))),
            device(
                "MX",
                Transport::Bluetooth,
                Presence::Online,
                Some((40, now)),
            ),
        ];
        let transports: Vec<Transport> = Roster::visible(&devices, all, now)
            .devices()
            .iter()
            .map(|d| d.info.transport)
            .collect();
        assert_eq!(transports, [Transport::Sysfs, Transport::Bluetooth]);
    }

    #[test]
    fn roster_leaves_out_what_is_not_visible() {
        let now = BootTime::TEST_NOW;
        let devices = vec![
            online("mouse", now),
            online("hidden", now),
            device("dongle", Transport::Hidraw, Presence::Unreachable, None),
        ];
        let roster = Roster::visible(&devices, |n| n != "hidden", now);
        assert_eq!(names(&roster), ["mouse"]);
    }

    // --- featured -------------------------------------------------------------

    #[test]
    fn featured_follows_a_visible_pin() {
        let now = BootTime::TEST_NOW;
        let devices = vec![online("mouse", now), online("keyboard", now)];
        let pick = featured_id(&devices, Some("mouse"), now).map(|id| id.name);
        assert_eq!(pick.as_deref(), Some("mouse"));
    }

    #[test]
    fn featured_ignores_a_pin_that_is_not_visible() {
        let now = BootTime::TEST_NOW;
        let devices = vec![
            online("mouse", now),
            device("dongle", Transport::Hidraw, Presence::Unreachable, None),
        ];
        let pick = featured_id(&devices, Some("dongle"), now).map(|id| id.name);
        assert_eq!(pick.as_deref(), Some("mouse"));
        let hidden = Roster::visible(&devices, |n| n != "mouse", now).featured(Some("mouse"));
        assert_eq!(hidden, None);
    }

    #[test]
    fn featured_prefers_an_online_device_over_a_sleeping_one() {
        let now = BootTime::TEST_NOW;
        let devices = vec![
            device(
                "alpha",
                Transport::Sysfs,
                Presence::Unreachable,
                Some((90, now)),
            ),
            online("zebra", now),
        ];
        let pick = featured_id(&devices, None, now).map(|id| id.name);
        assert_eq!(pick.as_deref(), Some("zebra"));
    }

    #[test]
    fn featured_picks_the_online_copy_of_a_duplicate_name() {
        let now = BootTime::TEST_NOW;
        let devices = vec![
            device(
                "MX",
                Transport::Sysfs,
                Presence::Unreachable,
                Some((70, now)),
            ),
            device(
                "MX",
                Transport::Bluetooth,
                Presence::Online,
                Some((40, now)),
            ),
        ];
        for primary in [None, Some("MX")] {
            let pick = featured_id(&devices, primary, now).map(|id| id.transport);
            assert_eq!(pick, Some(Transport::Bluetooth), "pin {primary:?}");
        }
    }

    #[test]
    fn with_nothing_online_featured_skips_a_dongle_without_a_reading() {
        let now = BootTime::TEST_NOW;
        let dongle = device("a-dongle", Transport::Hidraw, Presence::Unreachable, None);
        let keys = device(
            "keys",
            Transport::Hidraw,
            Presence::Unreachable,
            Some((88, now)),
        );
        let pick = featured_id(&[dongle.clone(), keys], None, now).map(|id| id.name);
        assert_eq!(pick.as_deref(), Some("keys"));
        assert_eq!(featured_id(&[dongle], None, now), None);
    }
}
