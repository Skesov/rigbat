use std::ops::Add;
use std::time::Duration;

/// A point on the host's boot clock (`CLOCK_BOOTTIME`), in nanoseconds since
/// boot. Unlike `Instant` (`CLOCK_MONOTONIC`), it keeps counting while the host
/// is suspended, so a discharge rate or a reading's age spanning a suspend is
/// not shortened by it. Signed: a reading replayed from the state store can
/// predate this boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BootTime(i64);

impl BootTime {
    pub fn from_boot(since_boot: Duration) -> Self {
        Self(nanos(since_boot))
    }

    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(u64::try_from(self.0.saturating_sub(earlier.0)).unwrap_or(0))
    }

    pub fn checked_sub(self, d: Duration) -> Option<Self> {
        self.0.checked_sub(nanos(d)).map(Self)
    }
}

impl Add<Duration> for BootTime {
    type Output = Self;

    fn add(self, d: Duration) -> Self {
        Self(self.0.saturating_add(nanos(d)))
    }
}

fn nanos(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

#[cfg(test)]
impl BootTime {
    /// A fixed "now" a week after boot, for tests that pass time explicitly.
    pub const TEST_NOW: Self = Self(7 * 24 * 60 * 60 * 1_000_000_000);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_since_saturates_and_a_point_may_predate_boot() {
        let t = BootTime::from_boot(Duration::from_secs(10));
        let later = t + Duration::from_secs(5);
        assert_eq!(later.saturating_duration_since(t), Duration::from_secs(5));
        assert_eq!(t.saturating_duration_since(later), Duration::ZERO);

        let before_boot = t.checked_sub(Duration::from_secs(3600)).unwrap();
        assert_eq!(
            t.saturating_duration_since(before_boot),
            Duration::from_secs(3600)
        );
    }
}
