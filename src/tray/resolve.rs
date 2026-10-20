use crate::config::Config;
use crate::domain::{BatteryStatus, DeviceId, DeviceState, Roster, device_status};

/// The device a tray icon stands for, resolved against the current state and config.
pub(crate) struct Resolved {
    pub(crate) device: DeviceState,
    pub(crate) status: BatteryStatus,
    /// `true` when `status` classifies a retained reading from a device that
    /// is not currently `Online` — the icon should render dimmed.
    pub(super) stale: bool,
}

/// Resolves an icon's device from its key among the devices the tray shows.
///
/// Per-device icon (`key = Some(id)`): that device, if shown. A device named
/// in `hidden_devices` resolves to `None` even if `key` still names it:
/// `desired_keys` only creates per-device icons from the shown list, so a
/// keyed-but-hidden state is transient (config changed, reconcile has not yet
/// retired the icon) and showing it for one frame would be the bug.
/// Aggregate icon (`key = None`): `Roster::featured`'s pick.
pub(crate) fn resolve_for(
    key: Option<&DeviceId>,
    roster: &Roster<'_>,
    cfg: &Config,
) -> Option<Resolved> {
    let device = match key {
        Some(id) => roster
            .devices()
            .iter()
            .copied()
            .find(|d| d.info.id() == *id),
        None => roster.featured(cfg.primary_device.as_deref()),
    }?;
    let (status, stale) = device_status(device, cfg.effective_low_threshold(&device.info.name));
    Some(Resolved {
        device: device.clone(),
        status,
        stale,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Resolved;
    use crate::config::Config;
    use crate::domain::{BatteryReading, BootTime, ChargeState, DeviceId, DeviceInfo};
    use crate::domain::{BatteryStatus, DeviceState, Presence, Transport, TrayState};
    use crate::tray::fixtures::{
        cfg_with_primary, key, make_reading, make_state, no_access, retained,
        same_name_two_transports,
    };

    fn featured_id(state: &TrayState, cfg: &Config, now: BootTime) -> Option<DeviceId> {
        cfg.roster(&state.devices, now)
            .featured(cfg.primary_device.as_deref())
            .map(|d| d.info.id())
    }

    fn resolve_for(
        key: Option<&DeviceId>,
        state: &TrayState,
        cfg: &Config,
        now: BootTime,
    ) -> Option<Resolved> {
        super::resolve_for(key, &cfg.roster(&state.devices, now), cfg)
    }

    fn unreachable(reading: Option<BatteryReading>) -> TrayState {
        TrayState {
            devices: vec![
                DeviceState::test("mouse")
                    .presence(Presence::Unreachable)
                    .reading(reading)
                    .seen(reading.map(|_| crate::clock::now())),
            ],
        }
    }

    /// The pin is by name; between two devices sharing it, the aggregate icon
    /// shows the one that is answering, not whichever the roster lists first.
    #[test]
    fn featured_id_prefers_the_online_device_among_same_named_ones() {
        let state = same_name_two_transports(Presence::Online);
        let cfg = cfg_with_primary(Some("MX"));
        let featured = featured_id(&state, &cfg, crate::clock::now()).unwrap();
        assert_eq!(featured.transport, Transport::Bluetooth);
        let resolved = resolve_for(None, &state, &cfg, crate::clock::now()).unwrap();
        assert_eq!(resolved.device.last_reading, Some(make_reading(40)));
    }

    #[test]
    fn featured_id_falls_back_to_the_first_same_named_device_when_none_is_online() {
        let state = same_name_two_transports(Presence::Unreachable);
        let cfg = cfg_with_primary(Some("MX"));
        let featured = featured_id(&state, &cfg, crate::clock::now()).unwrap();
        assert_eq!(featured.transport, Transport::Sysfs);
    }

    fn featured_name(state: &TrayState, cfg: &Config, now: BootTime) -> Option<String> {
        featured_id(state, cfg, now).map(|id| id.name)
    }

    #[test]
    fn featured_name_explicit_shown_returns_that_name() {
        let state = make_state(vec![
            (DeviceInfo::test("mouse"), Some(make_reading(80))),
            (DeviceInfo::test("keyboard"), Some(make_reading(50))),
        ]);
        let cfg = cfg_with_primary(Some("keyboard"));
        assert_eq!(
            featured_name(&state, &cfg, crate::clock::now()),
            Some("keyboard".to_string())
        );
    }

    #[test]
    fn featured_name_explicit_hidden_falls_back_to_first_connected_by_name() {
        // "gamepad" is hidden, so the explicit choice is ignored.
        let mut cfg = cfg_with_primary(Some("gamepad"));
        cfg.hidden_devices = vec!["gamepad".to_string()];
        let state = make_state(vec![
            (DeviceInfo::test("mouse"), Some(make_reading(80))),
            (DeviceInfo::test("keyboard"), Some(make_reading(50))),
            (DeviceInfo::test("gamepad"), Some(make_reading(30))),
        ]);
        assert_eq!(
            featured_name(&state, &cfg, crate::clock::now()),
            Some("keyboard".to_string())
        );
    }

    #[test]
    fn featured_name_no_explicit_returns_first_connected_shown() {
        let state = make_state(vec![
            (DeviceInfo::test("mouse"), None),
            (DeviceInfo::test("keyboard"), Some(make_reading(60))),
        ]);
        let cfg = cfg_with_primary(None);
        assert_eq!(
            featured_name(&state, &cfg, crate::clock::now()),
            Some("keyboard".to_string())
        );
    }

    #[test]
    fn featured_name_no_connected_returns_first_shown() {
        let state = TrayState {
            devices: vec![
                retained("mouse", 80, Duration::from_secs(60)),
                retained("keyboard", 40, Duration::from_secs(60)),
            ],
        };
        let cfg = cfg_with_primary(None);
        // Nothing online; falls back to the first device still worth showing.
        assert_eq!(
            featured_name(&state, &cfg, crate::clock::now()),
            Some("keyboard".to_string())
        );
    }

    /// Devices that have never answered — a dongle enumerated while its mouse
    /// is switched off — have nothing to feature.
    #[test]
    fn featured_name_ignores_devices_that_never_answered() {
        let state = make_state(vec![
            (DeviceInfo::test("mouse"), None),
            (DeviceInfo::test("keyboard"), None),
        ]);
        let cfg = cfg_with_primary(None);
        assert_eq!(featured_name(&state, &cfg, crate::clock::now()), None);
    }

    /// A reading old enough to be a fact about last week is not a battery
    /// level, and stops counting as one.
    #[test]
    fn featured_name_ignores_a_reading_past_its_shelf_life() {
        let state = TrayState {
            devices: vec![retained(
                "mouse",
                80,
                Config::default().hide_offline_after() + Duration::from_secs(60),
            )],
        };
        let cfg = cfg_with_primary(None);
        assert_eq!(featured_name(&state, &cfg, crate::clock::now()), None);
    }

    #[test]
    fn featured_name_no_devices_returns_none() {
        let state = make_state(vec![]);
        let cfg = cfg_with_primary(None);
        assert_eq!(featured_name(&state, &cfg, crate::clock::now()), None);
    }

    #[test]
    fn featured_name_all_hidden_returns_none() {
        let mut cfg = cfg_with_primary(None);
        cfg.hidden_devices = vec!["mouse".to_string(), "keyboard".to_string()];
        let state = make_state(vec![
            (DeviceInfo::test("mouse"), Some(make_reading(80))),
            (DeviceInfo::test("keyboard"), Some(make_reading(50))),
        ]);
        // Both present devices are hidden.
        assert_eq!(featured_name(&state, &cfg, crate::clock::now()), None);
    }

    // --- resolve_for ----------------------------------------------------------

    #[test]
    fn resolve_for_per_device_key_present_and_shown() {
        let state = make_state(vec![(DeviceInfo::test("mouse"), Some(make_reading(80)))]);
        let cfg = cfg_with_primary(None);
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).unwrap();
        assert_eq!(resolved.device.info.name, "mouse");
        assert_eq!(resolved.device.last_reading, Some(make_reading(80)));
    }

    #[test]
    fn resolve_for_per_device_key_hidden_by_hidden_devices_returns_none() {
        let mut cfg = cfg_with_primary(None);
        cfg.hidden_devices = vec!["mouse".to_string()];
        let state = make_state(vec![(DeviceInfo::test("mouse"), Some(make_reading(80)))]);
        assert!(resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).is_none());
    }

    #[test]
    fn resolve_for_per_device_key_absent_from_state_returns_none() {
        let state = make_state(vec![(DeviceInfo::test("keyboard"), Some(make_reading(50)))]);
        let cfg = cfg_with_primary(None);
        assert!(resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).is_none());
    }

    #[test]
    fn resolve_for_aggregate_key_uses_explicit_primary_device() {
        let state = make_state(vec![
            (DeviceInfo::test("mouse"), Some(make_reading(80))),
            (DeviceInfo::test("keyboard"), Some(make_reading(50))),
        ]);
        let cfg = cfg_with_primary(Some("keyboard"));
        let resolved = resolve_for(None, &state, &cfg, crate::clock::now()).unwrap();
        assert_eq!(resolved.device.info.name, "keyboard");
    }

    #[test]
    fn resolve_for_aggregate_key_no_devices_returns_none() {
        let state = make_state(vec![]);
        let cfg = cfg_with_primary(None);
        assert!(resolve_for(None, &state, &cfg, crate::clock::now()).is_none());
    }

    #[test]
    fn resolve_for_status_uses_per_device_low_threshold_override() {
        let state = make_state(vec![(DeviceInfo::test("mouse"), Some(make_reading(15)))]);
        let mut cfg = cfg_with_primary(None);

        // Global threshold (20) would already flag 15% as Low; override it down
        // so the global default alone would report Ok, isolating the override.
        cfg.low_threshold = 5;
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).unwrap();
        assert!(matches!(resolved.status, BatteryStatus::Ok { .. }));

        cfg.device_overrides.insert(
            "mouse".to_string(),
            crate::config::DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(20),
            },
        );
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).unwrap();
        assert!(matches!(resolved.status, BatteryStatus::Low { .. }));
    }

    #[test]
    fn resolve_for_unreachable_device_with_retained_reading_classifies_and_marks_stale() {
        let cfg = cfg_with_primary(None);
        let state = unreachable(Some(make_reading(5)));
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).unwrap();
        assert!(matches!(resolved.status, BatteryStatus::Low { .. }));
        assert!(resolved.stale);
    }

    /// A wireless dongle stays enumerated while its mouse is off, so the device
    /// is discovered and polled and never answers. An empty battery outline
    /// that has never meant anything is worse than no icon.
    #[test]
    fn resolve_for_unreachable_device_with_no_reading_shows_nothing() {
        let cfg = cfg_with_primary(None);
        let state = unreachable(None);
        assert!(resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).is_none());
    }

    #[test]
    fn resolve_for_keeps_a_recent_retained_reading() {
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![retained("mouse", 88, Duration::from_secs(3600))],
        };
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).unwrap();
        assert_eq!(resolved.status, BatteryStatus::Ok { percent: 88 });
        assert!(resolved.stale);
    }

    #[test]
    fn resolve_for_drops_a_retained_reading_past_its_shelf_life() {
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![retained(
                "mouse",
                88,
                Config::default().hide_offline_after() + Duration::from_secs(1),
            )],
        };
        assert!(resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).is_none());
    }

    #[test]
    fn resolve_for_unreachable_stale_charging_never_shows_charging() {
        // A mouse retained at 80% Charging, then gone unreachable, must not render green.
        let cfg = cfg_with_primary(None);
        let state = unreachable(Some(BatteryReading::new(80, ChargeState::Charging)));
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).unwrap();
        assert_eq!(resolved.status, BatteryStatus::Ok { percent: 80 });
        assert!(resolved.stale);
    }

    #[test]
    fn resolve_for_unreachable_stale_charging_below_threshold_is_low_not_hidden() {
        // A stale Charging flag must not outrank and hide a low battery.
        let mut cfg = cfg_with_primary(None);
        cfg.low_threshold = 20;
        let state = unreachable(Some(BatteryReading::new(15, ChargeState::Charging)));
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).unwrap();
        assert_eq!(resolved.status, BatteryStatus::Low { percent: 15 });
        assert!(resolved.stale);
    }

    #[test]
    fn resolve_for_online_device_is_never_stale() {
        let state = make_state(vec![(DeviceInfo::test("mouse"), Some(make_reading(80)))]);
        let cfg = cfg_with_primary(None);
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, crate::clock::now()).unwrap();
        assert!(!resolved.stale);
    }

    #[test]
    fn featured_id_prefers_an_online_device_over_one_without_access() {
        let mut state = make_state(vec![(DeviceInfo::test("keyboard"), Some(make_reading(60)))]);
        state.devices.insert(0, no_access("mouse"));
        let cfg = cfg_with_primary(None);
        assert_eq!(
            featured_name(&state, &cfg, crate::clock::now()),
            Some("keyboard".to_string())
        );
    }
}
