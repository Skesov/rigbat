use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until};

use super::migration::ShownDevicesMigration;
use crate::config::Config;
use crate::discovery::BackendSweep;
use crate::domain::estimate::estimate as estimate_remaining;
use crate::domain::{
    BatteryReading, BootTime, DeviceId, DeviceInfo, DeviceKind, DeviceState, Estimate, Presence,
    TrayState,
};
use crate::refresh::{RefreshSignal, RefreshWaiter};
use crate::sources::hidraw::NodeReassigned;
use crate::sources::{AccessDenied, BatterySource, Context};
use crate::state;

const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);

/// Wireless receivers drop packets routinely; 3 misses in a row is a real outage.
const OFFLINE_AFTER_FAILURES: u32 = 3;

/// How long a `Disconnected` device stays in the roster: a device used
/// yesterday is still listed today.
const DISCONNECTED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Bounds the change-point history per device.
const HISTORY_CAP: usize = 20;

pub struct Supervisor;

/// Whether this process may write `config.json`. Only the tray, which holds
/// the single-instance name, owns it; `--waybar` can run beside a tray, and
/// two processes converting the same legacy whitelist against two rosters
/// would race with a destructive outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigRole {
    Owner,
    Reader,
}

impl Supervisor {
    /// Spawns the manager over the real discovery and config file.
    pub fn spawn(
        config_tx: watch::Sender<Config>,
        ctx: Arc<Context>,
        store: Option<state::Store>,
        role: ConfigRole,
    ) -> (watch::Receiver<TrayState>, RefreshSignal) {
        let save: fn(&Config) -> anyhow::Result<()> = match role {
            ConfigRole::Owner => crate::config::save,
            ConfigRole::Reader => |_cfg| Ok(()),
        };
        Self::spawn_with(
            config_tx,
            move || {
                let ctx = ctx.clone();
                Box::pin(async move { crate::discovery::discover_all(&ctx).await })
            },
            crate::config::load,
            save,
            store,
            role,
        )
    }

    /// Takes injectable discovery and config IO, so tests never touch the
    /// real config file. `discover` runs at start, on every discovery tick
    /// and on every refresh.
    fn spawn_with<F, Fut, L, S>(
        config_tx: watch::Sender<Config>,
        discover: F,
        load: L,
        save: S,
        store: Option<state::Store>,
        role: ConfigRole,
    ) -> (watch::Receiver<TrayState>, RefreshSignal)
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Vec<BackendSweep>> + Send + 'static,
        L: Fn() -> Config + Send + 'static,
        S: Fn(&Config) -> anyhow::Result<()> + Send + 'static,
    {
        let (watch_tx, watch_rx) = watch::channel(TrayState { devices: vec![] });
        let refresh = RefreshSignal::new();
        tokio::spawn(manager_task(
            discover,
            watch_tx,
            refresh.clone(),
            ConfigIo {
                tx: config_tx,
                load,
                save,
            },
            store,
            ShownDevicesMigration::new(role == ConfigRole::Owner),
        ));
        (watch_rx, refresh)
    }
}

/// A device rename the state store detected, for the config to follow.
#[derive(Debug, PartialEq, Eq)]
struct Rename {
    from: String,
    to: String,
}

/// Per-device bookkeeping behind the `DeviceState` snapshot; the failure
/// count stays here because nothing outside the registry needs it.
struct DeviceEntry {
    info: DeviceInfo,
    last_reading: Option<BatteryReading>,
    last_seen: Option<BootTime>,
    presence: Presence,
    consecutive_failures: u32,
    /// Percent-change points feeding the remaining-time estimate.
    battery_history: Vec<(BootTime, u8)>,
}

impl DeviceEntry {
    fn new(
        info: DeviceInfo,
        presence: Presence,
        latest: Option<(BootTime, BatteryReading)>,
        battery_history: Vec<(BootTime, u8)>,
    ) -> Self {
        Self {
            info,
            last_reading: latest.map(|(_, r)| r),
            last_seen: latest.map(|(at, _)| at),
            presence,
            consecutive_failures: 0,
            battery_history,
        }
    }

    /// Proof the device cannot answer, not a missed poll: demotes at once,
    /// and the saturated counter skips the grace on the next miss.
    fn mark_dead(&mut self, presence: Presence) {
        self.presence = presence;
        self.consecutive_failures = OFFLINE_AFTER_FAILURES;
    }

    fn estimate(&self) -> Estimate {
        match self.last_reading {
            Some(r) => estimate_remaining(&self.battery_history, r),
            None => Estimate::Unknown,
        }
    }
}

/// Only change points carry a discharge rate, so an equal percent is
/// dropped; an increase (charging, a battery swap) invalidates the trend
/// and clears it. Shared by live polls and `seed_history`, so a replayed
/// reading is reduced exactly like a live one.
fn push_history_point(history: &mut Vec<(BootTime, u8)>, now: BootTime, percent: u8) {
    if let Some(&(_, last)) = history.last() {
        match percent.cmp(&last) {
            std::cmp::Ordering::Equal => return,
            std::cmp::Ordering::Greater => history.clear(),
            std::cmp::Ordering::Less => {}
        }
    }
    history.push((now, percent));
    if history.len() > HISTORY_CAP {
        history.remove(0);
    }
}

/// A new entry's change-point history from the store, so the estimate need
/// not wait out a fresh window after every tray restart.
async fn seed_history(
    store: &state::Store,
    id: &DeviceId,
    now: BootTime,
    now_unix: i64,
) -> Vec<(BootTime, u8)> {
    let rows = match store.recent_readings(id, HISTORY_CAP).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(device = %id.name, "state store: failed to load history for seeding: {e:#}");
            return Vec::new();
        }
    };
    let mut history = Vec::new();
    for (at, percent) in rows.into_iter().rev() {
        let Some(at) = boot_time_at(at, now, now_unix) else {
            continue;
        };
        push_history_point(&mut history, at, percent);
    }
    history
}

/// The device's newest stored reading, so a restart does not blank what the
/// last run knew. `None` without one, or on a store error.
async fn latest_reading(
    store: &state::Store,
    id: &DeviceId,
    now: BootTime,
    now_unix: i64,
) -> Option<(BootTime, BatteryReading)> {
    let (at, reading) = store.latest_reading(id).await.unwrap_or_else(|e| {
        tracing::warn!(device = %id.name, "state store: failed to load the latest reading: {e:#}");
        None
    })?;
    Some((boot_time_at(at, now, now_unix)?, reading))
}

/// A stored wall-clock time as a `BootTime`, by its age — negative for a
/// reading from before this boot.
fn boot_time_at(at_unix: i64, now: BootTime, now_unix: i64) -> Option<BootTime> {
    let age_secs = u64::try_from(now_unix.saturating_sub(at_unix)).unwrap_or(0);
    now.checked_sub(Duration::from_secs(age_secs))
}

/// Whether a `Disconnected` entry seen at `last_seen` has outlived
/// `DISCONNECTED_RETENTION`. One that never produced a reading has nothing to
/// retain.
fn outlived(last_seen: Option<BootTime>, now: BootTime) -> bool {
    last_seen.is_none_or(|t| now.saturating_duration_since(t) > DISCONNECTED_RETENTION)
}

/// Live set of discovered devices and their polling tasks, keyed by identity.
/// `order` keeps the tray roster stable. A device that leaves discovery, or
/// one restored from the store at startup, keeps its entry without a task.
struct DeviceRegistry {
    order: Vec<DeviceId>,
    entries: HashMap<DeviceId, DeviceEntry>,
    tasks: HashMap<DeviceId, SourceTask>,
    /// Last generation handed to a spawned task; see `SourceMsg`.
    last_generation: u64,
    store: Option<state::Store>,
}

struct SourceTask {
    handle: AbortHandle,
    generation: u64,
    /// The backend whose failed sweep must not retire the device. Not derived
    /// from the transport: several backends share `Transport::Hidraw`.
    backend: &'static str,
}

/// `generation` drops a message a retired task queued before its replacement was spawned.
struct SourceMsg {
    id: DeviceId,
    generation: u64,
    event: SourceEvent,
}

enum SourceEvent {
    Polled(Option<BatteryReading>),
    AccessDenied,
    /// The node the source opens now belongs to another device; the task has ended.
    NodeReassigned,
}

/// What spawning a source task needs.
struct SourceCtx {
    tx: mpsc::Sender<SourceMsg>,
    config_rx: watch::Receiver<Config>,
    refresh: RefreshSignal,
}

impl DeviceRegistry {
    fn new(store: Option<state::Store>) -> Self {
        Self {
            order: Vec::new(),
            entries: HashMap::new(),
            tasks: HashMap::new(),
            last_generation: 0,
            store,
        }
    }

    fn is_live(&self, id: &DeviceId, generation: u64) -> bool {
        self.tasks
            .get(id)
            .is_some_and(|task| task.generation == generation)
    }

    fn spawn(
        &mut self,
        src: Box<dyn BatterySource>,
        id: DeviceId,
        backend: &'static str,
        ctx: &SourceCtx,
    ) {
        self.last_generation = self.last_generation.wrapping_add(1);
        let generation = self.last_generation;
        let handle = spawn_source_task(src, id.clone(), generation, ctx);
        self.tasks.insert(
            id,
            SourceTask {
                handle,
                generation,
                backend,
            },
        );
    }

    fn apply(&mut self, msg: SourceMsg) {
        let SourceMsg {
            id,
            generation,
            event,
        } = msg;
        match event {
            SourceEvent::Polled(reading) => self.record(&id, generation, reading),
            SourceEvent::AccessDenied => self.deny(&id, generation),
            SourceEvent::NodeReassigned => self.retire(&id, generation),
        }
    }

    /// Ignores a result from any generation but the live one: a poll can win
    /// a race against the abort of its own task. A failure demotes only
    /// after `OFFLINE_AFTER_FAILURES` in a row.
    fn record(&mut self, id: &DeviceId, generation: u64, reading: Option<BatteryReading>) {
        if !self.is_live(id, generation) {
            return;
        }
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        match reading {
            Some(r) => {
                let now = crate::clock::now();
                push_history_point(&mut entry.battery_history, now, r.percent);
                entry.last_reading = Some(r);
                entry.last_seen = Some(now);
                entry.consecutive_failures = 0;
                entry.presence = Presence::Online;
                if let Some(store) = &self.store {
                    store.record_reading(id, r, state::now_unix());
                }
            }
            None => {
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                if entry.consecutive_failures >= OFFLINE_AFTER_FAILURES {
                    entry.presence = Presence::Unreachable;
                }
            }
        }
    }

    /// Spawns a task for each new or reappeared device, aborts (without
    /// forgetting) the ones that vanished, and prunes long-`Disconnected`
    /// entries. A backend whose sweep failed is skipped by both passes: one
    /// failed sweep must not be more trigger-happy than
    /// `OFFLINE_AFTER_FAILURES` missed polls.
    async fn reconcile(&mut self, sweeps: Vec<BackendSweep>, ctx: &SourceCtx) -> Vec<Rename> {
        let mut renames = Vec::new();
        let now_boot = crate::clock::now();
        let now_unix = state::now_unix();

        let mut found: Vec<(&'static str, Box<dyn BatterySource>)> = Vec::new();
        let mut succeeded_backends: Vec<&'static str> = Vec::new();
        for BackendSweep { name, result } in sweeps {
            // An error was already logged once, by `discovery::discover_all`.
            if let Ok(sources) = result {
                succeeded_backends.push(name);
                found.extend(sources.into_iter().map(|src| (name, src)));
            }
        }

        let devices = found
            .iter()
            .map(|(_, src)| (src.device().id(), src.device().kind))
            .collect();
        let mut seen = self.record_seen(devices, now_unix).await.into_iter();
        let mut fresh_ids: Vec<DeviceId> = Vec::new();
        for (name, src) in found {
            fresh_ids.push(src.device().id());
            if let Some(rename) = self
                .admit(src, name, seen.next(), now_boot, now_unix, ctx)
                .await
            {
                renames.push(rename);
            }
        }

        self.demote_crashed(&fresh_ids, &succeeded_backends);
        self.retire_vanished(&fresh_ids, &succeeded_backends);
        self.prune_stale(crate::clock::now());
        renames
    }

    /// Upserts the inventory row of every device a sweep found, whether it is
    /// brand new, still running, or reappearing. Empty without a store or on
    /// a store error.
    async fn record_seen(
        &self,
        devices: Vec<(DeviceId, DeviceKind)>,
        now_unix: i64,
    ) -> Vec<state::Seen> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        store
            .record_seen(devices, now_unix)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("state store: failed to record the devices seen: {e:#}");
                Vec::new()
            })
    }

    /// One device a successful sweep found: follows a rename, then keeps a
    /// healthy task, respawns a crashed one, or adds the entry and spawns.
    async fn admit(
        &mut self,
        src: Box<dyn BatterySource>,
        backend: &'static str,
        seen: Option<state::Seen>,
        now_boot: BootTime,
        now_unix: i64,
        ctx: &SourceCtx,
    ) -> Option<Rename> {
        let id = src.device().id();
        let rename = match seen {
            Some(state::Seen::Renamed { from }) => Some(self.follow_rename(&id, from)),
            _ => None,
        };
        match self.tasks.get(&id).map(|task| task.handle.is_finished()) {
            // Healthy: dropping `src` drops only the transient handle.
            Some(false) => {}
            Some(true) => self.respawn_crashed(src, id, backend, ctx),
            None => {
                self.add_or_reuse_entry(src.device(), now_boot, now_unix)
                    .await;
                self.spawn(src, id, backend, ctx);
            }
        }
        rename
    }

    /// Drops the entry under the old name now: left to `retire_vanished` it
    /// would show as a `Disconnected` duplicate for `DISCONNECTED_RETENTION`.
    fn follow_rename(&mut self, id: &DeviceId, from: String) -> Rename {
        tracing::info!(
            device = %id.name,
            previous = %from,
            "device renamed; inventory row and settings follow it"
        );
        self.forget(&DeviceId {
            name: from.clone(),
            ..id.clone()
        });
        Rename {
            from,
            to: id.name.clone(),
        }
    }

    /// `is_finished()` cannot tell a panic from an abort, but `retire_vanished`
    /// removes a handle in the same call that aborts it, so a finished task
    /// still listed for a device the sweep found ended by itself.
    fn respawn_crashed(
        &mut self,
        src: Box<dyn BatterySource>,
        id: DeviceId,
        backend: &'static str,
        ctx: &SourceCtx,
    ) {
        tracing::error!(
            device = %src.device().name,
            "polling task ended unexpectedly; restarting it"
        );
        self.tasks.remove(&id);
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.mark_dead(Presence::Unreachable);
        }
        self.spawn(src, id, backend, ctx);
    }

    /// A reappearing device keeps its entry, retained reading and presence
    /// until its fresh task proves it online; a new one starts from what the
    /// store remembers.
    async fn add_or_reuse_entry(&mut self, info: &DeviceInfo, now_boot: BootTime, now_unix: i64) {
        let id = info.id();
        match self.entries.entry(id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                tracing::info!(device = %info.name, "device reappeared");
                slot.get_mut().info = info.clone();
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                tracing::info!(device = %info.name, "device appeared");
                let (latest, history) = match &self.store {
                    Some(store) => (
                        latest_reading(store, &id, now_boot, now_unix).await,
                        seed_history(store, &id, now_boot, now_unix).await,
                    ),
                    None => (None, Vec::new()),
                };
                slot.insert(DeviceEntry::new(
                    info.clone(),
                    Presence::Unreachable,
                    latest,
                    history,
                ));
                self.order.push(id);
            }
        }
    }

    /// A task whose backend's sweep failed escapes the crash check in `admit`
    /// and is not retired either. Respawning needs a source only a successful
    /// sweep hands over, so the entry is demoted and the dead handle dropped.
    fn demote_crashed(&mut self, fresh_ids: &[DeviceId], succeeded_backends: &[&'static str]) {
        let crashed: Vec<DeviceId> = self
            .tasks
            .iter()
            .filter(|(id, task)| !fresh_ids.contains(id) && task.handle.is_finished())
            .filter(|(_, task)| !succeeded_backends.contains(&task.backend))
            .map(|(id, _)| id.clone())
            .collect();
        for id in crashed {
            self.tasks.remove(&id);
            if let Some(entry) = self.entries.get_mut(&id) {
                tracing::error!(
                    device = %entry.info.name,
                    "polling task ended unexpectedly while its backend was failing; \
                     marking unreachable until discovery can respawn it"
                );
                entry.mark_dead(Presence::Unreachable);
            }
        }
    }

    fn retire_vanished(&mut self, fresh_ids: &[DeviceId], succeeded_backends: &[&'static str]) {
        let vanished: Vec<DeviceId> = self
            .tasks
            .iter()
            .filter(|(id, task)| {
                !fresh_ids.contains(id) && succeeded_backends.contains(&task.backend)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in vanished {
            self.disconnect(&id);
            if let Some(entry) = self.entries.get(&id) {
                tracing::info!(device = %entry.info.name, "device vanished");
            }
        }
    }

    /// Immediate, unlike `Unreachable`: a denial is not a dropped packet.
    fn deny(&mut self, id: &DeviceId, generation: u64) {
        if !self.is_live(id, generation) {
            return;
        }
        if let Some(entry) = self.entries.get_mut(id) {
            // Once access is granted, a silent device reads Unreachable at its first miss.
            entry.mark_dead(Presence::NoAccess);
        }
    }

    /// Treated as the device leaving, so the next sweep respawns it wherever it is now.
    fn retire(&mut self, id: &DeviceId, generation: u64) {
        if self.is_live(id, generation) {
            self.disconnect(id);
        }
    }

    fn disconnect(&mut self, id: &DeviceId) {
        self.abort_task(id);
        if let Some(entry) = self.entries.get_mut(id) {
            entry.presence = Presence::Disconnected;
            // The failures before the disconnect belong to the connection that
            // ended; kept, one miss after reconnecting would skip the grace.
            entry.consecutive_failures = 0;
        }
    }

    /// Drops `Disconnected` entries not seen for `DISCONNECTED_RETENTION`.
    /// `now` is a parameter so tests can age an entry without a sleep.
    fn prune_stale(&mut self, now: BootTime) {
        let stale: Vec<DeviceId> = self
            .entries
            .iter()
            .filter(|(_, e)| e.presence == Presence::Disconnected && outlived(e.last_seen, now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.forget(&id);
        }
    }

    /// Adds each inventory device no sweep has found yet whose stored reading
    /// is within `DISCONNECTED_RETENTION`, as `Disconnected` with that
    /// reading: a device that dropped before a restart reads the same as one
    /// that dropped while the tray ran, and ages out the same way.
    async fn restore_retained(&mut self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let records = match store.list_devices().await {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!("state store: failed to list devices to restore: {e:#}");
                return;
            }
        };
        let now_boot = crate::clock::now();
        let now_unix = state::now_unix();
        for record in records {
            let id = record.device;
            if self.entries.contains_key(&id) {
                continue;
            }
            let latest = latest_reading(&store, &id, now_boot, now_unix).await;
            if outlived(latest.map(|(at, _)| at), now_boot) {
                continue;
            }
            let history = seed_history(&store, &id, now_boot, now_unix).await;
            let info = DeviceInfo {
                name: id.name.clone(),
                kind: record.kind,
                transport: id.transport,
                locator: id.locator.clone(),
            };
            self.entries.insert(
                id.clone(),
                DeviceEntry::new(info, Presence::Disconnected, latest, history),
            );
            self.order.push(id);
        }
    }

    /// Drops every trace of one identity: its task, its entry, its place in the order.
    fn forget(&mut self, id: &DeviceId) {
        self.abort_task(id);
        self.entries.remove(id);
        self.order.retain(|oid| oid != id);
    }

    fn abort_task(&mut self, id: &DeviceId) {
        if let Some(task) = self.tasks.remove(id) {
            task.handle.abort();
        }
    }

    fn names(&self) -> Vec<String> {
        self.snapshot().into_iter().map(|d| d.info.name).collect()
    }

    fn snapshot(&self) -> Vec<DeviceState> {
        self.order
            .iter()
            .filter_map(|id| {
                let entry = self.entries.get(id)?;
                Some(DeviceState {
                    info: entry.info.clone(),
                    last_reading: entry.last_reading,
                    last_seen: entry.last_seen,
                    presence: entry.presence,
                    estimate: entry.estimate(),
                })
            })
            .collect()
    }
}

/// The config channel and the file behind it, injectable for tests.
struct ConfigIo<L, S> {
    tx: watch::Sender<Config>,
    load: L,
    save: S,
}

impl<L, S> ConfigIo<L, S>
where
    L: Fn() -> Config,
    S: Fn(&Config) -> anyhow::Result<()>,
{
    /// Moves per-device settings onto each device's new name, saves, and
    /// republishes so the tray applies them before the file watch fires.
    /// Reads the file fresh, not the channel: the settings window may have
    /// saved an edit the in-memory copy predates. Not async, so the borrows
    /// never cross an `.await` and `L`/`S` need not be `Sync`.
    fn apply_renames(&self, renames: &[Rename]) {
        if renames.is_empty() {
            return;
        }
        let mut cfg = (self.load)();
        let mut changed = false;
        for Rename { from, to } in renames {
            changed |= crate::config::rename_device(&mut cfg, from, to);
        }
        if !changed {
            return;
        }
        match (self.save)(&cfg) {
            Ok(()) => {
                let _ = self.tx.send(cfg);
            }
            Err(e) => tracing::warn!("failed to save config after a device rename: {e:#}"),
        }
    }

    fn migrate(&self, migration: &mut ShownDevicesMigration, registry: &DeviceRegistry) {
        migration.attempt(&registry.names(), &self.tx, &self.load, &self.save);
    }
}

async fn manager_task<F, Fut, L, S>(
    discover: F,
    watch_tx: watch::Sender<TrayState>,
    refresh: RefreshSignal,
    config: ConfigIo<L, S>,
    store: Option<state::Store>,
    mut migration: ShownDevicesMigration,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Vec<BackendSweep>> + Send + 'static,
    L: Fn() -> Config + Send + 'static,
    S: Fn(&Config) -> anyhow::Result<()> + Send + 'static,
{
    let (mpsc_tx, mut mpsc_rx) = mpsc::channel::<SourceMsg>(64);
    let mut waiter = refresh.sweep_waiter();
    let ctx = SourceCtx {
        tx: mpsc_tx,
        config_rx: config.tx.subscribe(),
        refresh,
    };

    let mut registry = DeviceRegistry::new(store);

    let renames = registry.reconcile(discover().await, &ctx).await;
    registry.restore_retained().await;
    // Sent even when unchanged: consumers wait on it as "the first sweep is in".
    watch_tx.send_replace(TrayState {
        devices: registry.snapshot(),
    });
    config.apply_renames(&renames);
    config.migrate(&mut migration, &registry);

    // Outlives iterations: a per-iteration sleep restarts on every reading and starves.
    let mut sweep_timer = interval_at(Instant::now() + DISCOVERY_INTERVAL, DISCOVERY_INTERVAL);
    sweep_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = mpsc_rx.recv() => {
                // All source tasks dropped their senders — nothing left to do.
                let Some(msg) = msg else { return };
                registry.apply(msg);
                publish(&registry, &watch_tx);
                continue;
            }
            _ = sweep_timer.tick() => {}
            _ = waiter.wait() => sweep_timer.reset(),
        }
        let renames = rediscover(&mut registry, discover(), &ctx, &watch_tx).await;
        config.apply_renames(&renames);
        config.migrate(&mut migration, &registry);
    }
}

// Takes the discovery future, not the closure: holding `&F` across the
// `.await` would require `F: Sync`, which the public API does not promise.
async fn rediscover<Fut>(
    registry: &mut DeviceRegistry,
    discover: Fut,
    ctx: &SourceCtx,
    watch_tx: &watch::Sender<TrayState>,
) -> Vec<Rename>
where
    Fut: Future<Output = Vec<BackendSweep>>,
{
    let sweeps = discover.await;
    let renames = registry.reconcile(sweeps, ctx).await;
    publish(registry, watch_tx);
    renames
}

fn spawn_source_task(
    mut src: Box<dyn BatterySource>,
    id: DeviceId,
    generation: u64,
    ctx: &SourceCtx,
) -> AbortHandle {
    let name = src.device().name.clone();
    let tx = ctx.tx.clone();
    let mut config_rx = ctx.config_rx.clone();
    let mut waiter = ctx.refresh.waiter();

    let handle = tokio::spawn(async move {
        // So a permanently absent device (a mouse left switched off) logs its
        // reason once at `warn` instead of every interval forever.
        let mut failures_in_a_row: u32 = 0;
        let mut pushed: Option<BatteryReading> = None;

        loop {
            let polled = match pushed.take() {
                Some(r) => Ok(r),
                None => src.poll().await,
            };
            let event = match polled {
                Ok(r) => {
                    failures_in_a_row = 0;
                    SourceEvent::Polled(Some(r))
                }
                Err(e) if e.is::<NodeReassigned>() => {
                    tracing::info!(device = %name, "{e:#}; retiring its source");
                    let event = SourceEvent::NodeReassigned;
                    let _ = tx
                        .send(SourceMsg {
                            id,
                            generation,
                            event,
                        })
                        .await;
                    return;
                }
                Err(e) => {
                    if failures_in_a_row == 0 {
                        tracing::warn!(device = %name, "poll failed: {e:#}");
                    } else {
                        tracing::debug!(device = %name, failures = failures_in_a_row, "poll failed: {e:#}");
                    }
                    failures_in_a_row = failures_in_a_row.saturating_add(1);
                    if e.is::<AccessDenied>() {
                        SourceEvent::AccessDenied
                    } else {
                        SourceEvent::Polled(None)
                    }
                }
            };
            let msg = SourceMsg {
                id: id.clone(),
                generation,
                event,
            };
            if tx.send(msg).await.is_err() {
                return;
            }
            pushed = wait_for_next_poll(src.as_mut(), &mut config_rx, &mut waiter, &name).await;
        }
    });
    handle.abort_handle()
}

/// Waits out this device's poll interval, or until a refresh. Returns the
/// reading the device pushed meanwhile, if it did.
async fn wait_for_next_poll(
    src: &mut dyn BatterySource,
    config_rx: &mut watch::Receiver<Config>,
    waiter: &mut RefreshWaiter,
    name: &str,
) -> Option<BatteryReading> {
    // effective_poll_interval_secs clamps to >= 1, so a hostile 0 in
    // config.json cannot spin the poll loop. The borrow ends in the
    // closure: a watch::Ref must never be held across an .await.
    let polled_at = Instant::now();
    let next_poll = |config_rx: &watch::Receiver<Config>| {
        polled_at + Duration::from_secs(config_rx.borrow().effective_poll_interval_secs(name))
    };
    let mut due = next_poll(config_rx);
    let mut config_open = true;

    loop {
        tokio::select! {
            _ = sleep_until(due) => return None,
            _ = waiter.wait() => return None,
            // Only this device's interval moves the next poll; a
            // palette or language change polls nothing.
            r = config_rx.changed(), if config_open => match r {
                Ok(()) => due = next_poll(config_rx),
                Err(_) => config_open = false,
            },
            r = src.pushed() => return Some(r),
        }
    }
}

/// Notifies receivers only when the published state differs from the last.
fn publish(registry: &DeviceRegistry, watch_tx: &watch::Sender<TrayState>) {
    let devices = registry.snapshot();
    watch_tx.send_if_modified(|current| {
        if current.devices == devices {
            return false;
        }
        current.devices = devices;
        true
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::time::timeout;

    use super::*;
    use crate::app::test_support::{denied, device, err, fake, ok, panics, reading, reassigned};
    use crate::domain::Transport;
    use crate::test_support::ScratchDir;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Stand-ins for `config::load`/`config::save`, so no test reads or
    /// rewrites the developer's real config file.
    fn no_migration_load() -> Config {
        Config::default()
    }

    fn no_migration_save(_cfg: &Config) -> anyhow::Result<()> {
        Ok(())
    }

    fn spawn_test<F, Fut>(
        config_tx: watch::Sender<Config>,
        discover: F,
    ) -> (watch::Receiver<TrayState>, RefreshSignal)
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Vec<BackendSweep>> + Send + 'static,
    {
        Supervisor::spawn_with(
            config_tx,
            discover,
            no_migration_load,
            no_migration_save,
            None,
            ConfigRole::Owner,
        )
    }

    fn ok_sweep(name: &'static str, sources: Vec<Box<dyn BatterySource>>) -> BackendSweep {
        BackendSweep {
            name,
            result: Ok(sources),
        }
    }

    /// A discovery error, distinct from an honest `ok_sweep(name, vec![])`.
    fn err_sweep(name: &'static str, msg: &str) -> BackendSweep {
        BackendSweep {
            name,
            result: Err(anyhow::anyhow!(msg.to_owned())),
        }
    }

    /// One successful `sysfs` sweep that finds `sources`.
    fn found(
        sources: Vec<Box<dyn BatterySource>>,
    ) -> impl Future<Output = Vec<BackendSweep>> + Send {
        let sweeps = vec![ok_sweep("sysfs", sources)];
        async move { sweeps }
    }

    fn counting(info: &DeviceInfo, polls: &Arc<AtomicUsize>) -> Box<dyn BatterySource> {
        let polls = polls.clone();
        fake(info, move || {
            polls.fetch_add(1, Ordering::Relaxed);
            Ok(reading(50))
        })
    }

    async fn wait_for(
        rx: &mut watch::Receiver<TrayState>,
        what: &str,
        done: impl Fn(&TrayState) -> bool,
    ) -> TrayState {
        timeout(TIMEOUT, async {
            loop {
                {
                    let state = rx.borrow().clone();
                    if done(&state) {
                        return state;
                    }
                }
                rx.changed().await.expect("watch channel closed");
            }
        })
        .await
        .map_err(|_| format!("timed out waiting for {what}"))
        .unwrap()
    }

    async fn wait_for_connected(rx: &mut watch::Receiver<TrayState>) -> TrayState {
        wait_for(rx, "a device online", |state| {
            state.devices.iter().any(|d| d.presence == Presence::Online)
        })
        .await
    }

    async fn wait_for_devices(rx: &mut watch::Receiver<TrayState>) -> TrayState {
        wait_for(rx, "devices to appear", |state| !state.devices.is_empty()).await
    }

    /// Sources are instant; this gives the spawned tasks a chance to run.
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn primary_is_first_connected() {
        let (tx, _config_rx) = watch::channel(Config::default());
        let (mut rx, _refresh) = spawn_test(tx, || {
            found(vec![err(&device("mouse")), ok(&device("keyboard"), 80)])
        });

        let state = wait_for_connected(&mut rx).await;
        assert_eq!(state.devices.len(), 2);
        let keyboard = state
            .devices
            .iter()
            .find(|d| d.info.name == "keyboard")
            .expect("keyboard present");
        assert_eq!(keyboard.last_reading, Some(reading(80)));
    }

    #[tokio::test]
    async fn all_err_stays_offline() {
        let (tx, _config_rx) = watch::channel(Config::default());
        let (rx, _refresh) = spawn_test(tx, || {
            found(vec![err(&device("mouse")), err(&device("keyboard"))])
        });
        settle().await;

        let state = rx.borrow().clone();
        assert_eq!(state.devices.len(), 2);
        assert!(state.devices.iter().all(|d| d.last_reading.is_none()));
    }

    #[tokio::test]
    async fn empty_sources() {
        let (tx, _config_rx) = watch::channel(Config::default());
        let (rx, _refresh) = spawn_test(tx, || async { Vec::<BackendSweep>::new() });
        settle().await;

        assert!(rx.borrow().devices.is_empty());
    }

    #[tokio::test]
    async fn hotplug_adds_device() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, _config_rx) = watch::channel(Config::default());
        let (mut rx, refresh) = spawn_test(tx, move || {
            let sources = if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                vec![]
            } else {
                vec![ok(&device("kbd"), 70)]
            };
            found(sources)
        });
        settle().await;
        assert!(rx.borrow().devices.is_empty());

        refresh.trigger();

        let state = wait_for_devices(&mut rx).await;
        assert_eq!(state.devices.len(), 1);
        assert_eq!(state.devices[0].info.name, "kbd");
    }

    /// The interval is an hour, so a rising poll count within the timeout
    /// can only come from the `config_rx.changed()` arm, not the sleep.
    #[tokio::test]
    async fn config_update_shortens_interval_immediately() {
        let initial = Config {
            poll_interval_secs: 3600,
            ..Config::default()
        };
        let (tx, _config_rx) = watch::channel(initial);
        let polls = Arc::new(AtomicUsize::new(0));
        let (mut rx, _refresh) = spawn_test(tx.clone(), {
            let polls = polls.clone();
            move || found(vec![counting(&device("mouse"), &polls)])
        });

        wait_for_connected(&mut rx).await;
        let before = polls.load(Ordering::Relaxed);

        let shortened = Config {
            poll_interval_secs: 1,
            ..Config::default()
        };
        tx.send(shortened).expect("config receiver still alive");

        timeout(TIMEOUT, async {
            loop {
                tokio::task::yield_now().await;
                if polls.load(Ordering::Relaxed) > before {
                    return;
                }
            }
        })
        .await
        .expect("poll count did not rise after config update");
    }

    /// A change that does not touch the polling interval (palette, theme,
    /// language) re-renders, it does not send a query to every device.
    #[tokio::test(start_paused = true)]
    async fn a_cosmetic_config_change_polls_nothing() {
        let initial = Config {
            poll_interval_secs: 3600,
            ..Config::default()
        };
        let (tx, _config_rx) = watch::channel(initial.clone());
        let polls = Arc::new(AtomicUsize::new(0));
        let (mut rx, _refresh) = spawn_test(tx.clone(), {
            let polls = polls.clone();
            move || found(vec![counting(&device("mouse"), &polls)])
        });
        wait_for_connected(&mut rx).await;
        let before = polls.load(Ordering::Relaxed);

        tx.send(Config {
            palette: crate::domain::Palette::Nord,
            language: Some("ru".to_owned()),
            ..initial
        })
        .expect("config receiver still alive");
        tokio::time::sleep(Duration::from_secs(10)).await;

        assert_eq!(polls.load(Ordering::Relaxed), before);
    }

    /// Inserts a bare entry directly, bypassing `reconcile`, so `record`/
    /// `prune_stale` can be tested in isolation from task spawning.
    fn insert_entry(
        registry: &mut DeviceRegistry,
        info: &DeviceInfo,
        presence: Presence,
        last_reading: Option<BatteryReading>,
        last_seen: Option<BootTime>,
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

    fn insert_online(registry: &mut DeviceRegistry, info: &DeviceInfo, percent: u8) {
        insert_entry(
            registry,
            info,
            Presence::Online,
            Some(reading(percent)),
            Some(crate::clock::now()),
            0,
        );
    }

    /// The context, the receiving end of its reports, and the config sender,
    /// which must outlive the test so `config_rx.changed()` waits instead of
    /// erroring.
    fn source_ctx() -> (SourceCtx, mpsc::Receiver<SourceMsg>, watch::Sender<Config>) {
        let (config_tx, config_rx) = watch::channel(Config::default());
        let (tx, rx) = mpsc::channel(64);
        let ctx = SourceCtx {
            tx,
            config_rx,
            refresh: RefreshSignal::new(),
        };
        (ctx, rx, config_tx)
    }

    async fn sweep(
        registry: &mut DeviceRegistry,
        ctx: &SourceCtx,
        sources: Vec<Box<dyn BatterySource>>,
    ) -> Vec<Rename> {
        registry
            .reconcile(vec![ok_sweep("sysfs", sources)], ctx)
            .await
    }

    async fn next_msg(rx: &mut mpsc::Receiver<SourceMsg>) -> SourceMsg {
        timeout(TIMEOUT, rx.recv())
            .await
            .expect("timed out waiting for the task's report")
            .expect("mpsc channel closed")
    }

    /// Gives an `insert_entry` device a live task that never ends.
    fn attach_task(registry: &mut DeviceRegistry, id: &DeviceId) {
        registry.last_generation += 1;
        let task = SourceTask {
            handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            generation: registry.last_generation,
            // Arbitrary: no caller exercises backend-ownership retirement.
            backend: "test",
        };
        registry.tasks.insert(id.clone(), task);
    }

    fn live_generation(registry: &DeviceRegistry, id: &DeviceId) -> u64 {
        registry.tasks.get(id).map_or(0, |task| task.generation)
    }

    fn record_live(registry: &mut DeviceRegistry, id: &DeviceId, reading: Option<BatteryReading>) {
        let generation = live_generation(registry, id);
        registry.record(id, generation, reading);
    }

    #[tokio::test]
    async fn reading_from_a_retired_generation_is_dropped() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        let first = live_generation(&registry, &a.id());
        registry.record(&a.id(), first, Some(reading(80)));

        sweep(&mut registry, &ctx, vec![]).await;
        sweep(&mut registry, &ctx, vec![ok(&a, 55)]).await;
        let second = live_generation(&registry, &a.id());
        assert_ne!(first, second);

        registry.record(&a.id(), first, Some(reading(10)));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Disconnected);
        assert_eq!(snapshot[0].last_reading, Some(reading(80)));

        registry.record(&a.id(), second, Some(reading(55)));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[0].last_reading, Some(reading(55)));
    }

    #[tokio::test]
    async fn reading_from_an_aborted_task_does_not_revive_a_vanished_device() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        let generation = live_generation(&registry, &a.id());
        registry.record(&a.id(), generation, Some(reading(80)));
        sweep(&mut registry, &ctx, vec![]).await;

        registry.record(&a.id(), generation, Some(reading(79)));

        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    #[tokio::test]
    async fn reassigned_node_ends_the_task_and_disconnects_the_device() {
        let (ctx, mut rx, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_online(&mut registry, &a, 80);
        registry.spawn(reassigned(&a), a.id(), "test", &ctx);

        let msg = next_msg(&mut rx).await;
        assert!(matches!(msg.event, SourceEvent::NodeReassigned));
        assert_eq!(msg.generation, live_generation(&registry, &a.id()));
        registry.retire(&msg.id, msg.generation);

        assert!(!registry.tasks.contains_key(&a.id()));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Disconnected);
        assert_eq!(snapshot[0].last_reading, Some(reading(80)));
    }

    /// The task keeps polling: access can be granted without the device leaving.
    #[tokio::test]
    async fn access_denied_is_reported_as_its_own_event_and_the_task_keeps_running() {
        let (ctx, mut rx, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_entry(&mut registry, &a, Presence::Unreachable, None, None, 0);
        registry.spawn(denied(&a), a.id(), "test", &ctx);

        let msg = next_msg(&mut rx).await;
        assert!(matches!(msg.event, SourceEvent::AccessDenied));
        registry.deny(&msg.id, msg.generation);

        assert_eq!(registry.snapshot()[0].presence, Presence::NoAccess);
        tokio::task::yield_now().await;
        assert!(!registry.tasks[&a.id()].handle.is_finished());
    }

    #[tokio::test]
    async fn access_denial_shows_at_once_and_a_reading_clears_it() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_online(&mut registry, &a, 80);
        attach_task(&mut registry, &a.id());

        registry.deny(&a.id(), live_generation(&registry, &a.id()));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::NoAccess);
        assert_eq!(snapshot[0].last_reading, Some(reading(80)));

        record_live(&mut registry, &a.id(), Some(reading(79)));
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    #[tokio::test]
    async fn a_silent_poll_after_access_is_granted_reads_unreachable_not_no_access() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_entry(&mut registry, &a, Presence::Unreachable, None, None, 0);
        attach_task(&mut registry, &a.id());

        registry.deny(&a.id(), live_generation(&registry, &a.id()));
        record_live(&mut registry, &a.id(), None);

        assert_eq!(registry.snapshot()[0].presence, Presence::Unreachable);
    }

    #[tokio::test]
    async fn access_denial_from_a_stale_generation_is_ignored() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_online(&mut registry, &a, 80);
        attach_task(&mut registry, &a.id());
        let stale = live_generation(&registry, &a.id());
        attach_task(&mut registry, &a.id());

        registry.deny(&a.id(), stale);

        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    #[tokio::test]
    async fn retire_from_a_stale_generation_keeps_the_live_task() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        let first = live_generation(&registry, &a.id());
        sweep(&mut registry, &ctx, vec![]).await;
        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        let second = live_generation(&registry, &a.id());
        registry.record(&a.id(), second, Some(reading(80)));

        registry.retire(&a.id(), first);
        assert_eq!(live_generation(&registry, &a.id()), second);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        registry.retire(&a.id(), second);
        assert!(!registry.tasks.contains_key(&a.id()));

        // The next sweep that finds the device spawns afresh, not via the crash path.
        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        assert!(live_generation(&registry, &a.id()) > second);
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    #[tokio::test]
    async fn registry_record_then_snapshot_preserves_discovery_order() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        let b = device("b");
        insert_entry(&mut registry, &a, Presence::Unreachable, None, None, 0);
        insert_entry(&mut registry, &b, Presence::Unreachable, None, None, 0);
        attach_task(&mut registry, &a.id());
        attach_task(&mut registry, &b.id());

        record_live(&mut registry, &a.id(), Some(reading(10)));
        record_live(&mut registry, &b.id(), Some(reading(20)));

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].info.name, "a");
        assert_eq!(snapshot[0].last_reading, Some(reading(10)));
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[1].info.name, "b");
        assert_eq!(snapshot[1].last_reading, Some(reading(20)));
    }

    #[test]
    fn registry_ignores_reading_for_unregistered_id() {
        let mut registry = DeviceRegistry::new(None);
        let ghost = device("ghost");

        record_live(&mut registry, &ghost.id(), Some(reading(99)));

        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn one_failed_poll_keeps_online_and_reading() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_online(&mut registry, &a, 80);

        attach_task(&mut registry, &a.id());
        record_live(&mut registry, &a.id(), None);

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[0].last_reading, Some(reading(80)));
    }

    #[tokio::test]
    async fn offline_after_failures_flips_to_unreachable() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_online(&mut registry, &a, 80);

        attach_task(&mut registry, &a.id());
        for _ in 0..OFFLINE_AFTER_FAILURES {
            record_live(&mut registry, &a.id(), None);
        }

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Unreachable);
        // The reading stays retained even once Unreachable.
        assert_eq!(snapshot[0].last_reading, Some(reading(80)));
    }

    #[tokio::test]
    async fn success_after_failures_resets_counter_and_online() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_entry(
            &mut registry,
            &a,
            Presence::Unreachable,
            Some(reading(80)),
            Some(crate::clock::now()),
            OFFLINE_AFTER_FAILURES - 1,
        );

        attach_task(&mut registry, &a.id());
        record_live(&mut registry, &a.id(), Some(reading(75)));
        assert_eq!(registry.entries[&a.id()].consecutive_failures, 0);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        // Counter was reset — a single further failure must not re-trip
        // Unreachable immediately.
        record_live(&mut registry, &a.id(), None);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    #[tokio::test]
    async fn vanished_device_becomes_disconnected_keeps_reading_and_aborts_task() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        record_live(&mut registry, &a.id(), Some(reading(80)));
        assert!(registry.tasks.contains_key(&a.id()));

        sweep(&mut registry, &ctx, vec![]).await;

        assert!(!registry.tasks.contains_key(&a.id()));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].presence, Presence::Disconnected);
        assert_eq!(snapshot[0].last_reading, Some(reading(80)));
        assert!(snapshot[0].last_seen.is_some());
    }

    /// A panicked task never calls `record`, so its stale `Online` reading
    /// must not survive the sweep that notices `is_finished()`. Prints
    /// tokio's panic message to stderr — expected, not a failure.
    #[tokio::test]
    async fn crashed_task_is_demoted_and_respawned() {
        let (ctx, mut rx, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        sweep(&mut registry, &ctx, vec![panics(&a)]).await;
        settle().await;
        assert!(registry.tasks[&a.id()].handle.is_finished());

        // Simulate the device having been Online with a stale reading right
        // before its task crashed.
        record_live(&mut registry, &a.id(), Some(reading(80)));
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        // Next sweep still sees the device, with a fresh replacement source.
        sweep(&mut registry, &ctx, vec![ok(&a, 55)]).await;

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Unreachable);
        assert_eq!(snapshot[0].last_reading, Some(reading(80)));

        // The replacement task is running — its first poll proves the respawn.
        let msg = next_msg(&mut rx).await;
        assert_eq!(msg.id, a.id());
        let SourceEvent::Polled(polled) = msg.event else {
            unreachable!("replacement task reported a reassigned node");
        };
        registry.record(&msg.id, msg.generation, polled);

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[0].last_reading, Some(reading(55)));
    }

    /// `reconcile` only inspects `is_finished()` for ids still in the fresh
    /// set, so a vanished device is aborted, not mistaken for a crash.
    #[tokio::test]
    async fn vanished_device_is_not_reported_as_crashed() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        record_live(&mut registry, &a.id(), Some(reading(80)));

        sweep(&mut registry, &ctx, vec![]).await;

        assert!(!registry.tasks.contains_key(&a.id()));
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    #[tokio::test]
    async fn disconnected_without_a_reading_is_dropped_immediately() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        sweep(&mut registry, &ctx, vec![err(&a)]).await;
        sweep(&mut registry, &ctx, vec![]).await;

        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn reappeared_device_reuses_entry_and_returns_online() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        record_live(&mut registry, &a.id(), Some(reading(80)));

        sweep(&mut registry, &ctx, vec![]).await;
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;

        // Same entry reused, not duplicated.
        assert_eq!(registry.snapshot().len(), 1);
        // Retained reading still visible until the fresh task proves online.
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

        record_live(&mut registry, &a.id(), Some(reading(80)));
        assert_eq!(registry.snapshot().len(), 1);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    /// A failed sweep (a bus hiccup) is not an empty one: the backend's
    /// devices stay exactly as they were.
    #[tokio::test]
    async fn failed_backend_sweep_does_not_retire_its_devices() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(vec![ok_sweep("bluez", vec![ok(&a, 80)])], &ctx)
            .await;
        record_live(&mut registry, &a.id(), Some(reading(80)));
        assert!(registry.tasks.contains_key(&a.id()));

        registry
            .reconcile(vec![err_sweep("bluez", "GetManagedObjects failed")], &ctx)
            .await;

        assert!(
            registry.tasks.contains_key(&a.id()),
            "task must keep running across a failed sweep"
        );
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_ne!(
            snapshot[0].presence,
            Presence::Disconnected,
            "presence must not change on a failed sweep"
        );
        assert_eq!(snapshot[0].last_reading, Some(reading(80)));
    }

    #[tokio::test]
    async fn succeeded_backend_sweep_still_retires_its_devices() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(vec![ok_sweep("bluez", vec![ok(&a, 80)])], &ctx)
            .await;
        record_live(&mut registry, &a.id(), Some(reading(80)));

        registry
            .reconcile(vec![ok_sweep("bluez", vec![])], &ctx)
            .await;

        assert!(!registry.tasks.contains_key(&a.id()));
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    /// `steelseries` and `eightbitdo` both report `Transport::Hidraw`, so
    /// only `SourceTask::backend` can tell whose sweep failed.
    #[tokio::test]
    async fn backend_ownership_does_not_collapse_across_shared_transport() {
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(None);

        let mut mouse = device("aerox");
        mouse.transport = Transport::Hidraw;
        let mut controller = device("ultimate2");
        controller.transport = Transport::Hidraw;

        registry
            .reconcile(
                vec![
                    ok_sweep("steelseries", vec![ok(&mouse, 80)]),
                    ok_sweep("eightbitdo", vec![ok(&controller, 50)]),
                ],
                &ctx,
            )
            .await;
        record_live(&mut registry, &mouse.id(), Some(reading(80)));
        record_live(&mut registry, &controller.id(), Some(reading(50)));

        // steelseries fails; eightbitdo succeeds and no longer sees its controller.
        registry
            .reconcile(
                vec![
                    err_sweep("steelseries", "hidraw read error"),
                    ok_sweep("eightbitdo", vec![]),
                ],
                &ctx,
            )
            .await;

        assert!(
            registry.tasks.contains_key(&mouse.id()),
            "steelseries device must survive its own backend's failure"
        );
        assert!(
            !registry.tasks.contains_key(&controller.id()),
            "eightbitdo device is correctly retired despite sharing Transport::Hidraw"
        );
    }

    #[test]
    fn disconnected_entry_older_than_cap_is_pruned() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        let last_seen = crate::clock::now();
        insert_entry(
            &mut registry,
            &a,
            Presence::Disconnected,
            Some(reading(80)),
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

    #[test]
    fn an_offline_icon_never_outlives_its_roster_entry() {
        assert!(crate::domain::OFFLINE_SHELF_LIFE_MAX <= DISCONNECTED_RETENTION);
    }

    #[tokio::test]
    async fn spawn_with_migrates_shown_devices_and_publishes_after_first_sweep() {
        let (tx, _config_rx) = watch::channel(Config::default());
        let mut published = tx.subscribe();

        let (saved_tx, saved) = std::sync::mpsc::channel::<Config>();

        let (_rx, _refresh) = Supervisor::spawn_with(
            tx,
            || found(vec![ok(&device("a"), 50), ok(&device("b"), 50)]),
            || Config {
                shown_devices: vec!["a".to_string()],
                ..Config::default()
            },
            move |cfg: &Config| {
                saved_tx.send(cfg.clone()).expect("test receiver alive");
                Ok(())
            },
            None,
            ConfigRole::Owner,
        );

        // The migration's config_tx.send is the channel's only publisher in
        // this test, so its first change is the migrated config.
        timeout(TIMEOUT, published.changed())
            .await
            .expect("timed out waiting for the migration to publish")
            .expect("config channel closed");

        let migrated = published.borrow().clone();
        assert_eq!(migrated.hidden_devices, vec!["b".to_string()]);
        assert!(migrated.shown_devices.is_empty());
        assert_eq!(saved.try_iter().last().as_ref(), Some(&migrated));
    }

    #[tokio::test]
    async fn a_reader_never_converts_shown_devices() {
        let (tx, config_rx) = watch::channel(Config::default());
        let (saved_tx, saved) = std::sync::mpsc::channel::<Config>();

        let (mut rx, _refresh) = Supervisor::spawn_with(
            tx,
            || found(vec![ok(&device("a"), 50), ok(&device("b"), 50)]),
            || Config {
                shown_devices: vec!["a".to_string()],
                ..Config::default()
            },
            move |cfg: &Config| {
                saved_tx.send(cfg.clone()).expect("test receiver alive");
                Ok(())
            },
            None,
            ConfigRole::Reader,
        );
        // A reading is applied only after the first sweep's conversion attempt.
        wait_for_connected(&mut rx).await;

        assert_eq!(saved.try_iter().count(), 0);
        assert!(!config_rx.has_changed().expect("config channel open"));
    }

    // Every test above passes no store; these open one in a scratch
    // directory, never `state::open()`'s real one.

    fn open_scratch_store(test_name: &str) -> (state::Store, ScratchDir) {
        let scratch = ScratchDir::new(test_name);
        let db = state::SqliteStore::open(&scratch.join("state.db"))
            .expect("opening scratch state store");
        let store = state::Store::start(db).expect("starting the store thread");
        (store, scratch)
    }

    #[tokio::test]
    async fn reconcile_upserts_devices_row_when_store_present() {
        let (store, _scratch) = open_scratch_store("reconcile-upsert");
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(Some(store.clone()));
        let a = device("a");

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;

        let devices = store.list_devices().await.expect("listing devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device, a.id());
    }

    /// Same transport and locator under a new display name (a BlueZ alias
    /// edit): the row is renamed, not doubled, and the caller is told.
    #[tokio::test]
    async fn reconcile_reports_a_rename_instead_of_adding_a_row() {
        let (store, _scratch) = open_scratch_store("reconcile-rename");
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(Some(store.clone()));

        let before = DeviceInfo {
            name: "MX Anywhere 3".to_owned(),
            kind: DeviceKind::Mouse,
            transport: Transport::Sysfs,
            locator: Some("00:00:5e:00:53:01".to_owned()),
        };
        let after = DeviceInfo {
            name: "Work mouse".to_owned(),
            ..before.clone()
        };

        let renames = sweep(&mut registry, &ctx, vec![ok(&before, 80)]).await;
        assert!(renames.is_empty());

        let renames = sweep(&mut registry, &ctx, vec![ok(&after, 80)]).await;

        assert_eq!(
            renames,
            vec![Rename {
                from: "MX Anywhere 3".to_string(),
                to: "Work mouse".to_string(),
            }]
        );
        let devices = store.list_devices().await.expect("listing devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device, after.id());

        // The live roster must not keep the old name around as a Disconnected
        // duplicate: the tray would show one device twice for a day.
        let roster = registry.snapshot();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].info.name, "Work mouse");
    }

    /// The config half of the same story: the settings keyed by the old name
    /// move to the new one and the result is both saved and republished.
    #[test]
    fn apply_renames_moves_settings_and_republishes() {
        let (saved_tx, saved) = std::sync::mpsc::channel::<Config>();
        let on_disk = Config {
            hidden_devices: vec!["MX Anywhere 3".to_string()],
            primary_device: Some("MX Anywhere 3".to_string()),
            ..Config::default()
        };
        let (tx, config_rx) = watch::channel(on_disk.clone());
        let config = ConfigIo {
            tx,
            load: move || on_disk.clone(),
            save: move |cfg: &Config| {
                saved_tx.send(cfg.clone()).expect("test receiver alive");
                Ok(())
            },
        };

        config.apply_renames(&[Rename {
            from: "MX Anywhere 3".to_string(),
            to: "Work mouse".to_string(),
        }]);

        let published = config_rx.borrow().clone();
        assert_eq!(published.hidden_devices, vec!["Work mouse".to_string()]);
        assert_eq!(published.primary_device, Some("Work mouse".to_string()));
        assert_eq!(saved.try_iter().last().as_ref(), Some(&published));
    }

    #[tokio::test]
    async fn record_writes_reading_to_store_when_present() {
        let (store, _scratch) = open_scratch_store("record-reading");
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(Some(store.clone()));
        let a = device("a");

        sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
        record_live(&mut registry, &a.id(), Some(reading(80)));

        let history = store
            .recent_readings(&a.id(), 10)
            .await
            .expect("reading history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1, 80);
    }

    /// A device found for the first time in a fresh process still gets the
    /// change-point history a previous run persisted, so the estimate does
    /// not start from zero after every tray restart.
    #[tokio::test]
    async fn newly_discovered_device_seeds_history_from_store() {
        let (store, _scratch) = open_scratch_store("seed-on-discovery");
        let a = device("a");
        let id = a.id();

        store
            .record_seen(vec![(id.clone(), a.kind)], 0)
            .await
            .expect("record_seen");
        for (at, percent) in [(0i64, 80u8), (600, 79), (1200, 78)] {
            store.record_reading(&id, reading(percent), at);
        }

        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(Some(store.clone()));

        sweep(&mut registry, &ctx, vec![ok(&a, 78)]).await;

        let entry = registry.entries.get(&id).expect("device entry present");
        let percents: Vec<u8> = entry.battery_history.iter().map(|&(_, p)| p).collect();
        assert_eq!(percents, vec![80, 79, 78]);
    }

    const HOUR: Duration = Duration::from_secs(60 * 60);

    /// A store holding one device and its reading taken `age` ago, as a
    /// previous run left it.
    async fn store_with_reading(
        test_name: &str,
        info: &DeviceInfo,
        percent: u8,
        age: Duration,
    ) -> (state::Store, ScratchDir) {
        let (store, scratch) = open_scratch_store(test_name);
        let age_secs = i64::try_from(age.as_secs()).expect("a test age fits in i64");
        let at = state::now_unix() - age_secs;
        store
            .record_seen(vec![(info.id(), info.kind)], at)
            .await
            .expect("record_seen");
        store.record_reading(&info.id(), reading(percent), at);
        (store, scratch)
    }

    fn age_of(device: &DeviceState) -> Duration {
        let seen = device.last_seen.expect("a restored reading has a time");
        crate::clock::now().saturating_duration_since(seen)
    }

    /// A device switched off before a restart: no sweep finds it, and it
    /// still shows, dimmed, with the age of its last reading.
    #[tokio::test]
    async fn a_restart_restores_a_recent_reading_of_an_undiscovered_device() {
        let keyboard = device("keyboard");
        let (store, _scratch) = store_with_reading("restore-recent", &keyboard, 64, 2 * HOUR).await;

        let (tx, _config_rx) = watch::channel(Config::default());
        let (mut rx, _refresh) = Supervisor::spawn_with(
            tx,
            || found(vec![]),
            no_migration_load,
            no_migration_save,
            Some(store),
            ConfigRole::Owner,
        );
        let state = wait_for_devices(&mut rx).await;

        assert_eq!(state.devices.len(), 1, "{:?}", state.devices);
        let restored = &state.devices[0];
        assert_eq!(restored.presence, Presence::Disconnected);
        assert_eq!(restored.last_reading, Some(reading(64)));
        let age = age_of(restored);
        assert!(
            age >= 2 * HOUR && age < 2 * HOUR + Duration::from_secs(60),
            "age {age:?}"
        );
        assert!(crate::domain::is_visible(
            restored,
            |_| true,
            crate::clock::now(),
            crate::domain::OFFLINE_SHELF_LIFE_MAX,
        ));
    }

    #[tokio::test]
    async fn a_restart_does_not_restore_a_reading_past_the_retention() {
        let keyboard = device("keyboard");
        let (store, _scratch) = store_with_reading("restore-old", &keyboard, 64, 25 * HOUR).await;
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(Some(store));

        sweep(&mut registry, &ctx, vec![]).await;
        registry.restore_retained().await;

        assert_eq!(registry.snapshot(), []);
    }

    #[tokio::test]
    async fn a_discovered_device_shows_its_stored_reading_until_its_first_poll() {
        let mouse = device("mouse");
        let (store, _scratch) = store_with_reading("restore-discovered", &mouse, 70, HOUR).await;
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(Some(store));

        sweep(&mut registry, &ctx, vec![ok(&mouse, 65)]).await;

        let before = &registry.snapshot()[0];
        assert_eq!(before.presence, Presence::Unreachable);
        assert_eq!(before.last_reading, Some(reading(70)));
        assert!(age_of(before) >= HOUR);

        record_live(&mut registry, &mouse.id(), Some(reading(65)));

        let after = &registry.snapshot()[0];
        assert_eq!(after.presence, Presence::Online);
        assert_eq!(after.last_reading, Some(reading(65)));
        assert!(age_of(after) < Duration::from_secs(60));
    }

    /// A restored entry has no task; the sweep that finds the device gives it
    /// one, and losing it again retires it like any other.
    #[tokio::test]
    async fn a_restored_device_is_polled_once_a_sweep_finds_it() {
        let mouse = device("mouse");
        let (store, _scratch) = store_with_reading("restore-handover", &mouse, 70, HOUR).await;
        let (ctx, _, _config) = source_ctx();
        let mut registry = DeviceRegistry::new(Some(store));
        sweep(&mut registry, &ctx, vec![]).await;
        registry.restore_retained().await;
        assert!(!registry.tasks.contains_key(&mouse.id()));

        registry
            .reconcile(vec![err_sweep("sysfs", "bus hiccup")], &ctx)
            .await;
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

        sweep(&mut registry, &ctx, vec![ok(&mouse, 65)]).await;
        assert!(registry.tasks.contains_key(&mouse.id()));
        assert_eq!(registry.snapshot()[0].last_reading, Some(reading(70)));
        record_live(&mut registry, &mouse.id(), Some(reading(65)));
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        sweep(&mut registry, &ctx, vec![]).await;
        assert!(!registry.tasks.contains_key(&mouse.id()));
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    #[test]
    fn push_history_point_dedups_and_caps_like_live_polling() {
        let base = crate::clock::now();
        let mut history = Vec::new();
        push_history_point(&mut history, base, 80);
        push_history_point(&mut history, base, 80); // duplicate, dropped
        push_history_point(&mut history, base, 79);
        assert_eq!(history, vec![(base, 80), (base, 79)]);

        // An increase clears everything recorded before it.
        push_history_point(&mut history, base, 90);
        assert_eq!(history, vec![(base, 90)]);
    }

    /// Readings every few seconds must not postpone the periodic sweep.
    #[tokio::test(start_paused = true)]
    async fn readings_do_not_postpone_the_discovery_sweep() {
        let cfg = Config {
            poll_interval_secs: 5,
            ..Config::default()
        };
        let (tx, _config_rx) = watch::channel(cfg);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let (_rx, _refresh) = spawn_test(tx, {
            let sweeps = sweeps.clone();
            move || {
                sweeps.fetch_add(1, Ordering::Relaxed);
                found(vec![ok(&device("mouse"), 80)])
            }
        });

        tokio::time::sleep(DISCOVERY_INTERVAL * 3 + Duration::from_secs(1)).await;

        assert_eq!(
            sweeps.load(Ordering::Relaxed),
            4,
            "initial sweep plus one per interval"
        );
    }

    /// A silent device's polls and every later sweep change nothing a surface
    /// shows, so none of them wakes a receiver.
    #[tokio::test(start_paused = true)]
    async fn unchanged_state_is_not_republished() {
        let cfg = Config {
            poll_interval_secs: 5,
            ..Config::default()
        };
        let (tx, _config_rx) = watch::channel(cfg);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let (mut rx, _refresh) = spawn_test(tx, {
            let sweeps = sweeps.clone();
            move || {
                sweeps.fetch_add(1, Ordering::Relaxed);
                found(vec![err(&device("mouse"))])
            }
        });
        rx.changed().await.expect("the first sweep is published");
        rx.borrow_and_update();

        let mut notifications = 0;
        let quiet = tokio::time::sleep(DISCOVERY_INTERVAL * 3 + Duration::from_secs(1));
        tokio::pin!(quiet);
        loop {
            tokio::select! {
                r = rx.changed() => {
                    r.expect("supervisor alive");
                    notifications += 1;
                }
                () = &mut quiet => break,
            }
        }

        assert_eq!(sweeps.load(Ordering::Relaxed), 4);
        assert_eq!(notifications, 0);
    }

    struct PushingSource {
        info: DeviceInfo,
        polls: Arc<AtomicUsize>,
        pushes: watch::Receiver<Option<BatteryReading>>,
    }

    #[async_trait::async_trait]
    impl BatterySource for PushingSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            Ok(reading(80))
        }

        async fn pushed(&mut self) -> BatteryReading {
            loop {
                if self.pushes.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
                if let Some(r) = *self.pushes.borrow_and_update() {
                    return r;
                }
            }
        }
    }

    /// A pushed reading lands without a poll; `rediscover` sweeps without
    /// re-polling the devices already running.
    #[tokio::test(start_paused = true)]
    async fn a_pushed_reading_and_a_rediscover_poll_nothing() {
        let (tx, _config_rx) = watch::channel(Config::default());
        let polls = Arc::new(AtomicUsize::new(0));
        let sweeps = Arc::new(AtomicUsize::new(0));
        let (push_tx, push_rx) = watch::channel(None);
        let (polls_seen, sweeps_seen) = (polls.clone(), sweeps.clone());
        let (mut rx, refresh) = spawn_test(tx, move || {
            sweeps_seen.fetch_add(1, Ordering::Relaxed);
            let source: Box<dyn BatterySource> = Box::new(PushingSource {
                info: device("mouse"),
                polls: polls_seen.clone(),
                pushes: push_rx.clone(),
            });
            found(vec![source])
        });
        let state = wait_for_connected(&mut rx).await;
        assert_eq!(state.devices[0].last_reading.map(|r| r.percent), Some(80));
        assert_eq!(polls.load(Ordering::Relaxed), 1);

        push_tx.send_replace(Some(reading(42)));
        timeout(Duration::from_secs(1), async {
            loop {
                rx.changed().await.expect("supervisor alive");
                let percent = rx.borrow_and_update().devices[0]
                    .last_reading
                    .map(|r| r.percent);
                if percent == Some(42) {
                    return;
                }
            }
        })
        .await
        .expect("the pushed reading was published");
        assert_eq!(polls.load(Ordering::Relaxed), 1, "a push is not a poll");

        let before = sweeps.load(Ordering::Relaxed);
        refresh.rediscover();
        settle().await;
        assert_eq!(sweeps.load(Ordering::Relaxed), before + 1);
        assert_eq!(polls.load(Ordering::Relaxed), 1, "rediscover re-polled");
    }
}
