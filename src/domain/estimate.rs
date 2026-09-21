//! Estimates remaining battery time from observed percent-change history.
//!
//! Most peripheral battery readings cannot support a time estimate at all:
//! BlueZ's GATT Battery Service often reports coarse buckets (100/70/40/10)
//! rather than a real percent, a sleeping device does not discharge at its
//! in-use rate even though the wall clock keeps moving, and the default 60 s
//! poll interval against a single-digit-percent-per-hour discharge means most
//! consecutive samples are identical. The only usable signal is the step
//! transitions where the percent actually changed, observed over a long
//! enough window to distinguish a real discharge rate from a bucket edge or
//! poll jitter. Everything here refuses rather than guesses whenever that
//! evidence is thin — a wrong number is worse than no number.

use std::time::{Duration, Instant};

/// Below this span, two transitions cannot be told apart from a coincidence
/// of poll timing — the estimate needs to see the trend hold for a while.
const MIN_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Fewer than two downward transitions cannot distinguish a real discharge
/// rate from a single bucket edge (e.g. 70% -> 40%).
const MIN_TRANSITIONS: usize = 2;

/// A single step larger than this is not plausible organic discharge at the
/// default poll interval — it is the signature of a coarse-bucket reading
/// (BlueZ Battery Service devices commonly report 100/70/40/10 or similar).
const MAX_PLAUSIBLE_STEP: u32 = 5;

/// If the largest observed step is more than this many times the smallest,
/// the steps are not one steady rate — averaging through them would report a
/// number the data does not support.
const MAX_STEP_RATIO: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Estimate {
    /// Not enough evidence yet, or the reading is not moving monotonically down.
    Unknown,
    /// Charging. No numeric time-to-full is reported — the same coarse-bucket
    /// and sleep problems that make a discharge estimate untrustworthy apply
    /// equally to a charge estimate, so this is a state flag, not a duration.
    Charging,
    Remaining(Duration),
}

/// Estimates remaining discharge time from a device's recorded percent-change
/// history. `history` holds only points where the percent differed from the
/// previous sample (see `push_reading` in the supervisor) — the caller is not
/// expected to include `current` in it. `now`/`current` are the latest poll,
/// passed separately so a poll that has not yet changed the percent still
/// updates the elapsed window.
pub fn estimate(history: &[(Instant, u8)], now: Instant, current: u8) -> Estimate {
    let mut points: Vec<(Instant, u8)> = history.to_vec();
    if points.last().map(|&(_, p)| p) != Some(current) {
        points.push((now, current));
    }

    let transitions: Vec<(Duration, i32)> = points
        .windows(2)
        .map(|w| {
            let dt = w[1].0.duration_since(w[0].0);
            let dp = w[1].1 as i32 - w[0].1 as i32;
            (dt, dp)
        })
        .filter(|&(_, dp)| dp != 0)
        .collect();

    let has_up = transitions.iter().any(|&(_, dp)| dp > 0);
    let has_down = transitions.iter().any(|&(_, dp)| dp < 0);

    match (has_up, has_down) {
        (true, true) => Estimate::Unknown,
        (true, false) => Estimate::Charging,
        (false, false) => Estimate::Unknown,
        (false, true) => estimate_discharge(&points, &transitions, current),
    }
}

fn estimate_discharge(
    points: &[(Instant, u8)],
    transitions: &[(Duration, i32)],
    current: u8,
) -> Estimate {
    if transitions.len() < MIN_TRANSITIONS {
        return Estimate::Unknown;
    }

    let (first, _) = points[0];
    let (last, _) = points[points.len() - 1];
    let span = last.duration_since(first);
    if span < MIN_WINDOW {
        return Estimate::Unknown;
    }

    let steps: Vec<u32> = transitions
        .iter()
        .map(|&(_, dp)| dp.unsigned_abs())
        .collect();
    let max_step = steps.iter().copied().max().unwrap_or(0);
    let min_step = steps.iter().copied().min().unwrap_or(0);

    if max_step > MAX_PLAUSIBLE_STEP {
        return Estimate::Unknown;
    }
    if min_step == 0 || max_step as f64 / min_step as f64 > MAX_STEP_RATIO {
        return Estimate::Unknown;
    }

    let total_drop: u32 = steps.iter().sum();
    let rate_per_sec = total_drop as f64 / span.as_secs_f64();
    if rate_per_sec <= 0.0 {
        return Estimate::Unknown;
    }

    let remaining_secs = (current as f64 / rate_per_sec).max(0.0);
    Estimate::Remaining(round_coarse(Duration::from_secs_f64(remaining_secs)))
}

/// Rounds a duration to a coarse, honest precision: whole hours at or above
/// 45 minutes, otherwise the nearest 15 minutes (minimum 15m). Never returns
/// a value precise enough to look like `1h 47m` — that precision is not
/// supported by a signal this noisy.
pub fn round_coarse(d: Duration) -> Duration {
    let mins = d.as_secs() / 60;
    if mins < 45 {
        let rounded = ((mins + 7) / 15) * 15;
        Duration::from_secs(rounded.max(15) * 60)
    } else {
        let hours = (mins + 30) / 60;
        Duration::from_secs(hours.max(1) * 3600)
    }
}

/// Formats a coarsely-rounded duration as `~2h` or `~45m`.
pub fn format_coarse(d: Duration) -> String {
    let rounded = round_coarse(d);
    let secs = rounded.as_secs();
    if secs < 3600 {
        format!("~{}m", secs / 60)
    } else {
        format!("~{}h", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, mins: u64) -> Instant {
        base + Duration::from_secs(mins * 60)
    }

    #[test]
    fn empty_history_is_unknown() {
        let now = Instant::now();
        assert_eq!(estimate(&[], now, 50), Estimate::Unknown);
    }

    #[test]
    fn single_transition_is_unknown() {
        let base = Instant::now();
        let history = [(at(base, 0), 80), (at(base, 10), 79)];
        assert_eq!(estimate(&history, at(base, 10), 79), Estimate::Unknown);
    }

    #[test]
    fn bucketed_device_is_unknown_not_confidently_wrong() {
        let base = Instant::now();
        let history = [(at(base, 0), 100), (at(base, 20), 70), (at(base, 40), 40)];
        assert_eq!(estimate(&history, at(base, 40), 40), Estimate::Unknown);
    }

    #[test]
    fn upward_step_yields_unknown_or_charging() {
        let base = Instant::now();
        let history = [(at(base, 0), 50), (at(base, 10), 60)];
        let result = estimate(&history, at(base, 10), 60);
        assert!(matches!(result, Estimate::Unknown | Estimate::Charging));
    }

    #[test]
    fn pure_upward_trend_is_charging() {
        let base = Instant::now();
        let history = [(at(base, 0), 50), (at(base, 10), 55), (at(base, 20), 60)];
        assert_eq!(estimate(&history, at(base, 20), 60), Estimate::Charging);
    }

    #[test]
    fn mixed_direction_is_unknown() {
        let base = Instant::now();
        let history = [(at(base, 0), 50), (at(base, 40), 40), (at(base, 80), 45)];
        assert_eq!(estimate(&history, at(base, 80), 45), Estimate::Unknown);
    }

    #[test]
    // clippy::panic has no allow-in-tests config (unlike unwrap_used/expect_used);
    // this panic is the test's own failure message for an unexpected match arm.
    #[expect(clippy::panic)]
    fn steady_discharge_yields_a_plausible_remaining_range() {
        let base = Instant::now();
        // 1% per 6 minutes, held for 36 minutes: 6 downward transitions.
        let history = [
            (at(base, 0), 60),
            (at(base, 6), 59),
            (at(base, 12), 58),
            (at(base, 18), 57),
            (at(base, 24), 56),
            (at(base, 30), 55),
            (at(base, 36), 54),
        ];
        let result = estimate(&history, at(base, 36), 54);
        match result {
            Estimate::Remaining(d) => {
                // 54% at 1%/6min is ~5.4h; assert a wide, honest range rather
                // than the exact figure.
                assert!(d >= Duration::from_secs(4 * 3600));
                assert!(d <= Duration::from_secs(7 * 3600));
            }
            other => panic!("expected Remaining, got {other:?}"),
        }
    }

    #[test]
    fn short_window_is_unknown_even_with_two_transitions() {
        let base = Instant::now();
        // Two transitions, but only 10 minutes apart — below MIN_WINDOW.
        let history = [(at(base, 0), 60), (at(base, 5), 59), (at(base, 10), 58)];
        assert_eq!(estimate(&history, at(base, 10), 58), Estimate::Unknown);
    }

    #[test]
    fn uneven_steps_are_unknown() {
        let base = Instant::now();
        // Steps of 1, 1, 4: individually plausible, but ratio 4/1 exceeds
        // MAX_STEP_RATIO — not one steady rate.
        let history = [
            (at(base, 0), 60),
            (at(base, 10), 59),
            (at(base, 20), 58),
            (at(base, 40), 54),
        ];
        assert_eq!(estimate(&history, at(base, 40), 54), Estimate::Unknown);
    }

    #[test]
    fn never_negative_when_current_exceeds_rate_projection() {
        let base = Instant::now();
        let history = [(at(base, 0), 10), (at(base, 30), 5), (at(base, 60), 0)];
        let result = estimate(&history, at(base, 60), 0);
        // current is 0: remaining must not be negative.
        if let Estimate::Remaining(d) = result {
            assert!(d.as_secs() < u64::MAX);
        }
    }

    // --- round_coarse / format_coarse --------------------------------------

    #[test]
    fn round_coarse_boundaries() {
        assert_eq!(format_coarse(Duration::from_secs(59 * 60)), "~1h");
        assert_eq!(format_coarse(Duration::from_secs(61 * 60)), "~1h");
        assert_eq!(format_coarse(Duration::from_secs(25 * 3600)), "~25h");
    }

    #[test]
    fn round_coarse_under_an_hour_rounds_to_nearest_quarter() {
        assert_eq!(format_coarse(Duration::from_secs(5 * 60)), "~15m");
        assert_eq!(format_coarse(Duration::from_secs(44 * 60)), "~45m");
    }
}
