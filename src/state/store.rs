//! SQLite implementation of `state::Store`, behind `rusqlite`'s `bundled`
//! feature (SQLite compiled from source, linked statically — see
//! `CONTRIBUTING.md`).
//!
//! WAL journal mode plus a `busy_timeout` is the documented pattern for a
//! database two processes touch concurrently (the tray writes, the settings
//! window reads and deletes); write transactions use `BEGIN IMMEDIATE` so a
//! lock upgrade between two writers cannot deadlock. `PRAGMA foreign_keys`
//! is off by default in SQLite and is not persisted in the file — it is set
//! on every connection in `configure`, which is what makes the `readings`
//! foreign key's `ON DELETE CASCADE` actually fire.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::domain::{BatteryReading, ChargeState, DeviceId, DeviceKind, Transport};

use super::{DeviceRecord, Seen, Store};

/// How long `readings` rows are kept. `domain::estimate` only ever looks at
/// the most recent `HISTORY_CAP` change points (see `app::supervisor`), so
/// this is generous headroom rather than a window the estimator needs
/// filled — it exists to keep the file in the low megabytes for a realistic
/// device count at one row per device per poll.
const RETENTION_SECS: i64 = 14 * 24 * 60 * 60;

/// A settings-window read losing a brief race with a tray write should not
/// surface as a user-visible error, but a genuinely stuck writer should not
/// be waited out forever either — a few seconds covers the first without
/// risking the second.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens (creating if absent) the database at `path`, applies pragmas,
    /// and runs the schema migration. `path`'s parent directory is created
    /// if missing — `ProjectDirs::state_dir()` names a directory this
    /// process may be the first thing to ever write into.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create state directory {dir:?}"))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open state store at {path:?}"))?;
        configure(&conn)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Recovers the connection even if a previous call panicked mid-critical
    /// section rather than propagating a poisoned-lock panic of its own —
    /// every write here runs inside an explicit transaction, so a poisoned
    /// guard still holds a connection with no half-applied statement left
    /// implicitly open outside one.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn configure(conn: &Connection) -> anyhow::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)
        .context("setting busy_timeout")?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .context("configuring WAL journal mode and foreign keys")?;
    Ok(())
}

/// Brings the file up to `SCHEMA_VERSION`, tracked via `PRAGMA user_version`
/// so each step is a forward migration from a known starting point, not a
/// guess about what an existing file already contains.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    // The version check and the CREATE must sit inside one write transaction,
    // and it has to be IMMEDIATE so the write lock is taken up front. A
    // deferred transaction lets two connections both read version 0 and both
    // reach the CREATE; the loser then fails with "table already exists".
    // Unreachable while only the single-instance-guarded tray opens the store,
    // and live the moment a second process — the settings window — opens its
    // own connection.
    conn.execute_batch("BEGIN IMMEDIATE")
        .context("starting the schema migration transaction")?;

    let applied = (|| -> anyhow::Result<()> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("reading schema version")?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        if version < 1 {
            create_schema(conn)?;
        }
        if version < 2 {
            drop_node_name_locators(conn)?;
        }
        if version < 3 {
            drop_sysfs_rows(conn)?;
        }
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .context("stamping the schema version")
    })();

    match applied {
        Ok(()) => {
            conn.execute_batch("COMMIT")
                .context("committing the schema migration")?;
            Ok(())
        }
        Err(e) => {
            // Best effort: the transaction is already failing, and a rollback
            // error would only mask the real cause.
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// The schema this build expects. `migrate` runs every step between the
/// version stamped in the file and this one.
const SCHEMA_VERSION: i64 = 3;

fn create_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS devices (
             id          INTEGER PRIMARY KEY,
             name        TEXT    NOT NULL,
             transport   TEXT    NOT NULL,
             locator     TEXT,
             kind        TEXT    NOT NULL,
             first_seen  INTEGER NOT NULL,
             last_seen   INTEGER NOT NULL,
             UNIQUE (name, transport, locator)
         );
         CREATE TABLE IF NOT EXISTS readings (
             device_id   INTEGER NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
             at          INTEGER NOT NULL,
             percent     INTEGER NOT NULL,
             state       TEXT    NOT NULL,
             PRIMARY KEY (device_id, at)
         );",
    )
    .context("creating the schema")
}

/// Finds a row for the same hardware under a different display name: same
/// transport, same non-NULL locator. That pair is the device's hardware
/// identity — a serial, a MAC, a USB path — so a row matching it is the same
/// device wearing a new name, not a different one.
///
/// A NULL locator never matches: without a locator the only thing left is the
/// name, and "same transport, no locator" would fuse two unrelated devices the
/// moment one of them was renamed.
fn find_renamed(conn: &Connection, id: &DeviceId) -> anyhow::Result<Option<(i64, String)>> {
    let Some(locator) = id.locator.as_deref() else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare(
            "SELECT id, name FROM devices
              WHERE transport = ?1 AND locator = ?2 AND name <> ?3
              ORDER BY last_seen DESC
              LIMIT 1",
        )
        .context("preparing the renamed-device lookup")?;
    let mut rows = stmt
        .query(params![id.transport.as_str(), locator, id.name])
        .context("running the renamed-device lookup")?;
    match rows.next().context("reading the renamed-device row")? {
        Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
        None => Ok(None),
    }
}

/// Clears inventory rows keyed by a `hidrawN` node name.
///
/// Before `sources::hidraw::stable_locator`, the hidraw backends used the node
/// name as the locator, and the node number changes on replug — so one
/// controller accumulated a row per enumeration order, each with its own
/// "first seen". There is no honest way to merge those rows: two rows with the
/// same name may be one device replugged or two identical devices. Dropping
/// them lets the next discovery sweep re-register each device once under its
/// serial or USB path. The cost is the first-seen date and the reading history
/// of hidraw devices only, which is exactly the loss `XDG_STATE_HOME` data is
/// defined to survive.
fn drop_node_name_locators(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM devices
          WHERE transport = 'hidraw' AND locator GLOB 'hidraw[0-9]*'",
        [],
    )
    .context("dropping inventory rows keyed by a hidraw node name")?;
    Ok(())
}

/// Clears every sysfs inventory row.
///
/// Before `sources::sysfs::stable_locator`, the sysfs backend used the
/// power_supply directory name as the locator. For a Logitech device that name
/// comes from a module-global counter in `hid-logitech-hidpp`, so it changed on
/// every reconnect and each reconnect wrote a new row.
///
/// This drops all sysfs rows rather than only the `hidpp_battery_*` ones,
/// because which stored locator the new scheme would reproduce cannot be known
/// without the device present: a supply that exposes a HID uevent now keys on
/// the serial or USB path, and one that does not keeps its directory name. A
/// row the new scheme would not reproduce is a duplicate that never merges, so
/// the honest move is to re-register them all on the next sweep. The cost is
/// first-seen dates and reading history for sysfs devices — loss-survivable by
/// definition, which is why this data lives in `XDG_STATE_HOME`.
fn drop_sysfs_rows(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("DELETE FROM devices WHERE transport = 'sysfs'", [])
        .context("dropping sysfs inventory rows keyed by a power_supply directory name")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// String encoding for the two domain enums this schema persists as TEXT.
// ---------------------------------------------------------------------------

/// The reverse of `Transport::as_str`. Falls back to `Sysfs` rather than
/// erroring: this module is the schema's only writer, so a value it cannot
/// recognise can only mean a future variant this build predates, not
/// corruption worth failing a read over.
fn transport_from_str(s: &str) -> Transport {
    match s {
        "bluetooth" => Transport::Bluetooth,
        "hidraw" => Transport::Hidraw,
        _ => Transport::Sysfs,
    }
}

/// The reverse of `DeviceKind::as_str`, with the same "unrecognised falls
/// back" reasoning as `transport_from_str` — `Other` is already the domain's
/// own catch-all for an unclassified device.
fn kind_from_str(s: &str) -> DeviceKind {
    match s {
        "mouse" => DeviceKind::Mouse,
        "keyboard" => DeviceKind::Keyboard,
        "headset" => DeviceKind::Headset,
        "controller" => DeviceKind::Controller,
        _ => DeviceKind::Other,
    }
}

fn state_to_str(s: ChargeState) -> &'static str {
    match s {
        ChargeState::Discharging => "discharging",
        ChargeState::Charging => "charging",
        ChargeState::Full => "full",
    }
}

/// Finds the `devices.id` for a `DeviceId`, using `IS` rather than `=` for
/// `locator` so a `None` locator matches a `NULL` column the same way two
/// `DeviceId`s with `locator: None` are equal in Rust. SQLite's `UNIQUE`
/// constraint cannot do this on its own — it treats every `NULL` as
/// distinct from every other `NULL` — so `record_seen`'s upsert (and every
/// other lookup by identity) resolves the row explicitly instead of relying
/// on `ON CONFLICT`.
fn find_device_id(conn: &Connection, id: &DeviceId) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM devices WHERE name = ?1 AND transport = ?2 AND locator IS ?3",
        params![id.name, id.transport.as_str(), id.locator],
        |row| row.get(0),
    )
    .optional()
}

impl Store for SqliteStore {
    fn record_seen(&self, id: &DeviceId, kind: DeviceKind, now: i64) -> anyhow::Result<Seen> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("beginning record_seen transaction")?;
        let outcome = match find_device_id(&tx, id).context("looking up existing device row")? {
            Some(row_id) => {
                // MAX(...) keeps last_seen monotonic even if the wall clock
                // steps backward (NTP correction, manual change): an
                // inventory "last seen" that visibly regresses is a worse
                // failure mode than one that briefly lags a backward step.
                // `readings.at` is not guarded the same way — see
                // `record_reading`.
                // `kind` is written on every sweep, not only on insert: a
                // device classified wrongly once (or reclassified by a later
                // `guess_kind`) would otherwise keep the old type in the
                // inventory forever, since nothing else updates the row.
                tx.execute(
                    "UPDATE devices SET last_seen = MAX(last_seen, ?1), kind = ?2 WHERE id = ?3",
                    params![now, kind.as_str(), row_id],
                )
                .context("updating last_seen")?;
                Seen::Existing
            }
            None => match find_renamed(&tx, id).context("looking up a renamed device row")? {
                Some((row_id, previous_name)) => {
                    tx.execute(
                        "UPDATE devices SET name = ?1, kind = ?2, last_seen = MAX(last_seen, ?3)
                          WHERE id = ?4",
                        params![id.name, kind.as_str(), now, row_id],
                    )
                    .context("renaming device row")?;
                    Seen::Renamed {
                        from: previous_name,
                    }
                }
                None => {
                    tx.execute(
                        "INSERT INTO devices (name, transport, locator, kind, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                        params![
                            id.name,
                            id.transport.as_str(),
                            id.locator,
                            kind.as_str(),
                            now
                        ],
                    )
                    .context("inserting new device row")?;
                    Seen::Inserted
                }
            },
        };
        tx.commit().context("committing record_seen transaction")?;
        Ok(outcome)
    }

    fn record_reading(
        &self,
        id: &DeviceId,
        reading: BatteryReading,
        now: i64,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("beginning record_reading transaction")?;
        let Some(device_id) =
            find_device_id(&tx, id).context("looking up device row for reading")?
        else {
            // record_seen always runs before a device is polled (see
            // app::supervisor::reconcile), so this should not happen in
            // practice; a missing inventory row is not worth losing the
            // reading's caller an error over.
            return Ok(());
        };
        // Timestamped as observed, not clamped forward like `last_seen`: a
        // wall-clock step backward can make this collide with (and, via
        // ON CONFLICT, overwrite) an existing row at the recomputed second,
        // losing one data point. Accepted — polls repeat every interval, so
        // one lost sample among thousands is inconsequential, unlike
        // `last_seen`, which is the field a user actually reads.
        tx.execute(
            "INSERT INTO readings (device_id, at, percent, state) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (device_id, at)
             DO UPDATE SET percent = excluded.percent, state = excluded.state",
            params![device_id, now, reading.percent, state_to_str(reading.state)],
        )
        .context("inserting reading")?;
        tx.commit().context("committing record_reading transaction")
    }

    fn recent_readings(&self, id: &DeviceId, cap: usize) -> anyhow::Result<Vec<(i64, u8)>> {
        let conn = self.conn();
        let Some(device_id) =
            find_device_id(&conn, id).context("looking up device row for history")?
        else {
            return Ok(Vec::new());
        };

        // A device polled every 60s writes one readings row per poll (see
        // RETENTION_SECS), most of which repeat the last percent. This
        // collapses runs of equal percent down to their first row —
        // exactly what `push_reading` keeps for the live, in-memory
        // history — so a caller can replay the result straight into a
        // fresh history and get what a live run would have produced,
        // instead of `cap` raw polls that are mostly duplicates.
        let mut stmt = conn
            .prepare(
                "WITH ranked AS (
                     SELECT at, percent,
                            LAG(percent) OVER (ORDER BY at) AS prev_percent
                     FROM readings
                     WHERE device_id = ?1
                 )
                 SELECT at, percent FROM ranked
                 WHERE prev_percent IS NULL OR percent <> prev_percent
                 ORDER BY at DESC
                 LIMIT ?2",
            )
            .context("preparing recent_readings query")?;
        let rows = stmt
            .query_map(params![device_id, cap as i64], |row| {
                let at: i64 = row.get(0)?;
                let percent: i64 = row.get(1)?;
                Ok((at, u8::try_from(percent.clamp(0, 100)).unwrap_or(0)))
            })
            .context("querying recent readings")?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("reading a history row")?);
        }
        Ok(out)
    }

    fn list_devices(&self) -> anyhow::Result<Vec<DeviceRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, transport, locator, kind, first_seen, last_seen \
                 FROM devices ORDER BY name",
            )
            .context("preparing list_devices query")?;
        let rows = stmt
            .query_map([], |row| {
                let transport: String = row.get(2)?;
                let kind: String = row.get(4)?;
                Ok(DeviceRecord {
                    id: row.get(0)?,
                    device: DeviceId {
                        name: row.get(1)?,
                        transport: transport_from_str(&transport),
                        locator: row.get(3)?,
                    },
                    kind: kind_from_str(&kind),
                    first_seen: row.get(5)?,
                    last_seen: row.get(6)?,
                })
            })
            .context("querying devices")?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("reading a device row")?);
        }
        Ok(out)
    }

    fn delete_device(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM devices WHERE id = ?1", params![id])
            .context("deleting device")?;
        Ok(())
    }

    fn prune(&self, now: i64) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("beginning prune transaction")?;
        let cutoff = now.saturating_sub(RETENTION_SECS);
        tx.execute("DELETE FROM readings WHERE at < ?1", params![cutoff])
            .context("pruning old readings")?;
        // Belt-and-braces: with foreign_keys=ON (set on every connection in
        // `configure`), ON DELETE CASCADE already removes a device's
        // readings the moment its devices row is deleted. This catches any
        // row that predates that pragma, or arrived through a connection
        // that did not set it.
        tx.execute(
            "DELETE FROM readings WHERE device_id NOT IN (SELECT id FROM devices)",
            [],
        )
        .context("pruning orphaned readings")?;
        tx.commit().context("committing prune transaction")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique scratch database path under the OS temp dir for one test —
    /// never the real state dir. Mirrors `config::tests::scratch_config_path`.
    fn scratch_db_path(test_name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rigbat-state-test-{test_name}-{}-{n}.db",
            std::process::id()
        ))
    }

    /// Removes the database file and its WAL/SHM sidecars.
    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn id(name: &str, transport: Transport, locator: Option<&str>) -> DeviceId {
        DeviceId {
            name: name.to_owned(),
            transport,
            locator: locator.map(str::to_owned),
        }
    }

    #[test]
    fn opening_twice_leaves_schema_and_data_intact() {
        let path = scratch_db_path("idempotent");
        {
            let store = SqliteStore::open(&path).unwrap();
            let a = id("mouse", Transport::Sysfs, Some("a"));
            store.record_seen(&a, DeviceKind::Mouse, 1000).unwrap();
        }

        let store = SqliteStore::open(&path).unwrap();
        let version: i64 = store
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(store.list_devices().unwrap().len(), 1);

        cleanup(&path);
    }

    /// The kind comes from `guess_kind`, which improves over time; a row
    /// classified wrongly once must not keep the old type forever.
    #[test]
    fn record_seen_adopts_a_corrected_kind() {
        let path = scratch_db_path("kind-update");
        let store = SqliteStore::open(&path).unwrap();
        let id = id("8BitDo Ultimate 2", Transport::Hidraw, Some("A1B2C3D4E5"));

        store.record_seen(&id, DeviceKind::Other, 1000).unwrap();
        store
            .record_seen(&id, DeviceKind::Controller, 2000)
            .unwrap();

        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].kind, DeviceKind::Controller);
        assert_eq!(devices[0].first_seen, 1000);

        cleanup(&path);
    }

    /// A v1 file carries hidraw rows keyed by the node name, which the
    /// backends no longer produce. Reopening it must clear exactly those and
    /// leave every other transport alone.
    #[test]
    fn migration_to_v2_drops_hidraw_rows_keyed_by_node_name() {
        let path = scratch_db_path("v2-migration");
        {
            let store = SqliteStore::open(&path).unwrap();
            let node_keyed = id("8BitDo Ultimate 2", Transport::Hidraw, Some("hidraw13"));
            store
                .record_seen(&node_keyed, DeviceKind::Controller, 1000)
                .unwrap();
            let serial_keyed = id("8BitDo Ultimate 2", Transport::Hidraw, Some("A1B2C3D4E5"));
            store
                .record_seen(&serial_keyed, DeviceKind::Controller, 1000)
                .unwrap();
            let bluetooth = id(
                "NuPhy Air75",
                Transport::Bluetooth,
                Some("00:00:5E:00:53:02"),
            );
            store
                .record_seen(&bluetooth, DeviceKind::Keyboard, 1000)
                .unwrap();
            store
                .conn()
                .execute_batch("PRAGMA user_version = 1")
                .unwrap();
        }

        let store = SqliteStore::open(&path).unwrap();
        let mut names: Vec<String> = store
            .list_devices()
            .unwrap()
            .into_iter()
            .map(|d| d.device.locator.unwrap_or_default())
            .collect();
        names.sort();
        assert_eq!(names, vec!["00:00:5E:00:53:02", "A1B2C3D4E5"]);

        cleanup(&path);
    }

    /// Same hardware identity, new display name: the row is renamed in place,
    /// keeping its first-seen date and its readings, instead of a second row
    /// appearing for what is one device.
    #[test]
    fn record_seen_renames_a_row_that_kept_its_locator() {
        let path = scratch_db_path("rename");
        let store = SqliteStore::open(&path).unwrap();
        let before = id("MX Anywhere 3", Transport::Sysfs, Some("00:00:5e:00:53:01"));
        assert_eq!(
            store.record_seen(&before, DeviceKind::Mouse, 1000).unwrap(),
            Seen::Inserted
        );
        store
            .record_reading(
                &before,
                BatteryReading::new(80, ChargeState::Discharging),
                1000,
            )
            .unwrap();

        let after = id("Work mouse", Transport::Sysfs, Some("00:00:5e:00:53:01"));
        assert_eq!(
            store.record_seen(&after, DeviceKind::Mouse, 2000).unwrap(),
            Seen::Renamed {
                from: "MX Anywhere 3".to_string()
            }
        );

        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device, after);
        assert_eq!(devices[0].first_seen, 1000);
        assert_eq!(store.recent_readings(&after, 10).unwrap().len(), 1);

        cleanup(&path);
    }

    /// Without a locator the only thing left to match on is the name, so a
    /// rename is indistinguishable from a different device: insert, never
    /// rename.
    #[test]
    fn record_seen_never_renames_a_row_with_no_locator() {
        let path = scratch_db_path("rename-null-locator");
        let store = SqliteStore::open(&path).unwrap();
        let before = id("headset", Transport::Bluetooth, None);
        store
            .record_seen(&before, DeviceKind::Headset, 1000)
            .unwrap();

        let after = id("other headset", Transport::Bluetooth, None);
        assert_eq!(
            store
                .record_seen(&after, DeviceKind::Headset, 2000)
                .unwrap(),
            Seen::Inserted
        );
        assert_eq!(store.list_devices().unwrap().len(), 2);

        cleanup(&path);
    }

    /// A v2 file still keys sysfs rows by the power_supply directory name,
    /// which `hid-logitech-hidpp` rebuilds from a global counter on every
    /// reconnect. Reopening clears them and leaves the other transports alone.
    #[test]
    fn migration_to_v3_drops_sysfs_rows() {
        let path = scratch_db_path("v3-migration");
        {
            let store = SqliteStore::open(&path).unwrap();
            let sysfs = id("MX Anywhere 3", Transport::Sysfs, Some("hidpp_battery_6"));
            store.record_seen(&sysfs, DeviceKind::Mouse, 1000).unwrap();
            let bluetooth = id(
                "NuPhy Air75",
                Transport::Bluetooth,
                Some("00:00:5E:00:53:02"),
            );
            store
                .record_seen(&bluetooth, DeviceKind::Keyboard, 1000)
                .unwrap();
            store
                .conn()
                .execute_batch("PRAGMA user_version = 2")
                .unwrap();
        }

        let store = SqliteStore::open(&path).unwrap();
        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device.transport, Transport::Bluetooth);

        cleanup(&path);
    }

    #[test]
    fn record_seen_upserts_preserving_first_seen_and_advancing_last_seen() {
        let path = scratch_db_path("upsert");
        let store = SqliteStore::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));

        store.record_seen(&a, DeviceKind::Mouse, 1000).unwrap();
        store.record_seen(&a, DeviceKind::Mouse, 2000).unwrap();

        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].first_seen, 1000);
        assert_eq!(devices[0].last_seen, 2000);

        // A different locator on the same name is a different device.
        let b = id("mouse", Transport::Sysfs, Some("b"));
        store.record_seen(&b, DeviceKind::Mouse, 3000).unwrap();
        assert_eq!(store.list_devices().unwrap().len(), 2);

        cleanup(&path);
    }

    #[test]
    fn record_seen_dedups_none_locator_and_last_seen_never_regresses() {
        let path = scratch_db_path("null-locator");
        let store = SqliteStore::open(&path).unwrap();
        let a = id("headset", Transport::Bluetooth, None);

        store.record_seen(&a, DeviceKind::Headset, 5000).unwrap();
        // A clock step backward must not make last_seen regress...
        store.record_seen(&a, DeviceKind::Headset, 1000).unwrap();
        // ...and repeated None-locator upserts must stay one row, not two
        // (SQLite's UNIQUE constraint alone does not dedup NULL columns).
        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].last_seen, 5000);

        cleanup(&path);
    }

    #[test]
    fn list_devices_round_trips_kind_and_transport() {
        let path = scratch_db_path("roundtrip");
        let store = SqliteStore::open(&path).unwrap();
        let a = id("kbd", Transport::Bluetooth, None);

        store.record_seen(&a, DeviceKind::Keyboard, 42).unwrap();
        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device, a);
        assert_eq!(devices[0].kind, DeviceKind::Keyboard);

        cleanup(&path);
    }

    #[test]
    fn delete_device_cascades_to_readings() {
        let path = scratch_db_path("cascade");
        let store = SqliteStore::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));

        store.record_seen(&a, DeviceKind::Mouse, 1000).unwrap();
        store
            .record_reading(&a, BatteryReading::new(80, ChargeState::Discharging), 1000)
            .unwrap();
        store
            .record_reading(&a, BatteryReading::new(79, ChargeState::Discharging), 1100)
            .unwrap();

        let row_id = store.list_devices().unwrap()[0].id;
        store.delete_device(row_id).unwrap();

        assert!(store.list_devices().unwrap().is_empty());
        assert!(store.recent_readings(&a, 10).unwrap().is_empty());

        cleanup(&path);
    }

    #[test]
    fn prune_deletes_rows_past_the_window_and_keeps_rows_inside_it() {
        let path = scratch_db_path("retention");
        let store = SqliteStore::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        let now = 100_000_000;
        store.record_seen(&a, DeviceKind::Mouse, now).unwrap();

        let inside = now - RETENTION_SECS + 10;
        let outside = now - RETENTION_SECS - 10;
        store
            .record_reading(
                &a,
                BatteryReading::new(80, ChargeState::Discharging),
                inside,
            )
            .unwrap();
        store
            .record_reading(
                &a,
                BatteryReading::new(50, ChargeState::Discharging),
                outside,
            )
            .unwrap();

        store.prune(now).unwrap();

        let remaining: Vec<i64> = store
            .conn()
            .prepare("SELECT at FROM readings ORDER BY at")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(remaining, vec![inside]);

        cleanup(&path);
    }

    #[test]
    fn recent_readings_collapses_repeated_percent_to_change_points() {
        let path = scratch_db_path("dedup");
        let store = SqliteStore::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        store.record_seen(&a, DeviceKind::Mouse, 0).unwrap();

        for at in [0, 60, 120] {
            store
                .record_reading(&a, BatteryReading::new(80, ChargeState::Discharging), at)
                .unwrap();
        }
        store
            .record_reading(&a, BatteryReading::new(79, ChargeState::Discharging), 180)
            .unwrap();

        let history = store.recent_readings(&a, 10).unwrap();
        assert_eq!(history, vec![(180, 79), (0, 80)]);

        cleanup(&path);
    }

    #[test]
    fn recent_readings_survives_reopen_ordered_and_capped() {
        let path = scratch_db_path("seed");
        let a = id("mouse", Transport::Sysfs, Some("a"));
        {
            let store = SqliteStore::open(&path).unwrap();
            store.record_seen(&a, DeviceKind::Mouse, 0).unwrap();
            for (i, percent) in [80u8, 79, 78, 77, 76, 75].into_iter().enumerate() {
                store
                    .record_reading(
                        &a,
                        BatteryReading::new(percent, ChargeState::Discharging),
                        i as i64 * 100,
                    )
                    .unwrap();
            }
        }

        // Reopen — history must survive a restart, not just live in the
        // handle that wrote it.
        let store = SqliteStore::open(&path).unwrap();
        let capped = store.recent_readings(&a, 3).unwrap();
        assert_eq!(capped, vec![(500, 75), (400, 76), (300, 77)]);

        cleanup(&path);
    }
}
