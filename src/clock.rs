//! Reads the host boot clock into `domain::BootTime`.

use std::time::Duration;

use nix::time::{ClockId, clock_gettime};

use crate::domain::BootTime;

pub fn now() -> BootTime {
    // CLOCK_BOOTTIME exists on every kernel rigbat supports (Linux >= 2.6.39).
    let since_boot = clock_gettime(ClockId::CLOCK_BOOTTIME)
        .map(Duration::from)
        .unwrap_or_default();
    BootTime::from_boot(since_boot)
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
