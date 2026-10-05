//! Human-facing text for a device's state: the charge-state word, a coarse
//! age, the value and note the dashboard and the Devices tab render, the line
//! the tray menu, its tooltip and the `--waybar` tooltip render.
//!
//! In `domain` rather than in any one surface because several render the same
//! words and none of them owns them: an adapter importing another adapter is
//! the one direction the layering forbids. Pure — `now` and `lang` are always
//! parameters.

use std::time::Duration;

use super::estimate::format_coarse;
use super::{BootTime, ChargeState, DeviceState, Estimate, Presence, PrimaryStatus};
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
/// The smallest unit `format_age` shows: text that ages changes at most this often.
pub const AGE_STEP: Duration = Duration::from_secs(60);

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

pub const CHARGING_SIGN: char = '\u{26A1}';
pub const LOW_SIGN: char = '\u{26A0}';

/// Where a value is shown, which decides the case of a presence word: one
/// that starts its own slot is capitalised, one after `name: ` is not.
#[derive(Clone, Copy)]
enum Slot {
    Own,
    AfterName,
}

/// The charge as one short value in its own slot: the dashboard's and the
/// Devices tab's right-hand column. A low level carries a sign as well as a
/// color.
pub fn charge_value(
    presence: Presence,
    percent: Option<u8>,
    charge: Option<ChargeState>,
    status: PrimaryStatus,
    lang: Lang,
) -> String {
    value_text(presence, percent, charge, status, lang, Slot::Own)
}

fn value_text(
    presence: Presence,
    percent: Option<u8>,
    charge: Option<ChargeState>,
    status: PrimaryStatus,
    lang: Lang,
    slot: Slot,
) -> String {
    let text = match (presence, percent) {
        (Presence::Online, None) => "—".to_owned(),
        (Presence::Online, Some(p)) => match (status, charge) {
            (PrimaryStatus::Charging { .. }, _) => format!("{CHARGING_SIGN} {p}%"),
            (_, Some(ChargeState::Full)) => {
                format!("{p}% · {}", state_label(ChargeState::Full, lang))
            }
            _ => format!("{p}%"),
        },
        (presence, _) => match slot {
            Slot::Own => capitalised(&presence_label(presence, lang)),
            Slot::AfterName => presence_label(presence, lang),
        },
    };
    if matches!(status, PrimaryStatus::Low { .. }) {
        format!("{LOW_SIGN} {text}")
    } else {
        text
    }
}

fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn presence_label(presence: Presence, lang: Lang) -> String {
    let l = loader(lang);
    match presence {
        Presence::Online => fl!(l, "presence-online"),
        Presence::Unreachable => fl!(l, "presence-unreachable"),
        Presence::Disconnected => fl!(l, "presence-disconnected"),
        Presence::NoAccess => fl!(l, "presence-no-access"),
    }
}

/// Only what `charge_value` does not already say.
pub fn status_note(
    presence: Presence,
    remaining: Option<Duration>,
    seen_ago: Option<Duration>,
    lang: Lang,
) -> Option<String> {
    let l = loader(lang);
    match presence {
        Presence::NoAccess => Some(fl!(l, "note-no-access")),
        Presence::Online => remaining.map(|left| {
            let estimate = format_coarse(left, lang);
            fl!(l, "note-remaining", estimate = estimate.as_str())
        }),
        Presence::Unreachable | Presence::Disconnected => seen_ago.map(|ago| {
            let age = format_age(ago, lang);
            fl!(l, "note-last-reading", age = age.as_str())
        }),
    }
}

/// A tray menu row and tooltip: name, the value, then `status_note`.
pub fn device_line(
    device: &DeviceState,
    status: PrimaryStatus,
    now: BootTime,
    lang: Lang,
) -> String {
    let reading = device.last_reading;
    let value = value_text(
        device.presence,
        reading.map(|r| r.percent),
        reading.map(|r| r.state),
        status,
        lang,
        Slot::AfterName,
    );
    let remaining = match device.estimate {
        Estimate::Remaining(left) => Some(left),
        Estimate::Unknown => None,
    };
    let seen_ago = device
        .last_seen
        .map(|seen| now.saturating_duration_since(seen));
    let name = &device.info.name;
    match status_note(device.presence, remaining, seen_ago, lang) {
        Some(note) => format!("{name}: {value} · {note}"),
        None => format!("{name}: {value}"),
    }
}

/// The tooltip of a per-device icon whose device has left the roster.
pub fn absent_line(name: &str, lang: Lang) -> String {
    let word = presence_label(Presence::Disconnected, lang);
    format!("{name}: {word}")
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
        last_seen: Option<BootTime>,
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
            last_seen: Some(BootTime::TEST_NOW),
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
        assert_eq!(
            format_age(Duration::from_secs(60), Lang::En),
            "1\u{a0}min ago"
        );
        assert_eq!(
            format_age(Duration::from_secs(59 * 60), Lang::En),
            "59\u{a0}min ago"
        );
    }

    #[test]
    fn format_age_hours() {
        assert_eq!(
            format_age(Duration::from_secs(3600), Lang::En),
            "1\u{a0}h ago"
        );
        assert_eq!(
            format_age(Duration::from_secs(23 * 3600), Lang::En),
            "23\u{a0}h ago"
        );
    }

    #[test]
    fn format_age_days() {
        assert_eq!(
            format_age(Duration::from_secs(86400), Lang::En),
            "1\u{a0}d ago"
        );
        assert_eq!(
            format_age(Duration::from_secs(3 * 86400), Lang::En),
            "3\u{a0}d ago"
        );
    }

    // --- Russian ------------------------------------------------------------

    #[test]
    fn russian_ages_take_the_plural_form_of_their_count() {
        let age = |secs| format_age(Duration::from_secs(secs), Lang::Ru);
        assert_eq!(age(5), "только что");
        for (count, minutes, hours, days) in [
            (1, "1\u{a0}минуту", "1\u{a0}час", "1\u{a0}день"),
            (2, "2\u{a0}минуты", "2\u{a0}часа", "2\u{a0}дня"),
            (5, "5\u{a0}минут", "5\u{a0}часов", "5\u{a0}дней"),
            (21, "21\u{a0}минуту", "21\u{a0}час", "21\u{a0}день"),
        ] {
            assert_eq!(age(count * 60), format!("{minutes} назад"));
            assert_eq!(age(count * 3600), format!("{hours} назад"));
            assert_eq!(age(count * 86400), format!("{days} назад"));
        }
        let l = loader(Lang::Ru);
        assert_eq!(fl!(l, "age-minutes", count = 0), "0\u{a0}минут назад");
        assert_eq!(fl!(l, "age-days", count = 0), "0\u{a0}дней назад");
    }

    #[test]
    fn a_number_never_parts_from_its_unit() {
        for lang in Lang::ALL {
            for secs in [90, 2 * 3600, 3 * 86400] {
                let age = format_age(Duration::from_secs(secs), lang);
                let (number, _) = age.split_once('\u{a0}').expect("number, NBSP, unit");
                assert!(number.chars().all(|c| c.is_ascii_digit()), "{age:?}");
            }
        }
    }

    // --- device_line ---------------------------------------------------------

    fn line(device: &DeviceState, now: BootTime, lang: Lang) -> String {
        let (status, _) = crate::domain::device_status(device, 20);
        device_line(device, status, now, lang)
    }

    #[test]
    fn device_line_reads_every_state_in_one_wording() {
        let seen = BootTime::TEST_NOW;
        let now = seen + Duration::from_secs(2 * 3600);
        let reading = |percent, state| Some(BatteryReading::new(percent, state));
        let discharging = reading(75, ChargeState::Discharging);
        let estimate = state_with_estimate(
            "MX Anywhere 3",
            reading(62, ChargeState::Discharging),
            Estimate::Remaining(Duration::from_secs(7 * 3600)),
        );
        let cases = [
            (
                state("kb", Presence::Online, discharging, Some(now)),
                "kb: 75%",
                "kb: 75%",
            ),
            (
                state(
                    "ear",
                    Presence::Online,
                    reading(42, ChargeState::Charging),
                    Some(now),
                ),
                "ear: \u{26A1} 42%",
                "ear: \u{26A1} 42%",
            ),
            (
                state(
                    "pad",
                    Presence::Online,
                    reading(100, ChargeState::Full),
                    Some(now),
                ),
                "pad: 100% · full",
                "pad: 100% · заряжено",
            ),
            (
                state(
                    "mouse",
                    Presence::Online,
                    reading(5, ChargeState::Discharging),
                    Some(now),
                ),
                "mouse: \u{26A0} 5%",
                "mouse: \u{26A0} 5%",
            ),
            (
                state("mouse", Presence::Online, None, None),
                "mouse: —",
                "mouse: —",
            ),
            (
                state("NuPhy", Presence::Unreachable, discharging, Some(seen)),
                "NuPhy: unreachable · last reading 2\u{a0}h ago",
                "NuPhy: недоступно · последние данные 2\u{a0}часа назад",
            ),
            (
                state("NuPhy", Presence::Disconnected, discharging, Some(seen)),
                "NuPhy: disconnected · last reading 2\u{a0}h ago",
                "NuPhy: отключено · последние данные 2\u{a0}часа назад",
            ),
            (
                state("pad", Presence::Disconnected, None, None),
                "pad: disconnected",
                "pad: отключено",
            ),
            (
                state("mouse", Presence::NoAccess, discharging, Some(seen)),
                "mouse: no access · run rigbat doctor",
                "mouse: нет доступа · запустите rigbat doctor",
            ),
        ];
        for (device, en, ru) in cases {
            assert_eq!(line(&device, now, Lang::En), en);
            assert_eq!(line(&device, now, Lang::Ru), ru);
        }
        let estimate = line(&estimate, BootTime::TEST_NOW, Lang::En);
        assert_eq!(estimate, "MX Anywhere 3: 62% · ~7\u{a0}h left");
    }

    #[test]
    fn an_absent_device_reads_disconnected() {
        assert_eq!(absent_line("pad", Lang::En), "pad: disconnected");
        assert_eq!(absent_line("pad", Lang::Ru), "pad: отключено");
    }

    // --- presence words ------------------------------------------------------

    #[test]
    fn a_presence_word_is_capitalised_in_its_own_slot_and_not_after_a_name() {
        let seen = BootTime::TEST_NOW;
        let now = seen + Duration::from_secs(2 * 3600);
        let r = BatteryReading::new(88, ChargeState::Discharging);
        let unreachable = state("NuPhy", Presence::Unreachable, Some(r), Some(seen));
        let denied = state("mouse", Presence::NoAccess, None, None);
        let offline = PrimaryStatus::Offline;
        let low = PrimaryStatus::Low { percent: 5 };
        for (lang, text, expected) in [
            (
                Lang::En,
                charge_value(Presence::Unreachable, Some(88), None, offline, Lang::En),
                "Unreachable",
            ),
            (
                Lang::En,
                charge_value(Presence::Disconnected, Some(5), None, low, Lang::En),
                "\u{26A0} Disconnected",
            ),
            (
                Lang::En,
                charge_value(
                    Presence::Online,
                    Some(100),
                    Some(ChargeState::Full),
                    PrimaryStatus::Ok { percent: 100 },
                    Lang::En,
                ),
                "100% · full",
            ),
            (
                Lang::Ru,
                charge_value(Presence::NoAccess, None, None, offline, Lang::Ru),
                "Нет доступа",
            ),
            (
                Lang::En,
                device_line(&denied, offline, now, Lang::En),
                "mouse: no access · run rigbat doctor",
            ),
            (
                Lang::En,
                device_line(&unreachable, offline, now, Lang::En),
                "NuPhy: unreachable · last reading 2\u{a0}h ago",
            ),
            (
                Lang::Ru,
                device_line(&denied, offline, now, Lang::Ru),
                "mouse: нет доступа · запустите rigbat doctor",
            ),
            (
                Lang::Ru,
                device_line(&unreachable, low, now, Lang::Ru),
                "NuPhy: \u{26A0} недоступно · последние данные 2\u{a0}часа назад",
            ),
        ] {
            assert_eq!(text, expected, "{lang:?}");
        }
    }

    #[test]
    fn state_label_translates_but_state_str_does_not() {
        assert_eq!(state_label(ChargeState::Full, Lang::Ru), "заряжено");
        assert_eq!(state_label(ChargeState::Full, Lang::En), "full");
        assert_eq!(state_str(ChargeState::Full), "full");
    }
}
