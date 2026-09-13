use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::sleep;

use crate::app::refresh::RefreshSignal;
use crate::config::Config;
use crate::domain::estimate::estimate as estimate_remaining;
use crate::domain::{BatteryReading, DeviceId, DeviceInfo, DeviceState, Estimate, Presence};
use crate::sources::BatterySource;

const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);

/// A device flips to `Unreachable` only after this many consecutive failed
/// polls. Wireless receivers drop telemetry packets as normal behaviour, so 1
/// would flicker the icon on every dropped packet; 3 misses is long enough to
/// absorb that noise while still catching a genuinely dead device within a
/// few minutes at the default 60 s poll interval.
const OFFLINE_AFTER_FAILURES: u32 = 3;

/// How long a `Disconnected` device stays in the roster before it is pruned,
/// so a long-running session's roster does not grow without bound. Generous
/// enough that a device used yesterday is still listed today.
const DISCONNECTED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Cap on the retained percent-change history per device, used to estimate
/// remaining battery time. Only points where the percent changed are kept
/// (see `push_reading`), so this is naturally small; the cap exists so a
/// device left running for weeks cannot grow it without bound. 20 points is
/// generous for the multi-hour windows the estimator needs while staying a
/// few bytes per device. History is in-memory only — a restart starts over.
/// Persisting it would mean writing a data file and reasoning about clock
/// jumps across reboots, disproportionate to a tooltip hint.
const HISTORY_CAP: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub devices: Vec<DeviceState>,
}

pub struct Supervisor;

impl Supervisor {
    /// Spawns the manager with the real `discovery::discover_all` backend.
    pub fn spawn(
        config_rx: watch::Receiver<Config>,
    ) -> (watch::Receiver<TrayState>, RefreshSignal) {
        Self::spawn_with(config_rx, || Box::pin(crate::discovery::discover_all()))
    }

    /// Injectable discovery for tests. `discover` is called once at start, then
    /// on every discovery tick and every `refresh` trigger.
    pub fn spawn_with<F, Fut>(
        config_rx: watch::Receiver<Config>,
        discover: F,
    ) -> (watch::Receiver<TrayState>, RefreshSignal)
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Vec<Box<dyn BatterySource>>> + Send + 'static,
    {
        let initial = TrayState { devices: vec![] };
        let (watch_tx, watch_rx) = watch::channel(initial);
        let refresh = RefreshSignal::new();

        let refresh_inner = refresh.clone();
        tokio::spawn(manager_task(config_rx, discover, watch_tx, refresh_inner));

        (watch_rx, refresh)
    }
}

/// Bookkeeping kept per device, in addition to the `DeviceState` snapshot
/// exposed to the tray/notifications: the running-total failure count that
/// drives debounce, kept out of `DeviceState` because nothing outside the
/// registry needs it.
struct DeviceEntry {
    info: DeviceInfo,
    last_reading: Option<BatteryReading>,
    last_seen: Option<Instant>,
    presence: Presence,
    consecutive_failures: u32,
    /// Percent-change points feeding the remaining-time estimate. Only a
    /// point where the percent differs from the previous one is kept — see
    /// `push_reading`.
    battery_history: Vec<(Instant, u8)>,
}

impl DeviceEntry {
    /// Records a new percent reading into `battery_history`. A percent equal
    /// to the last recorded one is not a transition and is dropped — most
    /// polls see no change at the default interval, and only the change
    /// points carry a discharge-rate signal. An increase (charging, or a
    /// device re-reporting after a battery swap) invalidates any discharge
    /// trend observed so far, so it clears the history before recording the
    /// new baseline.
    fn push_reading(&mut self, now: Instant, percent: u8) {
        if let Some(&(_, last)) = self.battery_history.last() {
            match percent.cmp(&last) {
                std::cmp::Ordering::Equal => return,
                std::cmp::Ordering::Greater => self.battery_history.clear(),
                std::cmp::Ordering::Less => {}
            }
        }
        self.battery_history.push((now, percent));
        if self.battery_history.len() > HISTORY_CAP {
            self.battery_history.remove(0);
        }
    }

    fn estimate(&self, now: Instant) -> Estimate {
        match self.last_reading {
            Some(r) => estimate_remaining(&self.battery_history, now, r.percent),
            None => Estimate::Unknown,
        }
    }
}

/// Live set of discovered devices and their polling tasks, keyed by identity.
/// `order` preserves discovery order so the tray roster is stable across polls.
/// A device that leaves discovery keeps its entry (see `reconcile`) — only
/// `tasks` loses the id, so `entries` can outlive `tasks` for a given id.
struct DeviceRegistry {
    order: Vec<DeviceId>,
    entries: HashMap<DeviceId, DeviceEntry>,
    tasks: HashMap<DeviceId, AbortHandle>,
}

/// What spawning a source task needs, bundled so `reconcile` stays a
/// small-signature method rather than an argument list.
struct SourceCtx {
    tx: mpsc::Sender<(DeviceId, Option<BatteryReading>)>,
    config_rx: watch::Receiver<Config>,
    refresh: RefreshSignal,
}

impl DeviceRegistry {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            entries: HashMap::new(),
            tasks: HashMap::new(),
        }
    }

    /// Applies a poll outcome to a device that is still registered. A result
    /// for an id no longer present is ignored — a poll can win a race against
    /// the abort of its own task.
    ///
    /// Success sets `Online` and resets the failure count immediately. A
    /// failure only flips presence to `Unreachable` once it reaches
    /// `OFFLINE_AFTER_FAILURES` — a single dropped poll leaves presence (and
    /// any retained reading) untouched.
    fn record(&mut self, id: &DeviceId, reading: Option<BatteryReading>) {
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        match reading {
            Some(r) => {
                let now = Instant::now();
                entry.push_reading(now, r.percent);
                entry.last_reading = Some(r);
                entry.last_seen = Some(now);
                entry.consecutive_failures = 0;
                entry.presence = Presence::Online;
            }
            None => {
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                if entry.consecutive_failures >= OFFLINE_AFTER_FAILURES {
                    entry.presence = Presence::Unreachable;
                }
            }
        }
    }

    /// Spawns a task for each genuinely new or reappeared id, drops the
    /// transient handle for ids that already run, and aborts (without
    /// forgetting) ids that vanished from discovery. Finally prunes entries
    /// that have been `Disconnected` for too long.
    fn reconcile(&mut self, fresh: Vec<Box<dyn BatterySource>>, ctx: &SourceCtx) {
        let fresh_ids: Vec<DeviceId> = fresh.iter().map(|s| s.device().id()).collect();

        for src in fresh {
            let id = src.device().id();
            if let Some(handle) = self.tasks.get(&id) {
                if handle.is_finished() {
                    // A handle only stays in `tasks` for an id that is also
                    // still in `fresh_ids` if it was never aborted: the
                    // vanished-device path below removes the handle from
                    // `tasks` in the same call that aborts it, so it is never
                    // seen here again. `is_finished()` can't tell a panic
                    // apart from a deliberate abort on its own — this is
                    // what makes the two paths unambiguous. Reaching this
                    // branch therefore means the task ended by itself:
                    // panicked, or its mpsc sender was dropped.
                    tracing::error!(
                        device = %src.device().name,
                        "polling task ended unexpectedly; restarting it"
                    );
                    self.tasks.remove(&id);
                    if let Some(entry) = self.entries.get_mut(&id) {
                        // Positive proof of death, not a single missed poll —
                        // demote immediately rather than waiting out
                        // OFFLINE_AFTER_FAILURES.
                        entry.presence = Presence::Unreachable;
                        entry.consecutive_failures = OFFLINE_AFTER_FAILURES;
                    }
                    let handle = spawn_source_task(src, id.clone(), ctx);
                    self.tasks.insert(id, handle);
                } else {
                    // Still healthy — drop the transient handle, task keeps running.
                    drop(src);
                }
                continue;
            }

            match self.entries.entry(id.clone()) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    // Reappeared after being Disconnected: reuse the entry,
                    // keep the retained reading and presence until the fresh
                    // task proves it Online again.
                    tracing::info!(device = %src.device().name, "device reappeared");
                    slot.get_mut().info = src.device().clone();
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    tracing::info!(device = %src.device().name, "device appeared");
                    slot.insert(DeviceEntry {
                        info: src.device().clone(),
                        last_reading: None,
                        last_seen: None,
                        presence: Presence::Unreachable,
                        consecutive_failures: 0,
                        battery_history: Vec::new(),
                    });
                    self.order.push(id.clone());
                }
            }

            let handle = spawn_source_task(src, id.clone(), ctx);
            self.tasks.insert(id, handle);
        }

        let vanished: Vec<DeviceId> = self
            .tasks
            .keys()
            .filter(|id| !fresh_ids.contains(id))
            .cloned()
            .collect();
        for id in vanished {
            if let Some(handle) = self.tasks.remove(&id) {
                handle.abort();
            }
            if let Some(entry) = self.entries.get_mut(&id) {
                entry.presence = Presence::Disconnected;
                tracing::info!(device = %entry.info.name, "device vanished");
            }
        }

        self.prune_stale(Instant::now());
    }

    /// Drops `Disconnected` entries that have not been seen for longer than
    /// `DISCONNECTED_RETENTION`. An entry that never produced a reading has
    /// nothing to retain, so it is dropped on disconnect rather than waiting
    /// out the cap. `now` is a parameter so tests can age an entry without a
    /// wall-clock sleep.
    fn prune_stale(&mut self, now: Instant) {
        let stale: Vec<DeviceId> = self
            .entries
            .iter()
            .filter(|(_, e)| e.presence == Presence::Disconnected)
            .filter(|(_, e)| match e.last_seen {
                None => true,
                Some(t) => now.duration_since(t) > DISCONNECTED_RETENTION,
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.entries.remove(&id);
            self.order.retain(|oid| oid != &id);
        }
    }

    fn snapshot(&self) -> Vec<DeviceState> {
        let now = Instant::now();
        self.order
            .iter()
            .filter_map(|id| {
                let entry = self.entries.get(id)?;
                Some(DeviceState {
                    info: entry.info.clone(),
                    last_reading: entry.last_reading,
                    last_seen: entry.last_seen,
                    presence: entry.presence,
                    estimate: entry.estimate(now),
                })
            })
            .collect()
    }
}

async fn manager_task<F, Fut>(
    config_rx: watch::Receiver<Config>,
    discover: F,
    watch_tx: watch::Sender<TrayState>,
    refresh: RefreshSignal,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Vec<Box<dyn BatterySource>>> + Send + 'static,
{
    let (mpsc_tx, mut mpsc_rx) = mpsc::channel::<(DeviceId, Option<BatteryReading>)>(64);
    let mut waiter = refresh.waiter();
    let ctx = SourceCtx {
        tx: mpsc_tx,
        config_rx,
        refresh,
    };

    let mut registry = DeviceRegistry::new();

    rediscover(&mut registry, discover(), &ctx, &watch_tx).await;

    loop {
        tokio::select! {
            msg = mpsc_rx.recv() => {
                match msg {
                    Some((id, reading)) => {
                        registry.record(&id, reading);
                        publish(&registry, &watch_tx);
                    }
                    // All source tasks dropped their senders — nothing left to do.
                    None => return,
                }
            }
            _ = sleep(DISCOVERY_INTERVAL) => {
                rediscover(&mut registry, discover(), &ctx, &watch_tx).await;
            }
            _ = waiter.wait() => {
                rediscover(&mut registry, discover(), &ctx, &watch_tx).await;
            }
        }
    }
}

// Takes the discovery future itself, rather than the `discover` closure, so
// the generated future only needs `Fut: Send` — holding `&F` across the
// `.await` below would additionally require `F: Sync`, which the public API
// does not promise.
async fn rediscover<Fut>(
    registry: &mut DeviceRegistry,
    discover: Fut,
    ctx: &SourceCtx,
    watch_tx: &watch::Sender<TrayState>,
) where
    Fut: Future<Output = Vec<Box<dyn BatterySource>>>,
{
    let fresh = discover.await;
    registry.reconcile(fresh, ctx);
    publish(registry, watch_tx);
}

fn spawn_source_task(
    mut src: Box<dyn BatterySource>,
    id: DeviceId,
    ctx: &SourceCtx,
) -> AbortHandle {
    let name = src.device().name.clone();
    let tx = ctx.tx.clone();
    let mut config_rx = ctx.config_rx.clone();
    let mut waiter = ctx.refresh.waiter();

    let handle = tokio::spawn(async move {
        loop {
            let reading = src.poll().await.ok();
            if tx.send((id.clone(), reading)).await.is_err() {
                return;
            }

            // effective_poll_interval_secs clamps to >= 1, so a hostile 0 in
            // config.json cannot spin this loop.
            let interval =
                Duration::from_secs(config_rx.borrow().effective_poll_interval_secs(&name));
            // The borrow above is dropped before the select below — a
            // watch::Ref must never be held across an .await.

            tokio::select! {
                _ = sleep(interval) => {}
                _ = waiter.wait() => {}
                // A config change wakes the task immediately so a shortened
                // interval applies at once instead of after the old sleep.
                _ = config_rx.changed() => {}
            }
        }
    });
    handle.abort_handle()
}

fn publish(registry: &DeviceRegistry, watch_tx: &watch::Sender<TrayState>) {
    let state = TrayState {
        devices: registry.snapshot(),
    };

    // Ignore error — receiver closed means the tray exited.
    let _ = watch_tx.send(state);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::Config;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind, Transport};
    use crate::sources::BatterySource;
    use tokio::time::timeout;

    /// Keeps the sender alive for the test's lifetime so `config_rx.changed()`
    /// in a source task's select loop waits instead of resolving with an
    /// error and spinning.
    fn config_channel(cfg: Config) -> (watch::Sender<Config>, watch::Receiver<Config>) {
        watch::channel(cfg)
    }

    struct OkSource {
        info: DeviceInfo,
        reading: BatteryReading,
    }

    struct ErrSource {
        info: DeviceInfo,
    }

    #[async_trait::async_trait]
    impl BatterySource for OkSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            Ok(self.reading)
        }
    }

    #[async_trait::async_trait]
    impl BatterySource for ErrSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            anyhow::bail!("device unavailable")
        }
    }

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            kind: DeviceKind::Other,
            transport: Transport::Sysfs,
            locator: Some(name.to_owned()),
        }
    }

    fn reading_discharging(percent: u8) -> BatteryReading {
        BatteryReading::new(percent, ChargeState::Discharging)
    }

    /// Waits for a state where at least one device is Online, with a timeout.
    async fn wait_for_connected(rx: &mut watch::Receiver<TrayState>) -> TrayState {
        timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let state = rx.borrow().clone();
                    if state.devices.iter().any(|d| d.presence == Presence::Online) {
                        return state;
                    }
                }
                rx.changed().await.expect("watch channel closed");
            }
        })
        .await
        .expect("timed out waiting for connected state")
    }

    /// Waits for TrayState.devices to be non-empty, with a timeout.
    async fn wait_for_devices(rx: &mut watch::Receiver<TrayState>) -> TrayState {
        timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let state = rx.borrow().clone();
                    if !state.devices.is_empty() {
                        return state;
                    }
                }
                rx.changed().await.expect("watch channel closed");
            }
        })
        .await
        .expect("timed out waiting for devices to appear")
    }

    /// [Err, Ok(80% discharging)] → both devices present, keyboard's reading is Some(80%)
    #[tokio::test]
    async fn primary_is_first_connected() {
        let (_tx, config_rx) = config_channel(Config::default());
        let (mut rx, _refresh) = Supervisor::spawn_with(config_rx, || {
            let sources: Vec<Box<dyn BatterySource>> = vec![
                Box::new(ErrSource {
                    info: device("mouse"),
                }),
                Box::new(OkSource {
                    info: device("keyboard"),
                    reading: reading_discharging(80),
                }),
            ];
            async move { sources }
        });

        let state = wait_for_connected(&mut rx).await;
        assert_eq!(state.devices.len(), 2);
        let keyboard = state
            .devices
            .iter()
            .find(|d| d.info.name == "keyboard")
            .expect("keyboard present");
        assert_eq!(keyboard.last_reading, Some(reading_discharging(80)));
    }

    /// All sources Err → both devices present, both readings None
    #[tokio::test]
    async fn all_err_stays_offline() {
        let (_tx, config_rx) = config_channel(Config::default());
        let (rx, _refresh) = Supervisor::spawn_with(config_rx, || {
            let sources: Vec<Box<dyn BatterySource>> = vec![
                Box::new(ErrSource {
                    info: device("mouse"),
                }),
                Box::new(ErrSource {
                    info: device("keyboard"),
                }),
            ];
            async move { sources }
        });

        // Sources are instant — give the scheduler a chance to run tasks.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let state = rx.borrow().clone();
        assert_eq!(state.devices.len(), 2);
        assert!(state.devices.iter().all(|d| d.last_reading.is_none()));
    }

    /// spawn_with(|| vec![]) → devices empty
    #[tokio::test]
    async fn empty_sources() {
        let (_tx, config_rx) = config_channel(Config::default());
        let (rx, _refresh) =
            Supervisor::spawn_with(config_rx, || async { Vec::<Box<dyn BatterySource>>::new() });

        // Give the manager task a chance to run its initial reconcile.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let state = rx.borrow().clone();
        assert!(state.devices.is_empty());
    }

    /// First discovery returns []; after refresh it returns [OkSource("kbd", 70)].
    /// The device should appear in TrayState after the refresh.
    #[tokio::test]
    async fn hotplug_adds_device() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let (_tx, config_rx) = config_channel(Config::default());
        let (mut rx, refresh) = Supervisor::spawn_with(config_rx, move || {
            let count = call_count_clone.fetch_add(1, Ordering::Relaxed);
            let sources: Vec<Box<dyn BatterySource>> = if count == 0 {
                vec![]
            } else {
                vec![Box::new(OkSource {
                    info: device("kbd"),
                    reading: reading_discharging(70),
                })]
            };
            async move { sources }
        });

        // Initial reconcile with empty list — give it time to settle.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let initial = rx.borrow().clone();
        assert!(initial.devices.is_empty());

        // Trigger re-discovery with the kbd present.
        refresh.trigger();

        let state = wait_for_devices(&mut rx).await;
        assert_eq!(state.devices.len(), 1);
        assert_eq!(state.devices[0].info.name, "kbd");
    }

    /// A device polling at a long interval picks up the config update at
    /// once, instead of waiting out the old sleep: the interval is set to an
    /// hour, so a rising poll count within the timeout below can only come
    /// from the `config_rx.changed()` select arm, not from the sleep expiring.
    #[tokio::test]
    async fn config_update_shortens_interval_immediately() {
        let initial = Config {
            poll_interval_secs: 3600,
            ..Config::default()
        };
        let (tx, config_rx) = config_channel(initial);

        let poll_count = Arc::new(AtomicUsize::new(0));
        let poll_count_clone = poll_count.clone();

        struct CountingSource {
            info: DeviceInfo,
            count: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl BatterySource for CountingSource {
            fn device(&self) -> &DeviceInfo {
                &self.info
            }

            async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
                self.count.fetch_add(1, Ordering::Relaxed);
                Ok(reading_discharging(50))
            }
        }

        let (mut rx, _refresh) = Supervisor::spawn_with(config_rx, move || {
            let sources: Vec<Box<dyn BatterySource>> = vec![Box::new(CountingSource {
                info: device("mouse"),
                count: poll_count_clone.clone(),
            })];
            async move { sources }
        });

        wait_for_connected(&mut rx).await;
        let before = poll_count.load(Ordering::Relaxed);

        let shortened = Config {
            poll_interval_secs: 1,
            ..Config::default()
        };
        tx.send(shortened).expect("config receiver still alive");

        timeout(std::time::Duration::from_secs(5), async {
            loop {
                tokio::task::yield_now().await;
                if poll_count.load(Ordering::Relaxed) > before {
                    return;
                }
            }
        })
        .await
        .expect("poll count did not rise after config update");
    }

    // --- DeviceRegistry -------------------------------------------------

    /// Inserts a bare entry directly, bypassing `reconcile`, so `record`/
    /// `prune_stale` can be tested in isolation from task spawning.
    fn insert_entry(
        registry: &mut DeviceRegistry,
        info: &DeviceInfo,
        presence: Presence,
        last_reading: Option<BatteryReading>,
        last_seen: Option<Instant>,
        consecutive_failures: u32,
    ) {
        registry.order.push(info.id());
        registry.entries.insert(
            info.id(),
            DeviceEntry {
                info: info.clone(),
                last_reading,
                last_seen,
                presence,
                consecutive_failures,
                battery_history: Vec::new(),
            },
        );
    }

    fn source_ctx(config_rx: watch::Receiver<Config>) -> SourceCtx {
        let (tx, _rx) = mpsc::channel(64);
        SourceCtx {
            tx,
            config_rx,
            refresh: RefreshSignal::new(),
        }
    }

    #[test]
    fn registry_record_then_snapshot_preserves_discovery_order() {
        let mut registry = DeviceRegistry::new();
        let a = device("a");
        let b = device("b");
        insert_entry(&mut registry, &a, Presence::Unreachable, None, None, 0);
        insert_entry(&mut registry, &b, Presence::Unreachable, None, None, 0);

        registry.record(&a.id(), Some(reading_discharging(10)));
        registry.record(&b.id(), Some(reading_discharging(20)));

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].info.name, "a");
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(10)));
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[1].info.name, "b");
        assert_eq!(snapshot[1].last_reading, Some(reading_discharging(20)));
    }

    #[test]
    fn registry_ignores_reading_for_unregistered_id() {
        let mut registry = DeviceRegistry::new();
        let ghost = device("ghost");

        registry.record(&ghost.id(), Some(reading_discharging(99)));

        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn one_failed_poll_keeps_online_and_reading() {
        let mut registry = DeviceRegistry::new();
        let a = device("a");
        insert_entry(
            &mut registry,
            &a,
            Presence::Online,
            Some(reading_discharging(80)),
            Some(Instant::now()),
            0,
        );

        registry.record(&a.id(), None);

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));
    }

    #[test]
    fn offline_after_failures_flips_to_unreachable() {
        let mut registry = DeviceRegistry::new();
        let a = device("a");
        insert_entry(
            &mut registry,
            &a,
            Presence::Online,
            Some(reading_discharging(80)),
            Some(Instant::now()),
            0,
        );

        for _ in 0..OFFLINE_AFTER_FAILURES {
            registry.record(&a.id(), None);
        }

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Unreachable);
        // The reading stays retained even once Unreachable.
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));
    }

    #[test]
    fn success_after_failures_resets_counter_and_online() {
        let mut registry = DeviceRegistry::new();
        let a = device("a");
        insert_entry(
            &mut registry,
            &a,
            Presence::Unreachable,
            Some(reading_discharging(80)),
            Some(Instant::now()),
            OFFLINE_AFTER_FAILURES - 1,
        );

        registry.record(&a.id(), Some(reading_discharging(75)));
        assert_eq!(registry.entries[&a.id()].consecutive_failures, 0);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        // Counter was reset — a single further failure must not re-trip
        // Unreachable immediately.
        registry.record(&a.id(), None);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    #[tokio::test]
    async fn vanished_device_becomes_disconnected_keeps_reading_and_aborts_task() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new();
        let a = device("a");

        registry.reconcile(
            vec![Box::new(OkSource {
                info: a.clone(),
                reading: reading_discharging(80),
            })],
            &ctx,
        );
        registry.record(&a.id(), Some(reading_discharging(80)));
        assert!(registry.tasks.contains_key(&a.id()));

        // Next discovery sweep no longer sees the device.
        registry.reconcile(vec![], &ctx);

        assert!(!registry.tasks.contains_key(&a.id()));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].presence, Presence::Disconnected);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));
        assert!(snapshot[0].last_seen.is_some());
    }

    struct PanicSource {
        info: DeviceInfo,
    }

    #[async_trait::async_trait]
    impl BatterySource for PanicSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        // clippy::panic has no allow-in-tests config (unlike unwrap_used/expect_used);
        // this panic simulates a crashing source task for the respawn test below.
        #[allow(clippy::panic)]
        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            panic!("simulated source panic");
        }
    }

    /// A task that panics ends without ever calling `record`, so a stale
    /// `Online` reading from before the crash must not survive the sweep
    /// that notices `is_finished()`. Prints tokio's panic message to stderr
    /// during the test run — expected, not a failure.
    #[tokio::test]
    async fn crashed_task_is_demoted_and_respawned() {
        let (_tx, config_rx) = config_channel(Config::default());
        let (mpsc_tx, mut mpsc_rx) = mpsc::channel(64);
        let ctx = SourceCtx {
            tx: mpsc_tx,
            config_rx,
            refresh: RefreshSignal::new(),
        };
        let mut registry = DeviceRegistry::new();
        let a = device("a");

        registry.reconcile(vec![Box::new(PanicSource { info: a.clone() })], &ctx);
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(registry.tasks[&a.id()].is_finished());

        // Simulate the device having been Online with a stale reading right
        // before its task crashed.
        registry.record(&a.id(), Some(reading_discharging(80)));
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        // Next sweep still sees the device, with a fresh replacement source.
        registry.reconcile(
            vec![Box::new(OkSource {
                info: a.clone(),
                reading: reading_discharging(55),
            })],
            &ctx,
        );

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Unreachable);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));

        // The replacement task is running — its first poll proves the respawn.
        let (id, reading) = timeout(std::time::Duration::from_secs(5), mpsc_rx.recv())
            .await
            .expect("timed out waiting for replacement task's poll")
            .expect("mpsc channel closed");
        assert_eq!(id, a.id());
        registry.record(&id, reading);

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(55)));
    }

    /// A device that legitimately vanishes from discovery is aborted, not
    /// mistaken for a crash: `reconcile` only inspects `is_finished()` for
    /// ids still present in the fresh set, so the vanished path here never
    /// runs the crash-detection branch above.
    #[tokio::test]
    async fn vanished_device_is_not_reported_as_crashed() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new();
        let a = device("a");

        registry.reconcile(
            vec![Box::new(OkSource {
                info: a.clone(),
                reading: reading_discharging(80),
            })],
            &ctx,
        );
        registry.record(&a.id(), Some(reading_discharging(80)));

        registry.reconcile(vec![], &ctx);

        assert!(!registry.tasks.contains_key(&a.id()));
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    #[tokio::test]
    async fn disconnected_without_a_reading_is_dropped_immediately() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new();
        let a = device("a");

        // Never polled successfully before it vanishes — nothing to retain.
        registry.reconcile(vec![Box::new(ErrSource { info: a.clone() })], &ctx);
        registry.reconcile(vec![], &ctx);

        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn reappeared_device_reuses_entry_and_returns_online() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new();
        let a = device("a");

        registry.reconcile(
            vec![Box::new(OkSource {
                info: a.clone(),
                reading: reading_discharging(80),
            })],
            &ctx,
        );
        registry.record(&a.id(), Some(reading_discharging(80)));

        // Vanishes, then reappears.
        registry.reconcile(vec![], &ctx);
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

        registry.reconcile(
            vec![Box::new(OkSource {
                info: a.clone(),
                reading: reading_discharging(80),
            })],
            &ctx,
        );

        // Same entry reused, not duplicated.
        assert_eq!(registry.snapshot().len(), 1);
        // Retained reading still visible until the fresh task proves online.
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

        registry.record(&a.id(), Some(reading_discharging(80)));
        assert_eq!(registry.snapshot().len(), 1);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    #[test]
    fn disconnected_entry_older_than_cap_is_pruned() {
        let mut registry = DeviceRegistry::new();
        let a = device("a");
        let last_seen = Instant::now();
        insert_entry(
            &mut registry,
            &a,
            Presence::Disconnected,
            Some(reading_discharging(80)),
            Some(last_seen),
            0,
        );

        // Still within the cap.
        registry.prune_stale(last_seen + DISCONNECTED_RETENTION);
        assert_eq!(registry.snapshot().len(), 1);

        // Past the cap.
        registry.prune_stale(last_seen + DISCONNECTED_RETENTION + Duration::from_secs(1));
        assert!(registry.snapshot().is_empty());
    }
}
