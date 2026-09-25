use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::{MissedTickBehavior, interval_at, sleep};

use super::migration::migrate_shown_devices_once;
use crate::config::Config;
use crate::discovery::BackendSweep;
use crate::domain::estimate::estimate as estimate_remaining;
use crate::domain::{
    BatteryReading, BootTime, DeviceId, DeviceInfo, DeviceKind, DeviceState, Estimate, Presence,
    TrayState,
};
use crate::refresh::RefreshSignal;
use crate::sources::hidraw::NodeReassigned;
use crate::sources::{AccessDenied, BatterySource, Context};
use crate::state;

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
/// few bytes per device. A new source is seeded from the state store.
const HISTORY_CAP: usize = 20;

pub struct Supervisor;

/// Whether this process may write `config.json`. Exactly one process owns
/// the file: the tray, which holds the single-instance name. Every other
/// mode reads it and leaves it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigRole {
    Owner,
    Reader,
}

impl Supervisor {
    /// Spawns the manager with the real `discovery::discover_all` backend,
    /// threading the shared `Context` (in particular its system-bus
    /// connection) into every discovery pass, and the real
    /// `config::load`/`config::save` backing the one-time `shown_devices`
    /// conversion (see `migrate_shown_devices_once`).
    ///
    /// `store` is the optional device inventory / reading history (T34) —
    /// `None` runs exactly as before it existed. See `src/state/mod.rs` for
    /// why it stays optional and which caller is expected to pass `Some`.
    pub fn spawn(
        config_tx: watch::Sender<Config>,
        ctx: Arc<Context>,
        store: Option<state::Store>,
        role: ConfigRole,
    ) -> (watch::Receiver<TrayState>, RefreshSignal) {
        // Only the owner may write the config. `--waybar` does not take the
        // session-bus single-instance name the tray takes, so it can run
        // alongside a tray — and two processes independently converting the
        // same legacy whitelist, against two different rosters, is a race with
        // a destructive outcome. A reader loads normally and saves nothing.
        let save: fn(&Config) -> anyhow::Result<()> = match role {
            ConfigRole::Owner => crate::config::save,
            ConfigRole::Reader => |_cfg| Ok(()),
        };
        let load: fn() -> Config = match role {
            ConfigRole::Owner => crate::config::load,
            // A reader must not convert at all, and `migrate_shown_devices`
            // returns `NotNeeded` for an empty whitelist, so hand it one.
            ConfigRole::Reader => || Config {
                shown_devices: Vec::new(),
                ..crate::config::load()
            },
        };
        Self::spawn_with(
            config_tx,
            move || {
                let ctx = ctx.clone();
                Box::pin(async move { crate::discovery::discover_all(&ctx).await })
            },
            load,
            save,
            store,
        )
    }

    /// Injectable discovery, config persistence, and state store for tests.
    /// `discover` is called once at start, then on every discovery tick and
    /// every `trigger`/`rediscover`. `load_config`/`save_config` back the
    /// one-time `shown_devices` → `hidden_devices` conversion
    /// (`migrate_shown_devices_once`) — tests must inject fakes here rather
    /// than let it fall through to the real `config::load`/`config::save`,
    /// which would read and rewrite the caller's actual config file. `store`
    /// is likewise never the real `state::open()` here — tests that
    /// exercise the wiring pass a `SqliteStore` opened against a temporary
    /// file, and every other test passes `None`, so no test can ever touch
    /// the real state directory.
    ///
    /// Takes the config `Sender`, not just a `Receiver`: the conversion
    /// needs to publish its result back onto the channel so the tray and
    /// notifier pick it up without a restart. `manager_task` derives its own
    /// `Receiver` from it.
    fn spawn_with<F, Fut, L, S>(
        config_tx: watch::Sender<Config>,
        discover: F,
        load_config: L,
        save_config: S,
        store: Option<state::Store>,
    ) -> (watch::Receiver<TrayState>, RefreshSignal)
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Vec<BackendSweep>> + Send + 'static,
        L: Fn() -> Config + Send + 'static,
        S: Fn(&Config) -> anyhow::Result<()> + Send + 'static,
    {
        let initial = TrayState { devices: vec![] };
        let (watch_tx, watch_rx) = watch::channel(initial);
        let refresh = RefreshSignal::new();

        let refresh_inner = refresh.clone();
        tokio::spawn(manager_task(
            config_tx,
            discover,
            watch_tx,
            refresh_inner,
            load_config,
            save_config,
            store,
        ));

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
    last_seen: Option<BootTime>,
    presence: Presence,
    consecutive_failures: u32,
    /// Percent-change points feeding the remaining-time estimate. Only a
    /// point where the percent differs from the previous one is kept — see
    /// `push_reading`.
    battery_history: Vec<(BootTime, u8)>,
    /// Name of the `BatteryBackend` that last (re)discovered this device —
    /// how `reconcile` decides whether a vanished entry is safe to retire.
    /// Not derived from `DeviceId`/`transport`: `Transport::Hidraw` is
    /// shared by both the `steelseries` and `eightbitdo` backends, so
    /// transport alone cannot answer "which backend owns this device" (see
    /// C2 investigation notes on `reconcile`).
    backend: &'static str,
}

impl DeviceEntry {
    /// Records a new percent reading into `battery_history`. See
    /// `push_history_point` for the reduction rule.
    fn push_reading(&mut self, now: BootTime, percent: u8) {
        push_history_point(&mut self.battery_history, now, percent);
    }

    fn estimate(&self) -> Estimate {
        match self.last_reading {
            Some(r) => estimate_remaining(&self.battery_history, r),
            None => Estimate::Unknown,
        }
    }
}

/// Applies one percent observation to a change-point history in place. A
/// percent equal to the last recorded one is not a transition and is
/// dropped — most polls see no change at the default interval, and only the
/// change points carry a discharge-rate signal. An increase (charging, or a
/// device re-reporting after a battery swap) invalidates any discharge trend
/// observed so far, so it clears the history before recording the new
/// baseline. The result is capped at `HISTORY_CAP`, oldest dropped first.
///
/// Shared between live polling (`DeviceEntry::push_reading`) and seeding a
/// freshly (re)discovered device's history from the state store
/// (`seed_history`), so a reading replayed from disk is reduced exactly the
/// way a live one would have been.
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

/// Rebuilds a freshly (re)discovered device's in-memory change-point history
/// from the state store, so `domain::estimate` has data to work with without
/// waiting out a fresh `MIN_WINDOW` after every tray restart. `store.
/// recent_readings` already returns change points, most-recent-first,
/// capped at `HISTORY_CAP`; this replays them in chronological order through
/// `push_history_point` — the same reduction a live poll would apply — so an
/// increase partway through the persisted history still clears what came
/// before it, exactly as it would live.
///
/// Each stored wall-clock timestamp is converted once, by its age, to a
/// `BootTime` — negative for a reading from before this boot.
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
        let age_secs = now_unix.saturating_sub(at).max(0) as u64;
        let Some(at) = now.checked_sub(Duration::from_secs(age_secs)) else {
            continue;
        };
        push_history_point(&mut history, at, percent);
    }
    history
}

/// Live set of discovered devices and their polling tasks, keyed by identity.
/// `order` preserves discovery order so the tray roster is stable across polls.
/// A device that leaves discovery keeps its entry (see `reconcile`) — only
/// `tasks` loses the id, so `entries` can outlive `tasks` for a given id.
struct DeviceRegistry {
    order: Vec<DeviceId>,
    entries: HashMap<DeviceId, DeviceEntry>,
    tasks: HashMap<DeviceId, SourceTask>,
    /// Last generation handed to a spawned task; see `SourceMsg`.
    last_generation: u64,
    /// Optional device inventory / reading history (T34). `reconcile`
    /// upserts a `devices` row and seeds a new entry's history from it;
    /// `record` writes each successful poll. `None` runs exactly as before
    /// the store existed — see `src/state/mod.rs` for why it stays optional.
    store: Option<state::Store>,
}

struct SourceTask {
    handle: AbortHandle,
    generation: u64,
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

/// What spawning a source task needs, bundled so `reconcile` stays a
/// small-signature method rather than an argument list.
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

    fn spawn(&mut self, src: Box<dyn BatterySource>, id: DeviceId, ctx: &SourceCtx) {
        self.last_generation = self.last_generation.wrapping_add(1);
        let generation = self.last_generation;
        let handle = spawn_source_task(src, id.clone(), generation, ctx);
        self.tasks.insert(id, SourceTask { handle, generation });
    }

    /// Applies a poll outcome to a device that is still registered. A result
    /// from any generation but the live one is ignored — a poll can win a race
    /// against the abort of its own task.
    ///
    /// Success sets `Online` and resets the failure count immediately. A
    /// failure only flips presence to `Unreachable` once it reaches
    /// `OFFLINE_AFTER_FAILURES` — a single dropped poll leaves presence (and
    /// any retained reading) untouched.
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
                entry.push_reading(now, r.percent);
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

    /// Spawns a task for each genuinely new or reappeared id, drops the
    /// transient handle for ids that already run, and aborts (without
    /// forgetting) ids that vanished from discovery. Finally prunes entries
    /// that have been `Disconnected` for too long.
    ///
    /// `sweeps` carries one outcome per backend rather than a flat source
    /// list (see `discovery::BackendSweep`). A backend whose sweep errored
    /// is skipped for both the upsert pass and the vanished-device pass
    /// below: polling only demotes a device after `OFFLINE_AFTER_FAILURES`
    /// consecutive misses, and a single failed discovery sweep must not be
    /// more trigger-happy than that. `entries[id].backend` — not
    /// `DeviceId`/`transport` — is what answers "which backend owns this
    /// id": `Transport::Hidraw` is shared by both the `steelseries` and
    /// `eightbitdo` backends, so transport cannot make that call.
    async fn reconcile(
        &mut self,
        sweeps: Vec<BackendSweep>,
        ctx: &SourceCtx,
    ) -> Vec<(String, String)> {
        let mut renames: Vec<(String, String)> = Vec::new();
        let now_boot = crate::clock::now();
        let now_unix = state::now_unix();

        let mut found: Vec<(&'static str, Box<dyn BatterySource>)> = Vec::new();
        let mut succeeded_backends: Vec<&'static str> = Vec::new();

        for sweep in sweeps {
            let BackendSweep { name, result } = sweep;
            match result {
                Ok(sources) => {
                    succeeded_backends.push(name);
                    found.extend(sources.into_iter().map(|src| (name, src)));
                }
                // Already logged once, by `discovery::discover_all` — not
                // logged again here per device or per sweep.
                Err(_) => continue,
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

    /// One device a successful sweep found: follows the rename the store
    /// detected, then respawns a crashed task, keeps a healthy one, or adds
    /// the entry and spawns its task. Returns the rename, if any.
    async fn admit(
        &mut self,
        src: Box<dyn BatterySource>,
        name: &'static str,
        seen: Option<state::Seen>,
        now_boot: BootTime,
        now_unix: i64,
        ctx: &SourceCtx,
    ) -> Option<(String, String)> {
        let id = src.device().id();
        let mut rename = None;
        if let Some(state::Seen::Renamed { from }) = seen {
            tracing::info!(
                device = %id.name,
                previous = %from,
                "device renamed; inventory row and settings follow it"
            );
            // The live roster is keyed by the same identity, so
            // the entry under the old name has to go now. Left
            // to `retire_vanished` it would linger as a
            // Disconnected duplicate for DISCONNECTED_RETENTION
            // — one device showing twice in the tray for a day,
            // while the Devices tab already shows it once.
            self.forget(&DeviceId {
                name: from.clone(),
                ..id.clone()
            });
            rename = Some((from, id.name.clone()));
        }

        if let Some(task) = self.tasks.get(&id) {
            if task.handle.is_finished() {
                // A handle only stays in `tasks` for an id that is also
                // still in `fresh_ids` if it was never aborted:
                // `retire_vanished` removes the handle from
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
                    entry.backend = name;
                }
                self.spawn(src, id, ctx);
            } else {
                // Still healthy — drop the transient handle, task keeps running.
                drop(src);
            }
            return rename;
        }

        match self.entries.entry(id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                // Reappeared after being Disconnected: reuse the entry,
                // keep the retained reading and presence until the fresh
                // task proves it Online again.
                tracing::info!(device = %src.device().name, "device reappeared");
                slot.get_mut().info = src.device().clone();
                slot.get_mut().backend = name;
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                tracing::info!(device = %src.device().name, "device appeared");
                // First time this run: rebuild its change-point history
                // from the store, if there is one, so an estimate can
                // fire without waiting out a fresh window — see
                // `seed_history`. A device reappearing after
                // `Disconnected` reuses the Occupied arm above and keeps
                // whatever history it already has in memory instead.
                let battery_history = match &self.store {
                    Some(store) => seed_history(store, &id, now_boot, now_unix).await,
                    None => Vec::new(),
                };
                slot.insert(DeviceEntry {
                    info: src.device().clone(),
                    last_reading: None,
                    last_seen: None,
                    presence: Presence::Unreachable,
                    consecutive_failures: 0,
                    battery_history,
                    backend: name,
                });
                self.order.push(id.clone());
            }
        }

        self.spawn(src, id, ctx);
        rename
    }

    fn demote_crashed(&mut self, fresh_ids: &[DeviceId], succeeded_backends: &[&'static str]) {
        // A task whose backend's sweep failed is not in `fresh_ids`, so the
        // crash check in `admit` never looked at it. Its device is not retired
        // either (an error is not an empty result), which left a panicked task
        // frozen — last reading intact, presence Online — for as long as the
        // backend kept failing. Respawning needs a source object only a
        // successful sweep can hand over, so the honest move is to demote the
        // entry and drop the dead handle; the next sweep that succeeds
        // respawns it through `admit`.
        let crashed: Vec<DeviceId> = self
            .tasks
            .iter()
            .filter(|(id, task)| !fresh_ids.contains(id) && task.handle.is_finished())
            .filter(|(id, _)| {
                self.entries
                    .get(*id)
                    .is_some_and(|e| !succeeded_backends.contains(&e.backend))
            })
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
                entry.presence = Presence::Unreachable;
                entry.consecutive_failures = OFFLINE_AFTER_FAILURES;
            }
        }
    }

    fn retire_vanished(&mut self, fresh_ids: &[DeviceId], succeeded_backends: &[&'static str]) {
        let vanished: Vec<DeviceId> = self
            .tasks
            .keys()
            .filter(|id| !fresh_ids.contains(id))
            .filter(|id| {
                // No entry for a tracked task should not happen (entries and
                // tasks are always inserted together in `admit`), but retiring is
                // the pre-fix behaviour and the safer default if it ever
                // does.
                self.entries
                    .get(id)
                    .is_none_or(|e| succeeded_backends.contains(&e.backend))
            })
            .cloned()
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
            entry.presence = Presence::NoAccess;
            // Once access is granted, a silent device reads Unreachable at its first miss.
            entry.consecutive_failures = OFFLINE_AFTER_FAILURES;
        }
    }

    /// Treated as the device leaving, so the next sweep respawns it wherever it is now.
    fn retire(&mut self, id: &DeviceId, generation: u64) {
        if self.is_live(id, generation) {
            self.disconnect(id);
        }
    }

    fn disconnect(&mut self, id: &DeviceId) {
        if let Some(task) = self.tasks.remove(id) {
            task.handle.abort();
        }
        if let Some(entry) = self.entries.get_mut(id) {
            entry.presence = Presence::Disconnected;
            // Reset the debounce counter: the failures that preceded the
            // disconnect belong to the connection that ended. Left as they
            // were, one failed poll after it reconnects re-trips
            // `Unreachable` instead of getting the usual grace.
            entry.consecutive_failures = 0;
        }
    }

    /// Drops `Disconnected` entries that have not been seen for longer than
    /// `DISCONNECTED_RETENTION`. An entry that never produced a reading has
    /// nothing to retain, so it is dropped on disconnect rather than waiting
    /// out the cap. `now` is a parameter so tests can age an entry without a
    /// wall-clock sleep.
    fn prune_stale(&mut self, now: BootTime) {
        let stale: Vec<DeviceId> = self
            .entries
            .iter()
            .filter(|(_, e)| e.presence == Presence::Disconnected)
            .filter(|(_, e)| match e.last_seen {
                None => true,
                Some(t) => now.saturating_duration_since(t) > DISCONNECTED_RETENTION,
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.forget(&id);
        }
    }

    /// Drops every trace of one identity: its polling task (aborted), its
    /// entry, and its place in the display order.
    fn forget(&mut self, id: &DeviceId) {
        if let Some(task) = self.tasks.remove(id) {
            task.handle.abort();
        }
        self.entries.remove(id);
        self.order.retain(|oid| oid != id);
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

async fn manager_task<F, Fut, L, S>(
    config_tx: watch::Sender<Config>,
    discover: F,
    watch_tx: watch::Sender<TrayState>,
    refresh: RefreshSignal,
    load_config: L,
    save_config: S,
    store: Option<state::Store>,
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
        config_rx: config_tx.subscribe(),
        refresh,
    };

    let mut registry = DeviceRegistry::new(store);

    let renames = registry.reconcile(discover().await, &ctx).await;
    // Sent even when unchanged: consumers wait on it as "the first sweep is in".
    watch_tx.send_replace(TrayState {
        devices: registry.snapshot(),
    });
    apply_renames(&renames, &config_tx, &load_config, &save_config);

    // The conversion needs a roster it can trust, and the first sweep after
    // login rarely has one — Bluetooth peripherals enumerate late. So it is
    // attempted here and retried on each sweep until it either converts or
    // gives up at MIGRATION_MAX_SWEEPS; `migration_sweeps` counts the
    // attempts and `migration_done` stops it for the life of the process,
    // so a device connecting later can never retrigger it.
    let mut migration_sweeps: u32 = 0;
    let mut migration_done = migrate_shown_devices_once(
        &registry.names(),
        &config_tx,
        &load_config,
        &save_config,
        migration_sweeps,
    );

    // Outlives iterations: a per-iteration sleep restarts on every reading and starves.
    let mut sweep_timer = interval_at(
        tokio::time::Instant::now() + DISCOVERY_INTERVAL,
        DISCOVERY_INTERVAL,
    );
    sweep_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = mpsc_rx.recv() => {
                match msg {
                    Some(SourceMsg { id, generation, event }) => {
                        match event {
                            SourceEvent::Polled(reading) => registry.record(&id, generation, reading),
                            SourceEvent::AccessDenied => registry.deny(&id, generation),
                            SourceEvent::NodeReassigned => registry.retire(&id, generation),
                        }
                        publish(&registry, &watch_tx);
                        continue;
                    }
                    // All source tasks dropped their senders — nothing left to do.
                    None => return,
                }
            }
            _ = sweep_timer.tick() => {}
            _ = waiter.wait() => sweep_timer.reset(),
        }
        let renames = rediscover(&mut registry, discover(), &ctx, &watch_tx).await;
        apply_renames(&renames, &config_tx, &load_config, &save_config);
        // Retried per sweep, not per loop iteration: readings arrive
        // far more often than sweeps, and counting those would burn
        // the deadline in seconds instead of minutes.
        if !migration_done {
            migration_sweeps = migration_sweeps.saturating_add(1);
            migration_done = migrate_shown_devices_once(
                &registry.names(),
                &config_tx,
                &load_config,
                &save_config,
                migration_sweeps,
            );
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
) -> Vec<(String, String)>
where
    Fut: Future<Output = Vec<BackendSweep>>,
{
    let sweeps = discover.await;
    let renames = registry.reconcile(sweeps, ctx).await;
    publish(registry, watch_tx);
    renames
}

/// Moves per-device settings onto each device's new name, then republishes the
/// config so the tray applies them without waiting for the file watch.
///
/// Reads the config fresh from disk rather than from the watch channel, for
/// the reason `settings::save_edit` documents: the in-memory copy may predate
/// an edit made in the settings window, and writing it back would undo that
/// edit.
///
/// Takes the pieces by reference and is deliberately **not** async: the
/// borrows must not cross an `.await`, or `manager_task`'s `L`/`S` would have
/// to be `Sync` — the same constraint the comment above `rediscover`
/// describes for the discovery closure.
fn apply_renames<L, S>(
    renames: &[(String, String)],
    config_tx: &watch::Sender<Config>,
    load_config: &L,
    save_config: &S,
) where
    L: Fn() -> Config,
    S: Fn(&Config) -> anyhow::Result<()>,
{
    if renames.is_empty() {
        return;
    }
    let mut cfg = load_config();
    let mut changed = false;
    for (from, to) in renames {
        changed |= crate::config::rename_device(&mut cfg, from, to);
    }
    if !changed {
        return;
    }
    match save_config(&cfg) {
        Ok(()) => {
            let _ = config_tx.send(cfg);
        }
        Err(e) => tracing::warn!("failed to save config after a device rename: {e:#}"),
    }
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
        // Counts failures in a row so a permanently absent device (a mouse left
        // switched off) reports its reason once at `warn` instead of every interval
        // forever. Local to the task: the registry's own failure count drives
        // presence, not diagnostics, and is not visible from here.
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
                r = src.pushed() => pushed = Some(r),
            }
        }
    });
    handle.abort_handle()
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

    /// `load_config`/`save_config` stand-ins for tests that do not exercise
    /// `migrate_shown_devices_once`. Using these instead of the real
    /// `crate::config::load`/`crate::config::save` is what keeps every test
    /// in this module from reading or overwriting the developer's actual
    /// `~/.config/rigbat/config.json`.
    fn no_migration_load() -> Config {
        Config::default()
    }

    fn no_migration_save(_cfg: &Config) -> anyhow::Result<()> {
        Ok(())
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

    /// A successful `BackendSweep` for `name`, carrying `sources`.
    fn ok_sweep(name: &'static str, sources: Vec<Box<dyn BatterySource>>) -> BackendSweep {
        BackendSweep {
            name,
            result: Ok(sources),
        }
    }

    /// A failed `BackendSweep` for `name` — a discovery error, distinct from
    /// an honest `ok_sweep(name, vec![])`.
    fn err_sweep(name: &'static str, msg: &str) -> BackendSweep {
        BackendSweep {
            name,
            result: Err(anyhow::anyhow!(msg.to_owned())),
        }
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
        let (tx, _config_rx) = config_channel(Config::default());
        let (mut rx, _refresh) = Supervisor::spawn_with(
            tx,
            || {
                let sources: Vec<Box<dyn BatterySource>> = vec![
                    Box::new(ErrSource {
                        info: device("mouse"),
                    }),
                    Box::new(OkSource {
                        info: device("keyboard"),
                        reading: reading_discharging(80),
                    }),
                ];
                async move { vec![ok_sweep("sysfs", sources)] }
            },
            no_migration_load,
            no_migration_save,
            None,
        );

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
        let (tx, _config_rx) = config_channel(Config::default());
        let (rx, _refresh) = Supervisor::spawn_with(
            tx,
            || {
                let sources: Vec<Box<dyn BatterySource>> = vec![
                    Box::new(ErrSource {
                        info: device("mouse"),
                    }),
                    Box::new(ErrSource {
                        info: device("keyboard"),
                    }),
                ];
                async move { vec![ok_sweep("sysfs", sources)] }
            },
            no_migration_load,
            no_migration_save,
            None,
        );

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
        let (tx, _config_rx) = config_channel(Config::default());
        let (rx, _refresh) = Supervisor::spawn_with(
            tx,
            || async { Vec::<BackendSweep>::new() },
            no_migration_load,
            no_migration_save,
            None,
        );

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

        let (tx, _config_rx) = config_channel(Config::default());
        let (mut rx, refresh) = Supervisor::spawn_with(
            tx,
            move || {
                let count = call_count_clone.fetch_add(1, Ordering::Relaxed);
                let sweeps = if count == 0 {
                    vec![]
                } else {
                    vec![ok_sweep(
                        "sysfs",
                        vec![Box::new(OkSource {
                            info: device("kbd"),
                            reading: reading_discharging(70),
                        })],
                    )]
                };
                async move { sweeps }
            },
            no_migration_load,
            no_migration_save,
            None,
        );

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
        let (tx, _config_rx) = config_channel(initial);

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

        let (mut rx, _refresh) = Supervisor::spawn_with(
            tx.clone(),
            move || {
                let sources: Vec<Box<dyn BatterySource>> = vec![Box::new(CountingSource {
                    info: device("mouse"),
                    count: poll_count_clone.clone(),
                })];
                async move { vec![ok_sweep("sysfs", sources)] }
            },
            no_migration_load,
            no_migration_save,
            None,
        );

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
                // Arbitrary: none of this helper's callers exercise
                // `reconcile`'s backend-ownership retire logic.
                backend: "test",
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

    /// Gives an `insert_entry` device a live task that never ends.
    fn attach_task(registry: &mut DeviceRegistry, id: &DeviceId) {
        registry.last_generation += 1;
        let task = SourceTask {
            handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            generation: registry.last_generation,
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

    fn ok_source(info: &DeviceInfo, percent: u8) -> Box<dyn BatterySource> {
        Box::new(OkSource {
            info: info.clone(),
            reading: reading_discharging(percent),
        })
    }

    #[tokio::test]
    async fn reading_from_a_retired_generation_is_dropped() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(vec![ok_sweep("sysfs", vec![ok_source(&a, 80)])], &ctx)
            .await;
        let first = live_generation(&registry, &a.id());
        registry.record(&a.id(), first, Some(reading_discharging(80)));

        registry
            .reconcile(vec![ok_sweep("sysfs", vec![])], &ctx)
            .await;
        registry
            .reconcile(vec![ok_sweep("sysfs", vec![ok_source(&a, 55)])], &ctx)
            .await;
        let second = live_generation(&registry, &a.id());
        assert_ne!(first, second);

        registry.record(&a.id(), first, Some(reading_discharging(10)));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Disconnected);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));

        registry.record(&a.id(), second, Some(reading_discharging(55)));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(55)));
    }

    #[tokio::test]
    async fn reading_from_an_aborted_task_does_not_revive_a_vanished_device() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(vec![ok_sweep("sysfs", vec![ok_source(&a, 80)])], &ctx)
            .await;
        let generation = live_generation(&registry, &a.id());
        registry.record(&a.id(), generation, Some(reading_discharging(80)));
        registry
            .reconcile(vec![ok_sweep("sysfs", vec![])], &ctx)
            .await;

        registry.record(&a.id(), generation, Some(reading_discharging(79)));

        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    struct ReassignedSource {
        info: DeviceInfo,
    }

    #[async_trait::async_trait]
    impl BatterySource for ReassignedSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            Err(anyhow::Error::new(NodeReassigned {
                node: "hidraw7".to_owned(),
            }))
        }
    }

    #[tokio::test]
    async fn reassigned_node_ends_the_task_and_disconnects_the_device() {
        let (_tx, config_rx) = config_channel(Config::default());
        let (mpsc_tx, mut mpsc_rx) = mpsc::channel(64);
        let ctx = SourceCtx {
            tx: mpsc_tx,
            config_rx,
            refresh: RefreshSignal::new(),
        };
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_entry(
            &mut registry,
            &a,
            Presence::Online,
            Some(reading_discharging(80)),
            Some(crate::clock::now()),
            0,
        );
        registry.spawn(Box::new(ReassignedSource { info: a.clone() }), a.id(), &ctx);

        let msg = timeout(std::time::Duration::from_secs(5), mpsc_rx.recv())
            .await
            .expect("timed out waiting for the task's report")
            .expect("mpsc channel closed");
        assert!(matches!(msg.event, SourceEvent::NodeReassigned));
        assert_eq!(msg.generation, live_generation(&registry, &a.id()));
        registry.retire(&msg.id, msg.generation);

        assert!(!registry.tasks.contains_key(&a.id()));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Disconnected);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));
    }

    struct DeniedSource {
        info: DeviceInfo,
    }

    #[async_trait::async_trait]
    impl BatterySource for DeniedSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            Err(anyhow::Error::new(AccessDenied {
                path: "/dev/hidraw7".into(),
            }))
        }
    }

    /// The task keeps polling: access can be granted without the device leaving.
    #[tokio::test]
    async fn access_denied_is_reported_as_its_own_event_and_the_task_keeps_running() {
        let (_tx, config_rx) = config_channel(Config::default());
        let (mpsc_tx, mut mpsc_rx) = mpsc::channel(64);
        let ctx = SourceCtx {
            tx: mpsc_tx,
            config_rx,
            refresh: RefreshSignal::new(),
        };
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_entry(&mut registry, &a, Presence::Unreachable, None, None, 0);
        registry.spawn(Box::new(DeniedSource { info: a.clone() }), a.id(), &ctx);

        let msg = timeout(std::time::Duration::from_secs(5), mpsc_rx.recv())
            .await
            .expect("timed out waiting for the task's report")
            .expect("mpsc channel closed");
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
        insert_entry(
            &mut registry,
            &a,
            Presence::Online,
            Some(reading_discharging(80)),
            Some(crate::clock::now()),
            0,
        );
        attach_task(&mut registry, &a.id());

        registry.deny(&a.id(), live_generation(&registry, &a.id()));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::NoAccess);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));

        record_live(&mut registry, &a.id(), Some(reading_discharging(79)));
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
        insert_entry(
            &mut registry,
            &a,
            Presence::Online,
            Some(reading_discharging(80)),
            Some(crate::clock::now()),
            0,
        );
        attach_task(&mut registry, &a.id());
        let stale = live_generation(&registry, &a.id());
        attach_task(&mut registry, &a.id());

        registry.deny(&a.id(), stale);

        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    #[tokio::test]
    async fn retire_from_a_stale_generation_keeps_the_live_task() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(vec![ok_sweep("sysfs", vec![ok_source(&a, 80)])], &ctx)
            .await;
        let first = live_generation(&registry, &a.id());
        registry
            .reconcile(vec![ok_sweep("sysfs", vec![])], &ctx)
            .await;
        registry
            .reconcile(vec![ok_sweep("sysfs", vec![ok_source(&a, 80)])], &ctx)
            .await;
        let second = live_generation(&registry, &a.id());
        registry.record(&a.id(), second, Some(reading_discharging(80)));

        registry.retire(&a.id(), first);
        assert_eq!(live_generation(&registry, &a.id()), second);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        registry.retire(&a.id(), second);
        assert!(!registry.tasks.contains_key(&a.id()));

        // The next sweep that finds the device spawns afresh, not via the crash path.
        registry
            .reconcile(vec![ok_sweep("sysfs", vec![ok_source(&a, 80)])], &ctx)
            .await;
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

        record_live(&mut registry, &a.id(), Some(reading_discharging(10)));
        record_live(&mut registry, &b.id(), Some(reading_discharging(20)));

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
        let mut registry = DeviceRegistry::new(None);
        let ghost = device("ghost");

        record_live(&mut registry, &ghost.id(), Some(reading_discharging(99)));

        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn one_failed_poll_keeps_online_and_reading() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_entry(
            &mut registry,
            &a,
            Presence::Online,
            Some(reading_discharging(80)),
            Some(crate::clock::now()),
            0,
        );

        attach_task(&mut registry, &a.id());
        record_live(&mut registry, &a.id(), None);

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Online);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));
    }

    #[tokio::test]
    async fn offline_after_failures_flips_to_unreachable() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_entry(
            &mut registry,
            &a,
            Presence::Online,
            Some(reading_discharging(80)),
            Some(crate::clock::now()),
            0,
        );

        attach_task(&mut registry, &a.id());
        for _ in 0..OFFLINE_AFTER_FAILURES {
            record_live(&mut registry, &a.id(), None);
        }

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Unreachable);
        // The reading stays retained even once Unreachable.
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));
    }

    #[tokio::test]
    async fn success_after_failures_resets_counter_and_online() {
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");
        insert_entry(
            &mut registry,
            &a,
            Presence::Unreachable,
            Some(reading_discharging(80)),
            Some(crate::clock::now()),
            OFFLINE_AFTER_FAILURES - 1,
        );

        attach_task(&mut registry, &a.id());
        record_live(&mut registry, &a.id(), Some(reading_discharging(75)));
        assert_eq!(registry.entries[&a.id()].consecutive_failures, 0);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        // Counter was reset — a single further failure must not re-trip
        // Unreachable immediately.
        record_live(&mut registry, &a.id(), None);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    #[tokio::test]
    async fn vanished_device_becomes_disconnected_keeps_reading_and_aborts_task() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;
        record_live(&mut registry, &a.id(), Some(reading_discharging(80)));
        assert!(registry.tasks.contains_key(&a.id()));

        // Next discovery sweep succeeds but no longer sees the device.
        registry
            .reconcile(vec![ok_sweep("sysfs", vec![])], &ctx)
            .await;

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
        #[expect(clippy::panic)]
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
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(PanicSource { info: a.clone() })],
                )],
                &ctx,
            )
            .await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(registry.tasks[&a.id()].handle.is_finished());

        // Simulate the device having been Online with a stale reading right
        // before its task crashed.
        record_live(&mut registry, &a.id(), Some(reading_discharging(80)));
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);

        // Next sweep still sees the device, with a fresh replacement source.
        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(55),
                    })],
                )],
                &ctx,
            )
            .await;

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].presence, Presence::Unreachable);
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));

        // The replacement task is running — its first poll proves the respawn.
        let msg = timeout(std::time::Duration::from_secs(5), mpsc_rx.recv())
            .await
            .expect("timed out waiting for replacement task's poll")
            .expect("mpsc channel closed");
        assert_eq!(msg.id, a.id());
        let SourceEvent::Polled(reading) = msg.event else {
            unreachable!("replacement task reported a reassigned node");
        };
        registry.record(&msg.id, msg.generation, reading);

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
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;
        record_live(&mut registry, &a.id(), Some(reading_discharging(80)));

        registry
            .reconcile(vec![ok_sweep("sysfs", vec![])], &ctx)
            .await;

        assert!(!registry.tasks.contains_key(&a.id()));
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    #[tokio::test]
    async fn disconnected_without_a_reading_is_dropped_immediately() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        // Never polled successfully before it vanishes — nothing to retain.
        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(ErrSource { info: a.clone() })],
                )],
                &ctx,
            )
            .await;
        registry
            .reconcile(vec![ok_sweep("sysfs", vec![])], &ctx)
            .await;

        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn reappeared_device_reuses_entry_and_returns_online() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;
        record_live(&mut registry, &a.id(), Some(reading_discharging(80)));

        // Vanishes, then reappears.
        registry
            .reconcile(vec![ok_sweep("sysfs", vec![])], &ctx)
            .await;
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;

        // Same entry reused, not duplicated.
        assert_eq!(registry.snapshot().len(), 1);
        // Retained reading still visible until the fresh task proves online.
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

        record_live(&mut registry, &a.id(), Some(reading_discharging(80)));
        assert_eq!(registry.snapshot().len(), 1);
        assert_eq!(registry.snapshot()[0].presence, Presence::Online);
    }

    // --- C2: a failed discovery sweep must not retire its own devices ---

    /// The maintainer's journal (see `.workbench/design/reviews/CONSOLIDATED.md`,
    /// C2): a momentary BlueZ hiccup made a connected keyboard "vanish"
    /// because `discover()` returning `Vec::new()` on error was
    /// indistinguishable from an honest empty sweep. A failed sweep must
    /// leave the backend's own devices exactly as they were.
    #[tokio::test]
    async fn failed_backend_sweep_does_not_retire_its_devices() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(
                vec![ok_sweep(
                    "bluez",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;
        record_live(&mut registry, &a.id(), Some(reading_discharging(80)));
        assert!(registry.tasks.contains_key(&a.id()));

        // Next sweep: the owning backend fails transiently (a bus hiccup),
        // not an honest "found nothing".
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
        assert_eq!(snapshot[0].last_reading, Some(reading_discharging(80)));
    }

    /// Counterpart to the test above: a sweep that succeeds and no longer
    /// reports a device still retires it — the fix narrows retirement to
    /// backends that actually succeeded, it does not disable retirement.
    #[tokio::test]
    async fn succeeded_backend_sweep_still_retires_its_devices() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);
        let a = device("a");

        registry
            .reconcile(
                vec![ok_sweep(
                    "bluez",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;
        record_live(&mut registry, &a.id(), Some(reading_discharging(80)));

        registry
            .reconcile(vec![ok_sweep("bluez", vec![])], &ctx)
            .await;

        assert!(!registry.tasks.contains_key(&a.id()));
        assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
    }

    /// `steelseries` and `eightbitdo` both report `Transport::Hidraw` (see
    /// `src/sources/steelseries.rs` and `src/sources/eightbitdo.rs`), so
    /// `DeviceId`/`transport` cannot answer "which backend owns this
    /// device" — this is why `DeviceEntry` tracks `backend` explicitly.
    /// One backend failing must not affect the other's retirement, even
    /// though both produce devices with the same `Transport` value.
    #[tokio::test]
    async fn backend_ownership_does_not_collapse_across_shared_transport() {
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(None);

        let mut mouse = device("aerox");
        mouse.transport = Transport::Hidraw;
        let mut controller = device("ultimate2");
        controller.transport = Transport::Hidraw;

        registry
            .reconcile(
                vec![
                    ok_sweep(
                        "steelseries",
                        vec![Box::new(OkSource {
                            info: mouse.clone(),
                            reading: reading_discharging(80),
                        })],
                    ),
                    ok_sweep(
                        "eightbitdo",
                        vec![Box::new(OkSource {
                            info: controller.clone(),
                            reading: reading_discharging(50),
                        })],
                    ),
                ],
                &ctx,
            )
            .await;
        record_live(&mut registry, &mouse.id(), Some(reading_discharging(80)));
        record_live(
            &mut registry,
            &controller.id(),
            Some(reading_discharging(50)),
        );

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

    /// End-to-end wiring test: a real `Supervisor::spawn_with` run, with
    /// fake `load_config`/`save_config` standing in for
    /// `crate::config::load`/`crate::config::save` (never the real config
    /// file — see `no_migration_load`/`no_migration_save`'s doc comment for
    /// why that substitution matters). Confirms the conversion is saved and
    /// republished onto `config_tx` once the first discovery sweep lands.
    #[tokio::test]
    async fn spawn_with_migrates_shown_devices_and_publishes_after_first_sweep() {
        let (tx, _config_rx) = config_channel(Config::default());
        let mut published = tx.subscribe();

        let (saved_tx, saved) = std::sync::mpsc::channel::<Config>();

        let (_rx, _refresh) = Supervisor::spawn_with(
            tx,
            || {
                let sources: Vec<Box<dyn BatterySource>> = vec![
                    Box::new(OkSource {
                        info: device("a"),
                        reading: reading_discharging(50),
                    }),
                    Box::new(OkSource {
                        info: device("b"),
                        reading: reading_discharging(50),
                    }),
                ];
                async move { vec![ok_sweep("sysfs", sources)] }
            },
            || Config {
                shown_devices: vec!["a".to_string()],
                ..Config::default()
            },
            move |cfg: &Config| {
                saved_tx.send(cfg.clone()).expect("test receiver alive");
                Ok(())
            },
            None,
        );

        // The migration's config_tx.send is the channel's only publisher in
        // this test, so its first change is the migrated config.
        timeout(std::time::Duration::from_secs(5), published.changed())
            .await
            .expect("timed out waiting for the migration to publish")
            .expect("config channel closed");

        let migrated = published.borrow().clone();
        assert_eq!(migrated.hidden_devices, vec!["b".to_string()]);
        assert!(migrated.shown_devices.is_empty());
        assert_eq!(saved.try_iter().last().as_ref(), Some(&migrated));
    }

    // --- state store wiring (T34) -------------------------------------------
    //
    // These exercise `DeviceRegistry` against a real `SqliteStore`, always
    // opened against a unique scratch file under the OS temp dir — never
    // `state::open()`'s real state directory. Every other test in this
    // module passes `None` (via the blanket `DeviceRegistry::new(None)` /
    // `Supervisor::spawn_with(..., None)` call sites above), so nothing
    // outside this section can touch a database file at all.

    /// Unique scratch database path under the OS temp dir for one test.
    /// Mirrors `state::store::tests::scratch_db_path`.
    fn scratch_store_path(test_name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rigbat-supervisor-state-test-{test_name}-{}-{n}.db",
            std::process::id()
        ))
    }

    fn cleanup_store(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn open_scratch_store(test_name: &str) -> (state::Store, std::path::PathBuf) {
        let path = scratch_store_path(test_name);
        let db = state::SqliteStore::open(&path).expect("opening scratch state store");
        let store = state::Store::start(db).expect("starting the store thread");
        (store, path)
    }

    #[tokio::test]
    async fn reconcile_upserts_devices_row_when_store_present() {
        let (store, path) = open_scratch_store("reconcile-upsert");
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(Some(store.clone()));
        let a = device("a");

        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;

        let devices = store.list_devices().await.expect("listing devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device, a.id());

        cleanup_store(&path);
    }

    /// A device that keeps its transport and locator but comes back under a
    /// new display name — a BlueZ alias edit — must rename its inventory row
    /// rather than gain a second one, and must tell the caller so the config
    /// entries can follow it.
    #[tokio::test]
    async fn reconcile_reports_a_rename_instead_of_adding_a_row() {
        let (store, path) = open_scratch_store("reconcile-rename");
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
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

        let renames = registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: before.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;
        assert!(renames.is_empty());

        let renames = registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: after.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;

        assert_eq!(
            renames,
            vec![("MX Anywhere 3".to_string(), "Work mouse".to_string())]
        );
        let devices = store.list_devices().await.expect("listing devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device, after.id());

        // The live roster must not keep the old name around as a Disconnected
        // duplicate: the tray would show one device twice for a day.
        let roster = registry.snapshot();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].info.name, "Work mouse");

        cleanup_store(&path);
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
        let (config_tx, config_rx) = config_channel(on_disk.clone());

        let load = {
            let on_disk = on_disk.clone();
            move || on_disk.clone()
        };
        let save = move |cfg: &Config| {
            saved_tx.send(cfg.clone()).expect("test receiver alive");
            Ok(())
        };

        apply_renames(
            &[("MX Anywhere 3".to_string(), "Work mouse".to_string())],
            &config_tx,
            &load,
            &save,
        );

        let published = config_rx.borrow().clone();
        assert_eq!(published.hidden_devices, vec!["Work mouse".to_string()]);
        assert_eq!(published.primary_device, Some("Work mouse".to_string()));
        assert_eq!(saved.try_iter().last().as_ref(), Some(&published));
    }

    #[tokio::test]
    async fn record_writes_reading_to_store_when_present() {
        let (store, path) = open_scratch_store("record-reading");
        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(Some(store.clone()));
        let a = device("a");

        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(80),
                    })],
                )],
                &ctx,
            )
            .await;
        record_live(&mut registry, &a.id(), Some(reading_discharging(80)));

        let history = store
            .recent_readings(&a.id(), 10)
            .await
            .expect("reading history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1, 80);

        cleanup_store(&path);
    }

    /// The end-to-end reason the store exists (see T34's "Why"): a device
    /// discovered for the first time in a fresh process still finds the
    /// change-point history a previous run persisted for it, so
    /// `domain::estimate` does not start from zero after every tray
    /// restart.
    #[tokio::test]
    async fn newly_discovered_device_seeds_history_from_store() {
        let (store, path) = open_scratch_store("seed-on-discovery");
        let a = device("a");
        let id = a.id();

        // Pre-populate the store directly, standing in for a previous run
        // that recorded this device's change-point history.
        store
            .record_seen(vec![(id.clone(), a.kind)], 0)
            .await
            .expect("record_seen");
        for (at, percent) in [(0i64, 80u8), (600, 79), (1200, 78)] {
            store.record_reading(&id, reading_discharging(percent), at);
        }

        let (_tx, config_rx) = config_channel(Config::default());
        let ctx = source_ctx(config_rx);
        let mut registry = DeviceRegistry::new(Some(store.clone()));

        registry
            .reconcile(
                vec![ok_sweep(
                    "sysfs",
                    vec![Box::new(OkSource {
                        info: a.clone(),
                        reading: reading_discharging(78),
                    })],
                )],
                &ctx,
            )
            .await;

        let entry = registry.entries.get(&id).expect("device entry present");
        let percents: Vec<u8> = entry.battery_history.iter().map(|&(_, p)| p).collect();
        assert_eq!(percents, vec![80, 79, 78]);

        cleanup_store(&path);
    }

    #[test]
    fn push_history_point_dedups_and_caps_like_live_polling() {
        // Exercises the free function directly (not just through
        // DeviceEntry::push_reading), since `seed_history` also drives it.
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
        let (tx, _config_rx) = config_channel(cfg);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let sweeps_seen = sweeps.clone();
        let (_rx, _refresh) = Supervisor::spawn_with(
            tx,
            move || {
                sweeps_seen.fetch_add(1, Ordering::Relaxed);
                let sources: Vec<Box<dyn BatterySource>> = vec![Box::new(OkSource {
                    info: device("mouse"),
                    reading: reading_discharging(80),
                })];
                async move { vec![ok_sweep("sysfs", sources)] }
            },
            no_migration_load,
            no_migration_save,
            None,
        );

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
        let (tx, _config_rx) = config_channel(cfg);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let sweeps_seen = sweeps.clone();
        let (mut rx, _refresh) = Supervisor::spawn_with(
            tx,
            move || {
                sweeps_seen.fetch_add(1, Ordering::Relaxed);
                let sources: Vec<Box<dyn BatterySource>> = vec![Box::new(ErrSource {
                    info: device("mouse"),
                })];
                async move { vec![ok_sweep("sysfs", sources)] }
            },
            no_migration_load,
            no_migration_save,
            None,
        );
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
            Ok(reading_discharging(80))
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
        let (tx, _config_rx) = config_channel(Config::default());
        let polls = Arc::new(AtomicUsize::new(0));
        let sweeps = Arc::new(AtomicUsize::new(0));
        let (push_tx, push_rx) = watch::channel(None);
        let (polls_seen, sweeps_seen) = (polls.clone(), sweeps.clone());
        let (mut rx, refresh) = Supervisor::spawn_with(
            tx,
            move || {
                sweeps_seen.fetch_add(1, Ordering::Relaxed);
                let sources: Vec<Box<dyn BatterySource>> = vec![Box::new(PushingSource {
                    info: device("mouse"),
                    polls: polls_seen.clone(),
                    pushes: push_rx.clone(),
                })];
                async move { vec![ok_sweep("sysfs", sources)] }
            },
            no_migration_load,
            no_migration_save,
            None,
        );
        let state = wait_for_connected(&mut rx).await;
        assert_eq!(state.devices[0].last_reading.map(|r| r.percent), Some(80));
        assert_eq!(polls.load(Ordering::Relaxed), 1);

        push_tx.send_replace(Some(reading_discharging(42)));
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
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(sweeps.load(Ordering::Relaxed), before + 1);
        assert_eq!(polls.load(Ordering::Relaxed), 1, "rediscover re-polled");
    }
}
