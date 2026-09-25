//! A persistent inventory of every device this machine has seen, and each
//! device's recent battery readings — kept so the settings window can offer
//! devices the user cannot currently see (T35), and so `domain::estimate`
//! survives a tray restart instead of starting its 30-minute window over
//! every time.
//!
//! Deliberately **not** where settings live: `config::Config` is the user's
//! intent (`$XDG_CONFIG_HOME`) and must survive; this module is an
//! observation log (`$XDG_STATE_HOME`) the program can rebuild by watching
//! devices again. Chrome keeps the same split (`Preferences` JSON,
//! `History` SQLite); the XDG spec defines `STATE_HOME` as data "not
//! important or portable enough" for `DATA_HOME`.
//!
//! The store is optional by design: `open()` returns `None` on any failure
//! (no state directory, read-only home, a corrupt file) rather than an
//! `Err` a caller might feel obliged to propagate. Battery monitoring must
//! not depend on the inventory — this mirrors how `sources::bluez::watch_events`
//! degrades: an optimisation, never a dependency.

mod store;

pub use store::SqliteStore;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use tokio::sync::oneshot;

use crate::domain::{BatteryReading, DeviceId, DeviceKind};

/// How often `spawn_retention` re-runs `Store::prune` after its initial
/// startup pass.
const RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// One inventory row: a device's identity, guessed kind, and the window it
/// has been seen in. Read by the settings window to list every device this
/// machine has ever recorded, including one not currently connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    /// SQLite rowid — the handle `delete_device` operates on.
    pub id: i64,
    pub device: DeviceId,
    pub kind: DeviceKind,
    /// Unix seconds, UTC.
    pub first_seen: i64,
    /// Unix seconds, UTC. Never moves backward even if the wall clock does
    /// — see `SqliteStore::record_seen`.
    pub last_seen: i64,
}

/// What `record_seen` did to the inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// A row already existed under this exact identity; only `last_seen` moved.
    Existing,
    /// No row matched, so one was inserted.
    Inserted,
    /// A row with the same transport and locator existed under a different
    /// name and was renamed in place, keeping its history and first-seen date.
    /// The caller moves the per-device config entries across — the store never
    /// depends on `config`.
    Renamed { from: String },
}

type Job = Box<dyn FnOnce(&mut SqliteStore) + Send>;

/// A handle to the device inventory and reading history. One dedicated thread
/// owns the SQLite connection and runs requests in the order they were sent;
/// callers await the reply, so a write held by another process (up to
/// `busy_timeout`) stalls that thread, never a tokio worker.
#[derive(Clone)]
pub struct Store {
    jobs: std::sync::mpsc::Sender<Job>,
}

impl Store {
    /// Moves `db` onto its own thread, which exits once every handle is dropped.
    pub fn start(mut db: SqliteStore) -> anyhow::Result<Self> {
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("rigbat-state".into())
            .spawn(move || {
                for job in queue {
                    job(&mut db);
                }
            })
            .context("spawning the state store thread")?;
        Ok(Self { jobs })
    }

    fn send<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut SqliteStore) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<T>>> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Box::new(move |db| {
                // The caller may have stopped waiting; the work is done either way.
                let _ = reply.send(op(db));
            }))
            .map_err(|_| anyhow::anyhow!("the state store thread has stopped"))?;
        Ok(answer)
    }

    async fn call<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut SqliteStore) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        self.send(op)?
            .await
            .map_err(|_| anyhow::anyhow!("the state store thread stopped mid-request"))?
    }

    /// Upserts a `devices` row for `id`: inserts it with
    /// `first_seen = last_seen = now` if this is the first time it has been
    /// seen, or advances `last_seen` otherwise. Called once per discovered
    /// device on every discovery sweep.
    pub async fn record_seen(
        &self,
        id: &DeviceId,
        kind: DeviceKind,
        now: i64,
    ) -> anyhow::Result<Seen> {
        let id = id.clone();
        self.call(move |db| db.record_seen(&id, kind, now)).await
    }

    /// Queues one successful poll as a `readings` row and returns at once;
    /// a failure is logged on the store thread. A no-op if `id` has no
    /// `devices` row — `record_seen` always runs before a device is polled.
    pub fn record_reading(&self, id: &DeviceId, reading: BatteryReading, now: i64) {
        let id = id.clone();
        let sent = self.send(move |db| {
            if let Err(e) = db.record_reading(&id, reading, now) {
                tracing::warn!(device = %id.name, "state store: failed to record reading: {e:#}");
            }
            Ok(())
        });
        if let Err(e) = sent {
            tracing::warn!("state store: failed to record reading: {e:#}");
        }
    }

    /// The most recent `cap` percent-change points for `id` (readings where
    /// the percent differs from the previous one), most-recent-first —
    /// mirrors `push_reading`'s dedup so a caller can replay them straight
    /// into a fresh change-point history. Empty if `id` has no readings, or
    /// no `devices` row.
    pub async fn recent_readings(
        &self,
        id: &DeviceId,
        cap: usize,
    ) -> anyhow::Result<Vec<(i64, u8)>> {
        let id = id.clone();
        self.call(move |db| db.recent_readings(&id, cap)).await
    }

    /// Every device this store has ever recorded, in no particular order
    /// stronger than "stable enough for a UI to sort".
    pub async fn list_devices(&self) -> anyhow::Result<Vec<DeviceRecord>> {
        self.call(|db| db.list_devices()).await
    }

    /// Forgets a device: deletes its `devices` row and, by cascade, every
    /// `readings` row for it. Does not touch `config::Config` —
    /// `hidden_devices` is the caller's responsibility (the settings
    /// window), since this module never depends on `config`. Blocks the
    /// calling thread, which must not be a runtime thread.
    pub fn delete_device_blocking(&self, id: i64) -> anyhow::Result<()> {
        self.send(move |db| db.delete_device(id))?
            .blocking_recv()
            .map_err(|_| anyhow::anyhow!("the state store thread stopped mid-request"))?
    }

    /// Deletes `readings` rows older than the retention window, and any
    /// `readings` row whose device no longer exists (normally handled by
    /// the `ON DELETE CASCADE` foreign key already; see `SqliteStore::prune`
    /// for why this is checked again explicitly).
    pub async fn prune(&self, now: i64) -> anyhow::Result<()> {
        self.call(move |db| db.prune(now)).await
    }
}

/// `$XDG_STATE_HOME/rigbat/rigbat.db` — `None` if there is no state
/// directory (non-Linux, or no home directory in the environment).
pub fn db_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "rigbat")
        .and_then(|dirs| dirs.state_dir().map(|dir| dir.join("rigbat.db")))
}

/// Opens the state store, or returns `None` and logs a warning once if it
/// cannot: no state directory, a read-only filesystem, a corrupt file. The
/// caller runs with today's behaviour in that case — see the module doc.
pub fn open() -> Option<Store> {
    let path = match db_path() {
        Some(path) => path,
        None => {
            tracing::warn!(
                "no state directory available; device inventory and reading history are disabled"
            );
            return None;
        }
    };
    match SqliteStore::open(&path).and_then(Store::start) {
        Ok(store) => Some(store),
        Err(e) => {
            tracing::warn!(
                "failed to open state store at {path:?}: {e:#}; device inventory and reading history are disabled"
            );
            None
        }
    }
}

/// Prunes `readings` older than the retention window once at startup, then
/// again every `RETENTION_INTERVAL` for the life of the process. Best
/// effort: a failed pass is logged and tried again next interval rather
/// than taken as a reason to stop the daemon.
pub fn spawn_retention(store: Store) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = store.prune(now_unix()).await {
                tracing::warn!("state store: retention prune failed: {e:#}");
            }
            tokio::time::sleep(RETENTION_INTERVAL).await;
        }
    });
}

/// Current wall-clock time as unix seconds — the one place this project
/// persists a timestamp (`std::time::Instant` is monotonic-but-unanchored
/// and cannot survive a restart, see `domain::DeviceState::last_seen`).
/// `SystemTime::now()` before `UNIX_EPOCH` cannot happen on a real system
/// clock; falls back to `0` rather than panicking if it ever did.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ChargeState, Transport};

    fn scratch_db_path(test_name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rigbat-state-actor-test-{test_name}-{}.db",
            std::process::id()
        ))
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn mouse() -> DeviceId {
        DeviceId {
            name: "mouse".to_owned(),
            transport: Transport::Sysfs,
            locator: Some("a".to_owned()),
        }
    }

    /// A write another process holds locks the database for up to
    /// `busy_timeout`; the single runtime thread here must keep running meanwhile.
    #[tokio::test]
    async fn a_locked_database_does_not_stall_the_runtime() {
        let path = scratch_db_path("locked");
        let store = Store::start(SqliteStore::open(&path).unwrap()).unwrap();
        let other_process = rusqlite::Connection::open(&path).unwrap();
        other_process.execute_batch("BEGIN IMMEDIATE").unwrap();

        let write = tokio::spawn({
            let store = store.clone();
            async move { store.record_seen(&mouse(), DeviceKind::Mouse, 1000).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !write.is_finished(),
            "the write should still wait on the lock"
        );

        other_process.execute_batch("COMMIT").unwrap();
        assert_eq!(write.await.unwrap().unwrap(), Seen::Inserted);

        cleanup(&path);
    }

    /// A queued reading is visible to the next request: one thread, one queue.
    #[tokio::test]
    async fn requests_run_in_the_order_they_were_sent() {
        let path = scratch_db_path("ordered");
        let store = Store::start(SqliteStore::open(&path).unwrap()).unwrap();
        store
            .record_seen(&mouse(), DeviceKind::Mouse, 1000)
            .await
            .unwrap();
        store.record_reading(
            &mouse(),
            BatteryReading::new(80, ChargeState::Discharging),
            1000,
        );

        let history = store.recent_readings(&mouse(), 10).await.unwrap();
        assert_eq!(history, [(1000, 80)]);

        cleanup(&path);
    }

    #[test]
    fn now_unix_is_plausible() {
        // Sanity bound, not a precise check: some time after this module was
        // written (2026-09-19 UTC, unix ~1_789_776_000) and before an
        // obviously-wrong clock.
        let t = now_unix();
        assert!(t > 1_735_689_600, "now_unix() = {t}, looks pre-2025");
        assert!(t < 4_000_000_000, "now_unix() = {t}, looks post-2096");
    }
}
