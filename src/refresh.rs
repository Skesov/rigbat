//! Broadcast "re-poll and re-discover now" signal.
//!
//! Backed by a `watch` channel carrying a generation counter rather than
//! `tokio::sync::Notify`: `Notify::notify_waiters()` wakes only the waiters
//! registered at that exact instant and stores no permit, so a trigger fired
//! while a task was mid-poll (a D-Bus round trip, hidraw I/O) was silently
//! lost — the roster then stayed stale until the next periodic sweep.
//! `watch` retains the value and tracks each receiver's "seen" state, so a
//! bump that happens while a receiver is busy is still observed on that
//! receiver's next `wait()`.

use tokio::sync::watch;

/// Sender half: `trigger` is fired from the tray menu's Refresh item and
/// `session::watch_resume` on resume; `rediscover` by `bluez::watch_events`
/// when a device connects, disconnects, or its interfaces change.
#[derive(Clone)]
pub struct RefreshSignal {
    poll: watch::Sender<u64>,
    sweep: watch::Sender<u64>,
}

impl Default for RefreshSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl RefreshSignal {
    pub fn new() -> Self {
        Self {
            poll: watch::channel(0u64).0,
            sweep: watch::channel(0u64).0,
        }
    }

    /// Re-polls every source and re-discovers; wakes every waiter, including busy ones.
    pub fn trigger(&self) {
        // send_modify still marks every existing receiver as changed even
        // when there are currently no receivers at all.
        self.poll.send_modify(|n| *n = n.wrapping_add(1));
        self.rediscover();
    }

    /// Re-discovers without re-polling the devices already known.
    pub fn rediscover(&self) {
        self.sweep.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Resolves on `trigger`: what a source task waits on.
    pub fn waiter(&self) -> RefreshWaiter {
        RefreshWaiter {
            rx: self.poll.subscribe(),
        }
    }

    /// Resolves on `trigger` and `rediscover`: what the discovery sweep waits on.
    pub fn sweep_waiter(&self) -> RefreshWaiter {
        RefreshWaiter {
            rx: self.sweep.subscribe(),
        }
    }
}

/// Receiver half, held by `manager_task` (`sweep_waiter`) and each source task (`waiter`).
pub struct RefreshWaiter {
    rx: watch::Receiver<u64>,
}

impl RefreshWaiter {
    /// Resolves when a refresh has been requested since the last call.
    /// Never resolves if every sender is gone, so it is safe as a
    /// `tokio::select!` arm that must not spin.
    pub async fn wait(&mut self) {
        // `changed()` returns `Err` once every `Sender` is dropped. Looping
        // on that `Err` would spin the select arm forever, so park on a
        // future that never resolves instead.
        if self.rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    /// This is the case `Notify` failed: a trigger fired before the waiter
    /// ever calls `wait()` must still be observed on the first call.
    #[tokio::test]
    async fn trigger_before_wait_is_still_observed() {
        let signal = RefreshSignal::new();
        let mut waiter = signal.waiter();

        signal.trigger();

        timeout(Duration::from_secs(1), waiter.wait())
            .await
            .expect("trigger fired before wait() must still be observed");
    }

    #[tokio::test]
    async fn two_waiters_both_observe_one_trigger() {
        let signal = RefreshSignal::new();
        let mut a = signal.waiter();
        let mut b = signal.waiter();

        signal.trigger();

        timeout(Duration::from_secs(1), a.wait())
            .await
            .expect("waiter a did not observe the trigger");
        timeout(Duration::from_secs(1), b.wait())
            .await
            .expect("waiter b did not observe the trigger");
    }

    /// A waiter whose sender was dropped must never resolve — it must not
    /// spin the containing `select!` loop.
    #[tokio::test]
    async fn waiter_never_resolves_once_sender_is_dropped() {
        let signal = RefreshSignal::new();
        let mut waiter = signal.waiter();
        drop(signal);

        let result = timeout(Duration::from_millis(200), waiter.wait()).await;
        assert!(
            result.is_err(),
            "wait() resolved after the sender was dropped instead of timing out"
        );
    }

    #[tokio::test]
    async fn rediscover_wakes_the_sweep_but_no_source() {
        let signal = RefreshSignal::new();
        let mut source = signal.waiter();
        let mut sweep = signal.sweep_waiter();

        signal.rediscover();

        timeout(Duration::from_secs(1), sweep.wait())
            .await
            .expect("the sweep did not observe rediscover");
        assert!(
            timeout(Duration::from_millis(100), source.wait())
                .await
                .is_err(),
            "a source re-polled on rediscover"
        );

        signal.trigger();
        timeout(Duration::from_secs(1), source.wait())
            .await
            .expect("a source did not observe trigger");
        timeout(Duration::from_secs(1), sweep.wait())
            .await
            .expect("the sweep did not observe trigger");
    }
}
