//! Supervising retry loop for the bus watchers (`sources::bluez::watch_events`,
//! `session::watch_resume`) and its exponential backoff. The policy is pure and
//! synchronous, deliberately: the watchers need a live D-Bus connection to
//! test, but growth, the cap, and the reset rule do not.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::Context;

/// Delay before the first retry after a watcher's attempt ends.
const INITIAL_DELAY: Duration = Duration::from_secs(1);

/// Delay never grows past this: a bus that is genuinely gone does not make
/// the watcher spin, and one that comes back is not kept waiting long.
const MAX_DELAY: Duration = Duration::from_secs(60);

/// An attempt that ran at least this long counts as a working session: the
/// next failure's backoff starts over at `INITIAL_DELAY` instead of
/// inheriting a delay built up from an earlier, unrelated outage.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// The delay to use after `current`: doubles, capped at `MAX_DELAY`.
fn next_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_DELAY)
}

/// Whether an attempt that ran for `ran_for` counts as a working session and
/// should reset the backoff to `INITIAL_DELAY` before the next failure.
fn is_healthy_run(ran_for: Duration) -> bool {
    ran_for >= HEALTHY_RUN
}

/// Backoff state carried across one watcher's attempts.
#[derive(Debug)]
struct Backoff {
    delay: Duration,
    consecutive_failures: u32,
}

impl Backoff {
    fn new() -> Self {
        Self {
            delay: INITIAL_DELAY,
            consecutive_failures: 0,
        }
    }

    /// Records an attempt that ran for `ran_for`. Returns the wait before the
    /// next attempt and how many failures preceded this one in the current
    /// streak (0: the first, worth a warning).
    fn after_attempt(&mut self, ran_for: Duration) -> (Duration, u32) {
        if is_healthy_run(ran_for) {
            self.delay = INITIAL_DELAY;
            self.consecutive_failures = 0;
        }
        let failures = self.consecutive_failures;
        self.consecutive_failures = failures.saturating_add(1);
        let wait = self.delay;
        self.delay = next_delay(wait);
        (wait, failures)
    }
}

/// Spawns `attempt` under a retry loop that never gives up. Each attempt gets
/// its connection from `ctx.system_bus()`, which re-dials a closed one; an
/// attempt that ends, with an error or not, is retried after the backoff.
/// Only the first failure of a streak logs a warning.
pub fn supervise<F, Fut>(label: &'static str, ctx: Arc<Context>, mut attempt: F)
where
    F: FnMut(zbus::Connection) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = Backoff::new();
        loop {
            let started = Instant::now();
            let result = match ctx.system_bus().await {
                Ok(conn) => attempt(conn).await,
                Err(e) => Err(e),
            };
            let detail = match &result {
                Ok(()) => "stream ended".to_owned(),
                Err(e) => format!("{e:#}"),
            };
            let (wait, failures) = backoff.after_attempt(started.elapsed());
            if failures == 0 {
                tracing::warn!("{label} stopped: {detail}");
            } else {
                tracing::debug!(failures, "{label} stopped: {detail}");
            }
            tokio::time::sleep(wait).await;
        }
    });
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

    #[test]
    fn failing_attempts_back_off_and_only_the_first_warns() {
        let mut backoff = Backoff::new();
        let steps: Vec<_> = (0..8)
            .map(|_| backoff.after_attempt(Duration::ZERO))
            .collect();
        let waits: Vec<u64> = steps.iter().map(|(w, _)| w.as_secs()).collect();
        let failures: Vec<u32> = steps.iter().map(|(_, f)| *f).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(failures, [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn healthy_run_starts_a_new_streak() {
        let mut backoff = Backoff::new();
        for _ in 0..4 {
            backoff.after_attempt(Duration::ZERO);
        }
        assert_eq!(backoff.after_attempt(HEALTHY_RUN), (INITIAL_DELAY, 0));
        assert_eq!(
            backoff.after_attempt(Duration::ZERO),
            (Duration::from_secs(2), 1)
        );
    }
}
