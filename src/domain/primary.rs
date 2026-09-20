use super::types::{BatteryReading, ChargeState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryStatus {
    Offline,
    Ok { percent: u8 },
    Low { percent: u8 },
    Charging { percent: u8 },
}

/// Classifies a reading for a primary device.
/// `reading == None` means offline (device not responding).
/// Priority order: Offline → Charging → Low → Ok.
pub fn classify(reading: Option<BatteryReading>, low_threshold: u8) -> PrimaryStatus {
    match reading {
        None => PrimaryStatus::Offline,
        Some(r) => match r.state {
            ChargeState::Charging => PrimaryStatus::Charging { percent: r.percent },
            _ if r.percent <= low_threshold => PrimaryStatus::Low { percent: r.percent },
            _ => PrimaryStatus::Ok { percent: r.percent },
        },
    }
}

/// Classifies a reading whose liveness is no longer known: the percentage is a
/// measurement that ages gracefully, the charge state is a live condition that
/// does not. Used for a device that is no longer `Online` but still holds a
/// reading — see `tray::manager::resolve_for`.
pub fn classify_stale(percent: u8, low_threshold: u8) -> PrimaryStatus {
    if percent <= low_threshold {
        PrimaryStatus::Low { percent }
    } else {
        PrimaryStatus::Ok { percent }
    }
}

/// Picks the featured device by name from an already-shown-filtered roster.
///
/// `shown` is `(name, online)` pairs; `online` is the tiebreak signal used
/// when there is no explicit choice — `Presence::Online` for the tray, but
/// callers with no presence information (the one-shot CLI path) can pass any
/// other "prefer this one" signal, such as "this poll returned a reading".
///
/// Priority: explicit choice (if still shown) → first online → first shown → None.
/// It lives in `domain`, not in `tray`, because three surfaces answer this
/// same question — the aggregate tray icon, `--waybar`, and the settings
/// window's description of what the single icon will show — so the policy
/// cannot belong to any one of them. Pure: names and flags in, a name out.
pub fn select_featured(shown: &[(&str, bool)], primary_device: Option<&str>) -> Option<String> {
    if let Some(name) = primary_device
        && shown.iter().any(|(n, _)| *n == name)
    {
        return Some(name.to_string());
    }

    shown
        .iter()
        .find(|(_, online)| *online)
        .or_else(|| shown.first())
        .map(|(name, _)| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::{BatteryReading, ChargeState};

    #[test]
    fn none_is_offline() {
        assert_eq!(classify(None, 20), PrimaryStatus::Offline);
    }

    #[test]
    fn charging_above_threshold_is_charging() {
        let r = BatteryReading::new(80, ChargeState::Charging);
        assert_eq!(
            classify(Some(r), 20),
            PrimaryStatus::Charging { percent: 80 }
        );
    }

    #[test]
    fn charging_below_threshold_is_charging_not_low() {
        let r = BatteryReading::new(10, ChargeState::Charging);
        assert_eq!(
            classify(Some(r), 20),
            PrimaryStatus::Charging { percent: 10 }
        );
    }

    #[test]
    fn discharging_below_threshold_is_low() {
        let r = BatteryReading::new(15, ChargeState::Discharging);
        assert_eq!(classify(Some(r), 20), PrimaryStatus::Low { percent: 15 });
    }

    #[test]
    fn discharging_above_threshold_is_ok() {
        let r = BatteryReading::new(50, ChargeState::Discharging);
        assert_eq!(classify(Some(r), 20), PrimaryStatus::Ok { percent: 50 });
    }

    #[test]
    fn at_threshold_is_low() {
        let r = BatteryReading::new(20, ChargeState::Discharging);
        assert_eq!(classify(Some(r), 20), PrimaryStatus::Low { percent: 20 });
    }

    #[test]
    fn full_high_percent_is_ok() {
        let r = BatteryReading::new(100, ChargeState::Full);
        assert_eq!(classify(Some(r), 20), PrimaryStatus::Ok { percent: 100 });
    }

    #[test]
    fn battery_reading_clamps_over_100() {
        let r = BatteryReading::new(150, ChargeState::Discharging);
        assert_eq!(r.percent, 100);
    }

    #[test]
    fn classify_stale_below_threshold_is_low() {
        assert_eq!(classify_stale(15, 20), PrimaryStatus::Low { percent: 15 });
    }

    #[test]
    fn classify_stale_above_threshold_is_ok() {
        assert_eq!(classify_stale(80, 20), PrimaryStatus::Ok { percent: 80 });
    }

    #[test]
    fn classify_stale_at_threshold_is_low() {
        assert_eq!(classify_stale(20, 20), PrimaryStatus::Low { percent: 20 });
    }

    #[test]
    fn classify_stale_ignores_stored_charging_state() {
        // classify_stale takes a bare percent, not a ChargeState, so a
        // percentage that was stored alongside Charging still comes out
        // Low/Ok — never Charging.
        assert_eq!(classify_stale(80, 20), PrimaryStatus::Ok { percent: 80 });
        assert_eq!(classify_stale(15, 20), PrimaryStatus::Low { percent: 15 });
    }

    // --- select_featured ------------------------------------------------------

    #[test]
    fn select_featured_explicit_choice_wins_when_shown() {
        let shown = [("mouse", true), ("keyboard", false)];
        assert_eq!(
            select_featured(&shown, Some("keyboard")),
            Some("keyboard".to_string())
        );
    }

    #[test]
    fn select_featured_explicit_choice_ignored_when_not_shown() {
        let shown = [("mouse", true), ("keyboard", false)];
        assert_eq!(
            select_featured(&shown, Some("gamepad")),
            Some("mouse".to_string())
        );
    }

    #[test]
    fn select_featured_no_choice_prefers_online() {
        let shown = [("mouse", false), ("keyboard", true)];
        assert_eq!(select_featured(&shown, None), Some("keyboard".to_string()));
    }

    #[test]
    fn select_featured_no_online_falls_back_to_first() {
        let shown = [("mouse", false), ("keyboard", false)];
        assert_eq!(select_featured(&shown, None), Some("mouse".to_string()));
    }

    #[test]
    fn select_featured_empty_returns_none() {
        assert_eq!(select_featured(&[], None), None);
    }
}
