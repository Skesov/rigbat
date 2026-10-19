//! Reads the host boot clock into `domain::BootTime`.

use std::time::Duration;

use nix::time::{ClockId, clock_gettime};

use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at};

use crate::domain::{AGE_STEP, BootTime};

pub fn now() -> BootTime {
    // CLOCK_BOOTTIME exists on every kernel rigbat supports (Linux >= 2.6.39).
    let since_boot = clock_gettime(ClockId::CLOCK_BOOTTIME)
        .map(Duration::from)
        .unwrap_or_default();
    BootTime::from_boot(since_boot)
}

/// Ticks every `AGE_STEP`, the first tick one step from now.
pub fn age_ticker() -> Interval {
    let mut tick = interval_at(Instant::now() + AGE_STEP, AGE_STEP);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_clock_is_running_and_never_goes_back() {
        let first = now();
        let second = now();
        assert!(first > BootTime::from_boot(Duration::ZERO));
        assert!(second >= first);
    }
}
