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
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// A persisted device inventory and reading history, behind a port so the
/// supervisor's tests never need a real database — the same shape
/// `discovery::Context` uses to inject infrastructure from the composition
/// root. All methods are synchronous: SQLite I/O here is the same kind of
/// brief, direct blocking call `config::save` already makes from async
/// contexts in this codebase, not something that needs `spawn_blocking`.
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

pub trait Store: Send + Sync {
    /// Upserts a `devices` row for `id`: inserts it with
    /// `first_seen = last_seen = now` if this is the first time it has been
    /// seen, or advances `last_seen` otherwise. Called once per discovered
    /// device on every discovery sweep.
    fn record_seen(&self, id: &DeviceId, kind: DeviceKind, now: i64) -> anyhow::Result<Seen>;

    /// Records one successful poll as a `readings` row. A no-op (not an
    /// error) if `id` has no `devices` row yet — `record_seen` is always
    /// called before a device can be polled, so this should not happen in
    /// practice, but a reading is not worth losing the device's inventory
    /// entry over.
    fn record_reading(
        &self,
        id: &DeviceId,
        reading: BatteryReading,
        now: i64,
    ) -> anyhow::Result<()>;

    /// The most recent `cap` percent-change points for `id` (readings where
    /// the percent differs from the previous one), most-recent-first —
    /// mirrors `push_reading`'s dedup so a caller can replay them straight
    /// into a fresh change-point history. Empty if `id` has no readings, or
    /// no `devices` row.
    fn recent_readings(&self, id: &DeviceId, cap: usize) -> anyhow::Result<Vec<(i64, u8)>>;

    /// Every device this store has ever recorded, in no particular order
    /// stronger than "stable enough for a UI to sort".
    fn list_devices(&self) -> anyhow::Result<Vec<DeviceRecord>>;

    /// Forgets a device: deletes its `devices` row and, by cascade, every
    /// `readings` row for it. Does not touch `config::Config` —
    /// `hidden_devices` is the caller's responsibility (the settings
    /// window), since this module never depends on `config`.
    fn delete_device(&self, id: i64) -> anyhow::Result<()>;

    /// Deletes `readings` rows older than the retention window, and any
    /// `readings` row whose device no longer exists (normally handled by
    /// the `ON DELETE CASCADE` foreign key already; see `SqliteStore::prune`
    /// for why this is checked again explicitly).
    fn prune(&self, now: i64) -> anyhow::Result<()>;
}

/// `$XDG_STATE_HOME/rigbat/rigbat.db` — `None` if there is no state
/// directory (non-Linux, or no home directory in the environment).
fn db_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "rigbat")
        .and_then(|dirs| dirs.state_dir().map(|dir| dir.join("rigbat.db")))
}

/// Opens the state store, or returns `None` and logs a warning once if it
/// cannot: no state directory, a read-only filesystem, a corrupt file. The
/// caller runs with today's behaviour in that case — see the module doc.
pub fn open() -> Option<Arc<dyn Store>> {
    let path = match db_path() {
        Some(path) => path,
        None => {
            tracing::warn!(
                "no state directory available; device inventory and reading history are disabled"
            );
            return None;
        }
    };
    match SqliteStore::open(&path) {
        Ok(store) => Some(Arc::new(store)),
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
pub fn spawn_retention(store: Arc<dyn Store>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = store.prune(now_unix()) {
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
