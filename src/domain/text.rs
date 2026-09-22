//! Human-facing text for a device's state: the charge-state word, a coarse
//! age, and the one-line entry the tray menu, the CLI table and the settings
//! window all render.
//!
//! In `domain` rather than in `tray` because three surfaces render the same
//! sentence and none of them owns it: when it lived in `tray`, both `cli` and
//! `settings` imported one adapter from another, which is the one direction
//! the layering forbids. Pure — `now` and `lang` are always parameters.

use std::time::{Duration, Instant};

use super::estimate::format_coarse;
use super::{ChargeState, DeviceState, Estimate, Presence};
use crate::i18n::{Lang, fl, loader};

/// Wire value for `--json` and `list`; never translated (see `state_label`).
pub fn state_str(state: ChargeState) -> &'static str {
    match state {
        ChargeState::Charging => "charging",
        ChargeState::Discharging => "discharging",
        ChargeState::Full => "full",
    }
}

pub fn state_label(state: ChargeState, lang: Lang) -> String {
    let l = loader(lang);
    match state {
        ChargeState::Charging => fl!(l, "state-charging"),
        ChargeState::Discharging => fl!(l, "state-discharging"),
        ChargeState::Full => fl!(l, "state-full"),
    }
}

/// Formats a duration as a coarse, human-scale age string. Coarse units only
/// (never raw seconds) — this reads in a tooltip, not a log.
pub fn format_age(age: Duration, lang: Lang) -> String {
    let l = loader(lang);
    // Counts go through locals: `fl!` would parse `count = secs / 60` as `secs / 60.into()`.
    let secs = age.as_secs();
    if secs < 60 {
        fl!(l, "age-just-now")
    } else if secs < 3600 {
        let count = secs / 60;
        fl!(l, "age-minutes", count = count)
    } else if secs < 86400 {
        let count = secs / 3600;
        fl!(l, "age-hours", count = count)
    } else {
        let count = secs / 86400;
        fl!(l, "age-days", count = count)
    }
}

/// Formats a device entry string for a menu item or tooltip.
///
/// `Online` renders the live reading. `Unreachable`/`Disconnected` render the
/// retained reading with its age (e.g. "88%  offline (2h ago)"), or plain
/// "offline" when there is nothing retained. `now` is a parameter, not
/// `Instant::now()` inside the function, so callers can render deterministically.
pub fn format_device_entry(state: &DeviceState, now: Instant, lang: Lang) -> String {
    let l = loader(lang);
    let name = state.info.name.as_str();

    if state.presence == Presence::Online {
        return match state.last_reading {
            Some(r) => {
                let percent = r.percent;
                let charge = state_label(r.state, lang);
                let charge = charge.as_str();
                match state.estimate {
                    Estimate::Remaining(d) => {
                        let estimate = format_coarse(d, lang);
                        let estimate = estimate.as_str();
                        fl!(
                            l,
                            "entry-online-estimate",
                            name = name,
                            percent = percent,
                            state = charge,
                            estimate = estimate
                        )
                    }
                    Estimate::Unknown | Estimate::Charging => fl!(
                        l,
                        "entry-online",
                        name = name,
                        percent = percent,
                        state = charge
                    ),
                }
            }
            None => fl!(l, "entry-offline", name = name),
        };
    }

    match (state.last_reading, state.last_seen) {
        (Some(r), Some(seen)) => {
            let percent = r.percent;
            let age = format_age(now.duration_since(seen), lang);
            let age = age.as_str();
            fl!(
                l,
                "entry-offline-retained",
                name = name,
                percent = percent,
                age = age
            )
        }
        _ => fl!(l, "entry-offline", name = name),
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
        assert_eq!(format_age(Duration::from_secs(0), Lang::En), "just now");
        assert_eq!(format_age(Duration::from_secs(59), Lang::En), "just now");
    }

    #[test]
    fn format_age_minutes() {
        assert_eq!(format_age(Duration::from_secs(60), Lang::En), "1m ago");
        assert_eq!(
            format_age(Duration::from_secs(59 * 60), Lang::En),
            "59m ago"
        );
    }

    #[test]
    fn format_age_hours() {
        assert_eq!(format_age(Duration::from_secs(3600), Lang::En), "1h ago");
        assert_eq!(
            format_age(Duration::from_secs(23 * 3600), Lang::En),
            "23h ago"
        );
    }

    #[test]
    fn format_age_days() {
        assert_eq!(format_age(Duration::from_secs(86400), Lang::En), "1d ago");
        assert_eq!(
            format_age(Duration::from_secs(3 * 86400), Lang::En),
            "3d ago"
        );
    }

    // --- format_device_entry ------------------------------------------------

    #[test]
    fn format_device_entry_online_offline_reading() {
        let now = Instant::now();
        assert_eq!(
            format_device_entry(&state("mouse", Presence::Online, None, None), now, Lang::En),
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
                now,
                Lang::En
            ),
            "keyboard: 75%  discharging"
        );
    }

    #[test]
    fn format_device_entry_online_charging() {
        let now = Instant::now();
        let r = BatteryReading::new(42, ChargeState::Charging);
        assert_eq!(
            format_device_entry(
                &state("headset", Presence::Online, Some(r), Some(now)),
                now,
                Lang::En
            ),
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
                now,
                Lang::En
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
                now,
                Lang::En
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
                now,
                Lang::En
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
            format_device_entry(&s, Instant::now(), Lang::En),
            "MX Anywhere 3: 62%  discharging  ~7h left"
        );
    }

    #[test]
    fn format_device_entry_appends_nothing_when_estimate_is_unknown() {
        let r = BatteryReading::new(62, ChargeState::Discharging);
        let s = state_with_estimate("mouse", Some(r), Estimate::Unknown);
        assert_eq!(
            format_device_entry(&s, Instant::now(), Lang::En),
            "mouse: 62%  discharging"
        );
    }

    #[test]
    fn format_device_entry_appends_nothing_when_estimate_is_charging() {
        let r = BatteryReading::new(62, ChargeState::Charging);
        let s = state_with_estimate("mouse", Some(r), Estimate::Charging);
        assert_eq!(
            format_device_entry(&s, Instant::now(), Lang::En),
            "mouse: 62%  charging"
        );
    }

    #[test]
    fn format_device_entry_disconnected_without_reading() {
        let now = Instant::now();
        assert_eq!(
            format_device_entry(
                &state("gamepad", Presence::Disconnected, None, None),
                now,
                Lang::En
            ),
            "gamepad: offline"
        );
    }

    // --- Russian ------------------------------------------------------------

    #[test]
    fn russian_entries_render_every_shape() {
        let seen = Instant::now();
        let now = seen + Duration::from_secs(2 * 3600);
        let r = BatteryReading::new(88, ChargeState::Discharging);
        let remaining = state_with_estimate(
            "MX Anywhere 3",
            Some(BatteryReading::new(62, ChargeState::Discharging)),
            Estimate::Remaining(Duration::from_secs(7 * 3600)),
        );
        for (entry, expected) in [
            (
                format_device_entry(
                    &state("mouse", Presence::Online, Some(r), Some(seen)),
                    seen,
                    Lang::Ru,
                ),
                "mouse: 88%  разряжается",
            ),
            (
                format_device_entry(&remaining, Instant::now(), Lang::Ru),
                "MX Anywhere 3: 62%  разряжается  осталось ~7 ч",
            ),
            (
                format_device_entry(
                    &state("mouse", Presence::Disconnected, Some(r), Some(seen)),
                    now,
                    Lang::Ru,
                ),
                "mouse: 88%  не на связи (2 ч назад)",
            ),
            (
                format_device_entry(
                    &state("gamepad", Presence::Disconnected, None, None),
                    now,
                    Lang::Ru,
                ),
                "gamepad: не на связи",
            ),
        ] {
            assert_eq!(entry, expected);
        }
    }

    #[test]
    fn russian_ages_use_abbreviated_units() {
        assert_eq!(format_age(Duration::from_secs(5), Lang::Ru), "только что");
        assert_eq!(
            format_age(Duration::from_secs(5 * 60), Lang::Ru),
            "5 мин назад"
        );
        assert_eq!(
            format_age(Duration::from_secs(21 * 3600), Lang::Ru),
            "21 ч назад"
        );
        assert_eq!(
            format_age(Duration::from_secs(3 * 86400), Lang::Ru),
            "3 д назад"
        );
    }

    #[test]
    fn state_label_translates_but_state_str_does_not() {
        assert_eq!(state_label(ChargeState::Full, Lang::Ru), "заряжено");
        assert_eq!(state_label(ChargeState::Full, Lang::En), "full");
        assert_eq!(state_str(ChargeState::Full), "full");
    }
}
