pub mod icon;
pub mod manager;

use std::time::{Duration, Instant};

use crate::domain::estimate::format_coarse;
use crate::domain::{ChargeState, DeviceState, Estimate, Presence};

fn state_str(state: ChargeState) -> &'static str {
    match state {
        ChargeState::Charging => "charging",
        ChargeState::Discharging => "discharging",
        ChargeState::Full => "full",
    }
}

/// Formats a duration as a coarse, human-scale age string. Coarse units only
/// (never raw seconds) — this reads in a tooltip, not a log.
pub fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        "just now".to_owned()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Formats a device entry string for a menu item or tooltip.
///
/// `Online` renders the live reading. `Unreachable`/`Disconnected` render the
/// retained reading with its age (e.g. "88%  offline (2h ago)"), or plain
/// "offline" when there is nothing retained. `now` is a parameter, not
/// `Instant::now()` inside the function, so callers can render deterministically.
pub fn format_device_entry(state: &DeviceState, now: Instant) -> String {
    let name = &state.info.name;

    if state.presence == Presence::Online {
        return match state.last_reading {
            Some(r) => {
                let base = format!("{name}: {}%  {}", r.percent, state_str(r.state));
                match state.estimate {
                    Estimate::Remaining(d) => format!("{base}  {} left", format_coarse(d)),
                    Estimate::Unknown | Estimate::Charging => base,
                }
            }
            None => format!("{name}: offline"),
        };
    }

    match (state.last_reading, state.last_seen) {
        (Some(r), Some(seen)) => format!(
            "{name}: {}%  offline ({})",
            r.percent,
            format_age(now.duration_since(seen))
        ),
        _ => format!("{name}: offline"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            kind: DeviceKind::Other,
            transport: crate::domain::Transport::Sysfs,
            locator: None,
        }
    }

    fn state(
        name: &str,
        presence: Presence,
        last_reading: Option<BatteryReading>,
        last_seen: Option<Instant>,
    ) -> DeviceState {
        DeviceState {
            info: device(name),
            last_reading,
            last_seen,
            presence,
            estimate: Estimate::Unknown,
        }
    }

    fn state_with_estimate(
        name: &str,
        last_reading: Option<BatteryReading>,
        estimate: Estimate,
    ) -> DeviceState {
        DeviceState {
            info: device(name),
            last_reading,
            last_seen: Some(Instant::now()),
            presence: Presence::Online,
            estimate,
        }
    }

    // --- format_age -------------------------------------------------------

    #[test]
    fn format_age_under_a_minute_is_just_now() {
        assert_eq!(format_age(Duration::from_secs(0)), "just now");
        assert_eq!(format_age(Duration::from_secs(59)), "just now");
    }

    #[test]
    fn format_age_minutes() {
        assert_eq!(format_age(Duration::from_secs(60)), "1m ago");
        assert_eq!(format_age(Duration::from_secs(59 * 60)), "59m ago");
    }

    #[test]
    fn format_age_hours() {
        assert_eq!(format_age(Duration::from_secs(3600)), "1h ago");
        assert_eq!(format_age(Duration::from_secs(23 * 3600)), "23h ago");
    }

    #[test]
    fn format_age_days() {
        assert_eq!(format_age(Duration::from_secs(86400)), "1d ago");
        assert_eq!(format_age(Duration::from_secs(3 * 86400)), "3d ago");
    }

    // --- format_device_entry ------------------------------------------------

    #[test]
    fn format_device_entry_online_offline_reading() {
        let now = Instant::now();
        assert_eq!(
            format_device_entry(&state("mouse", Presence::Online, None, None), now),
            "mouse: offline"
        );
    }

    #[test]
    fn format_device_entry_online_discharging() {
        let now = Instant::now();
        let r = BatteryReading::new(75, ChargeState::Discharging);
        assert_eq!(
            format_device_entry(
                &state("keyboard", Presence::Online, Some(r), Some(now)),
                now
            ),
            "keyboard: 75%  discharging"
        );
    }

    #[test]
    fn format_device_entry_online_charging() {
        let now = Instant::now();
        let r = BatteryReading::new(42, ChargeState::Charging);
        assert_eq!(
            format_device_entry(&state("headset", Presence::Online, Some(r), Some(now)), now),
            "headset: 42%  charging"
        );
    }

    #[test]
    fn format_device_entry_online_full() {
        let now = Instant::now();
        let r = BatteryReading::new(100, ChargeState::Full);
        assert_eq!(
            format_device_entry(
                &state("controller", Presence::Online, Some(r), Some(now)),
                now
            ),
            "controller: 100%  full"
        );
    }

    #[test]
    fn format_device_entry_retained_reading_shows_age() {
        let seen = Instant::now();
        let now = seen + Duration::from_secs(2 * 3600);
        let r = BatteryReading::new(88, ChargeState::Discharging);
        assert_eq!(
            format_device_entry(
                &state("NuPhy Air75", Presence::Disconnected, Some(r), Some(seen)),
                now
            ),
            "NuPhy Air75: 88%  offline (2h ago)"
        );
    }

    #[test]
    fn format_device_entry_unreachable_with_retained_reading() {
        let seen = Instant::now();
        let now = seen + Duration::from_secs(300);
        let r = BatteryReading::new(50, ChargeState::Discharging);
        assert_eq!(
            format_device_entry(
                &state("mouse", Presence::Unreachable, Some(r), Some(seen)),
                now
            ),
            "mouse: 50%  offline (5m ago)"
        );
    }

    #[test]
    fn format_device_entry_appends_remaining_when_estimate_is_remaining() {
        let r = BatteryReading::new(62, ChargeState::Discharging);
        let s = state_with_estimate(
            "MX Anywhere 3",
            Some(r),
            Estimate::Remaining(Duration::from_secs(7 * 3600)),
        );
        assert_eq!(
            format_device_entry(&s, Instant::now()),
            "MX Anywhere 3: 62%  discharging  ~7h left"
        );
    }

    #[test]
    fn format_device_entry_appends_nothing_when_estimate_is_unknown() {
        let r = BatteryReading::new(62, ChargeState::Discharging);
        let s = state_with_estimate("mouse", Some(r), Estimate::Unknown);
        assert_eq!(
            format_device_entry(&s, Instant::now()),
            "mouse: 62%  discharging"
        );
    }

    #[test]
    fn format_device_entry_appends_nothing_when_estimate_is_charging() {
        let r = BatteryReading::new(62, ChargeState::Charging);
        let s = state_with_estimate("mouse", Some(r), Estimate::Charging);
        assert_eq!(
            format_device_entry(&s, Instant::now()),
            "mouse: 62%  charging"
        );
    }

    #[test]
    fn format_device_entry_disconnected_without_reading() {
        let now = Instant::now();
        assert_eq!(
            format_device_entry(&state("gamepad", Presence::Disconnected, None, None), now),
            "gamepad: offline"
        );
    }
}
