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
mod tests;
