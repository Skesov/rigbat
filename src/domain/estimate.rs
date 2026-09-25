//! Estimates remaining battery time from observed percent-change history.
//!
//! Most peripheral battery readings cannot support a time estimate at all:
//! BlueZ's GATT Battery Service often reports coarse buckets (100/70/40/10)
//! rather than a real percent, a sleeping device does not discharge at its
//! in-use rate even though the clock keeps moving, and the default 60 s
//! poll interval against a single-digit-percent-per-hour discharge means most
//! consecutive samples are identical. The only usable signal is the edges —
//! the moments the percent was seen to change — observed over a long enough
//! window to distinguish a real discharge rate from a bucket edge or poll
//! jitter. Everything here refuses rather than guesses whenever that
//! evidence is thin — a wrong number is worse than no number.

use std::time::Duration;

use crate::domain::{BatteryReading, BootTime, ChargeState};
use crate::i18n::{Lang, fl, loader};

/// Below this span between the first and last edge, the trend cannot be told
/// apart from a coincidence of poll timing.
const MIN_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Two edges are the fewest that bound a measured interval.
const MIN_EDGES: usize = 2;

/// A single drop larger than this is not plausible organic discharge at the
/// default poll interval — it is the signature of a coarse-bucket reading
/// (BlueZ Battery Service devices commonly report 100/70/40/10 or similar)
/// or a recalibration jump.
const MAX_PLAUSIBLE_STEP: i16 = 5;

/// Longer estimates are shown as "more than this many days": the evidence
/// behind them is a handful of edges days apart.
const MAX_SHOWN_DAYS: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Estimate {
    /// Not discharging, a coarse reading, or not enough evidence yet.
    Unknown,
    Remaining(Duration),
}

/// Estimates remaining discharge time from a device's change-point history:
/// one point per observed percent, oldest first (see `push_history_point` in
/// the supervisor).
///
/// The first point is when a level was first *seen*, not when it was reached,
/// so the rate is measured between edges only, from the first to the last.
/// An implausible step — a rise, or a drop above `MAX_PLAUSIBLE_STEP` —
/// restarts the window at that edge rather than disabling the estimate.
pub fn estimate(history: &[(BootTime, u8)], current: BatteryReading) -> Estimate {
    if current.state != ChargeState::Discharging || current.coarse {
        return Estimate::Unknown;
    }
    let mut first_edge = None;
    for (i, pair) in history.windows(2).enumerate() {
        let step = i16::from(pair[0].1) - i16::from(pair[1].1);
        if first_edge.is_none() || !(1..=MAX_PLAUSIBLE_STEP).contains(&step) {
            first_edge = Some(i + 1);
        }
    }
    let window = first_edge
        .and_then(|i| history.get(i..))
        .unwrap_or_default();
    let (Some(&(first_at, first_percent)), Some(&(last_at, last_percent))) =
        (window.first(), window.last())
    else {
        return Estimate::Unknown;
    };
    let span = last_at.saturating_duration_since(first_at);
    if window.len() < MIN_EDGES || span < MIN_WINDOW {
        return Estimate::Unknown;
    }
    let rate_per_sec = f64::from(first_percent - last_percent) / span.as_secs_f64();
    let remaining_secs = f64::from(current.percent) / rate_per_sec;
    Estimate::Remaining(round_coarse(Duration::from_secs_f64(remaining_secs)))
}

/// Rounds a duration to a coarse, honest precision: whole hours at or above
/// 45 minutes, otherwise the nearest 15 minutes (minimum 15m). Never returns
/// a value precise enough to look like `1h 47m` — that precision is not
/// supported by a signal this noisy.
fn round_coarse(d: Duration) -> Duration {
    let mins = d.as_secs() / 60;
    if mins < 45 {
        let rounded = ((mins + 7) / 15) * 15;
        Duration::from_secs(rounded.max(15) * 60)
    } else {
        let hours = (mins + 30) / 60;
        Duration::from_secs(hours.max(1) * 3600)
    }
}

/// Formats a coarsely-rounded duration as `~2h` or `~45m` (`~2 ч`, `~45 мин`),
/// or as `>4d` (`>4 д`) past `MAX_SHOWN_DAYS`.
pub fn format_coarse(d: Duration, lang: Lang) -> String {
    let l = loader(lang);
    let secs = round_coarse(d).as_secs();
    if secs > MAX_SHOWN_DAYS * 86_400 {
        let count = MAX_SHOWN_DAYS;
        fl!(l, "estimate-over-days", count = count)
    } else if secs < 3600 {
        let count = secs / 60;
        fl!(l, "estimate-minutes", count = count)
    } else {
        let count = secs / 3600;
        fl!(l, "estimate-hours", count = count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ChargeState;

    const HOUR: Duration = Duration::from_secs(3600);

    fn at(mins: u64) -> BootTime {
        BootTime::TEST_NOW + Duration::from_secs(mins * 60)
    }

    fn cur(percent: u8) -> BatteryReading {
        BatteryReading::new(percent, ChargeState::Discharging)
    }

    /// 1% every 6 minutes, 60% down to 54%.
    fn steady() -> Vec<(BootTime, u8)> {
        (0..=6).map(|i| (at(i * 6), 60 - i as u8)).collect()
    }

    #[test]
    fn empty_history_is_unknown() {
        assert_eq!(estimate(&[], cur(50)), Estimate::Unknown);
    }

    #[test]
    fn single_transition_is_unknown() {
        let history = [(at(0), 80), (at(10), 79)];
        assert_eq!(estimate(&history, cur(79)), Estimate::Unknown);
    }

    #[test]
    fn bucketed_device_is_unknown_not_confidently_wrong() {
        let history = [(at(0), 100), (at(20), 70), (at(40), 40)];
        assert_eq!(estimate(&history, cur(40)), Estimate::Unknown);
    }

    #[test]
    fn mixed_direction_is_unknown() {
        let history = [(at(0), 50), (at(40), 40), (at(80), 45)];
        assert_eq!(estimate(&history, cur(45)), Estimate::Unknown);
    }

    #[test]
    fn steady_discharge_yields_remaining_time() {
        // 54% at 1% per 6 minutes: 5.4h.
        assert_eq!(estimate(&steady(), cur(54)), Estimate::Remaining(5 * HOUR));
    }

    #[test]
    fn charging_or_full_device_holding_its_percent_has_no_remaining_time() {
        for state in [ChargeState::Charging, ChargeState::Full] {
            let current = BatteryReading::new(54, state);
            assert_eq!(estimate(&steady(), current), Estimate::Unknown, "{state:?}");
        }
    }

    #[test]
    fn rate_is_measured_from_the_first_edge_not_the_first_observation() {
        // 60% first seen a minute before it dropped; after that, 1% per hour.
        let history = [(at(0), 60), (at(1), 59), (at(61), 58), (at(121), 57)];
        assert_eq!(estimate(&history, cur(57)), Estimate::Remaining(57 * HOUR));
    }

    #[test]
    fn outlier_step_restarts_the_window_instead_of_disabling_the_estimate() {
        // 1% per hour, a 10% jump, then 1% per hour again.
        let history = [
            (at(0), 80),
            (at(60), 79),
            (at(120), 78),
            (at(180), 68),
            (at(240), 67),
            (at(300), 66),
        ];
        assert_eq!(estimate(&history, cur(66)), Estimate::Remaining(66 * HOUR));
    }

    #[test]
    fn outlier_step_as_the_latest_edge_leaves_no_window() {
        let history = [(at(0), 80), (at(60), 79), (at(120), 78), (at(180), 68)];
        assert_eq!(estimate(&history, cur(68)), Estimate::Unknown);
    }

    #[test]
    fn host_suspend_between_edges_counts_as_elapsed_time() {
        // 1h awake, 8h suspended, 1h awake: the boot clock says 10h per 1%.
        let history = [
            (at(0), 51),
            (at(60), 50),
            (at(60 + 10 * 60), 49),
            (at(60 + 20 * 60), 48),
        ];
        assert_eq!(estimate(&history, cur(48)), Estimate::Remaining(480 * HOUR));
    }

    #[test]
    fn coarse_reading_is_unknown_even_on_a_steady_history() {
        let coarse = BatteryReading::new_coarse(54, ChargeState::Discharging);
        assert_eq!(estimate(&steady(), coarse), Estimate::Unknown);
    }

    #[test]
    fn short_window_is_unknown_even_with_two_transitions() {
        let history = [(at(0), 60), (at(5), 59), (at(10), 58), (at(15), 57)];
        assert_eq!(estimate(&history, cur(57)), Estimate::Unknown);
    }

    // --- round_coarse / format_coarse --------------------------------------

    #[test]
    fn round_coarse_boundaries() {
        assert_eq!(format_coarse(Duration::from_secs(59 * 60), Lang::En), "~1h");
        assert_eq!(format_coarse(Duration::from_secs(61 * 60), Lang::En), "~1h");
        assert_eq!(format_coarse(25 * HOUR, Lang::En), "~25h");
    }

    #[test]
    fn round_coarse_under_an_hour_rounds_to_nearest_quarter() {
        assert_eq!(format_coarse(Duration::from_secs(5 * 60), Lang::En), "~15m");
        assert_eq!(
            format_coarse(Duration::from_secs(44 * 60), Lang::En),
            "~45m"
        );
    }

    #[test]
    fn format_coarse_in_russian() {
        let ru = |d| format_coarse(d, Lang::Ru);
        assert_eq!(ru(Duration::from_secs(44 * 60)), "~45 мин");
        assert_eq!(ru(25 * HOUR), "~25 ч");
    }

    #[test]
    fn format_coarse_caps_at_four_days_in_both_languages() {
        assert_eq!(format_coarse(96 * HOUR, Lang::En), "~96h");
        assert_eq!(format_coarse(97 * HOUR, Lang::En), ">4d");
        assert_eq!(format_coarse(300 * HOUR, Lang::En), ">4d");
        assert_eq!(format_coarse(300 * HOUR, Lang::Ru), ">4 д");
    }
}
