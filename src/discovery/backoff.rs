//! Exponential backoff policy for the bus-watcher supervising retry loops
//! (`sources::bluez::watch_events`, `session::watch_resume`). Pure and
//! synchronous, deliberately: the watchers themselves need a live D-Bus
//! connection to test, but growth, the cap, and the reset rule do not.

use std::time::Duration;

/// Delay before the first retry after a watcher's attempt ends.
pub const INITIAL_DELAY: Duration = Duration::from_secs(1);

/// Delay never grows past this: a bus that is genuinely gone does not make
/// the watcher spin, and one that comes back is not kept waiting long.
pub const MAX_DELAY: Duration = Duration::from_secs(60);

/// An attempt that ran at least this long counts as a working session: the
/// next failure's backoff starts over at `INITIAL_DELAY` instead of
/// inheriting a delay built up from an earlier, unrelated outage.
pub const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// The delay to use after `current`: doubles, capped at `MAX_DELAY`.
pub fn next_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_DELAY)
}

/// Whether an attempt that ran for `ran_for` counts as a working session and
/// should reset the backoff to `INITIAL_DELAY` before the next failure.
pub fn is_healthy_run(ran_for: Duration) -> bool {
    ran_for >= HEALTHY_RUN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_doubles_each_failure() {
        let d1 = INITIAL_DELAY;
        let d2 = next_delay(d1);
        let d3 = next_delay(d2);
        assert_eq!(d2, Duration::from_secs(2));
        assert_eq!(d3, Duration::from_secs(4));
    }

    #[test]
    fn delay_caps_at_max() {
        let mut d = INITIAL_DELAY;
        for _ in 0..10 {
            d = next_delay(d);
        }
        assert_eq!(d, MAX_DELAY);
    }

    #[test]
    fn delay_stays_at_max_once_reached() {
        assert_eq!(next_delay(MAX_DELAY), MAX_DELAY);
    }

    #[test]
    fn healthy_run_boundary_resets() {
        assert!(is_healthy_run(HEALTHY_RUN));
        assert!(is_healthy_run(HEALTHY_RUN + Duration::from_secs(1)));
    }

    #[test]
    fn short_run_does_not_reset() {
        assert!(!is_healthy_run(HEALTHY_RUN - Duration::from_millis(1)));
        assert!(!is_healthy_run(Duration::ZERO));
    }
}
