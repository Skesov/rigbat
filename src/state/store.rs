//! The SQLite database behind `state::Store`, owned by the store thread.
//!
//! WAL plus a `busy_timeout` is the documented pattern for a database two
//! processes touch (the tray writes, the settings window reads and deletes);
//! write transactions use `BEGIN IMMEDIATE` so a lock upgrade between two
//! writers cannot deadlock. `PRAGMA foreign_keys` is off by default and not
//! persisted in the file, so `configure` sets it on every connection — that is
//! what makes `ON DELETE CASCADE` fire.

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::domain::{BatteryReading, ChargeState, DeviceId, DeviceKind, Transport, state_str};

use super::{DeviceRecord, Seen};

/// How long `readings` rows are kept: headroom, since the estimator reads
/// only the newest few change points.
const RETENTION_SECS: i64 = 14 * 24 * 60 * 60;

/// A reading equal to the last stored one is written only once that row is
/// this old, so an idle device still leaves a recent row behind.
const READING_HEARTBEAT_SECS: i64 = 60 * 60;

/// `last_seen` moves only once it lags by this much, sparing a write per device
/// per discovery sweep; the inventory shows it in minutes.
const LAST_SEEN_RESOLUTION_SECS: i64 = 5 * 60;

/// Truncates the WAL after a checkpoint; SQLite's default (-1) never shrinks it.
const JOURNAL_SIZE_LIMIT_BYTES: i64 = 2 * 1024 * 1024;

/// Covers a settings-window read losing a brief race with a tray write
/// without waiting out a stuck writer forever.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens (creating if absent) the database at `path` and its parent
    /// directory, applies pragmas, and runs the schema migration.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create state directory {dir:?}"))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open state store at {path:?}"))?;
        configure(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }
}

fn configure(conn: &Connection) -> anyhow::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)
        .context("setting busy_timeout")?;
    // synchronous=NORMAL in WAL risks only the last commits on power loss, never corruption.
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;
         PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT_BYTES}; PRAGMA foreign_keys = ON;"
    ))
    .context("configuring the journal and foreign keys")?;
    Ok(())
}

/// Brings the file up to `SCHEMA_VERSION`, tracked via `PRAGMA user_version`
/// so each step is a forward migration from a known starting point, not a
/// guess about what an existing file already contains.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    // IMMEDIATE takes the write lock up front: a deferred transaction lets the
    // tray and the settings window both read version 0 and both reach the
    // CREATE, and the loser fails with "table already exists".
    conn.execute_batch("BEGIN IMMEDIATE")
        .context("starting the schema migration transaction")?;

    let applied = (|| -> anyhow::Result<()> {
        let version = schema_version(conn).context("reading schema version")?;
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
        if version < 4 {
            index_readings_by_time(conn)?;
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
const SCHEMA_VERSION: i64 = 4;

fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

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

/// Clears inventory rows keyed by a `hidrawN` node name, which changes on
/// replug. They cannot be merged — two may be one device replugged or two
/// identical ones — so the next sweep re-registers each device by its serial
/// or USB path, losing only hidraw first-seen dates and history.
fn drop_node_name_locators(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM devices
          WHERE transport = 'hidraw' AND locator GLOB 'hidraw[0-9]*'",
        [],
    )
    .context("dropping inventory rows keyed by a hidraw node name")?;
    Ok(())
}

/// Clears every sysfs inventory row keyed by a power_supply directory name,
/// which `hid-logitech-hidpp` renumbers on every reconnect. Which rows the
/// current locator scheme would reproduce cannot be known without the device
/// present, so all are re-registered, losing sysfs first-seen dates and history.
fn drop_sysfs_rows(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("DELETE FROM devices WHERE transport = 'sysfs'", [])
        .context("dropping sysfs inventory rows keyed by a power_supply directory name")?;
    Ok(())
}

/// Serves the retention prune, which deletes by age across all devices.
fn index_readings_by_time(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("CREATE INDEX IF NOT EXISTS readings_at ON readings (at)")
        .context("indexing readings by time")
}

/// The reverse of `Transport::as_str`. `None` is a value this build cannot
/// name — a newer build's variant — and the caller skips the row.
fn transport_from_str(s: &str) -> Option<Transport> {
    Transport::ALL.into_iter().find(|t| t.as_str() == s)
}

/// The reverse of `DeviceKind::as_str`; `None` as for `transport_from_str`.
fn kind_from_str(s: &str) -> Option<DeviceKind> {
    DeviceKind::ALL.into_iter().find(|k| k.as_str() == s)
}

/// The reverse of `state_str`; `None` as for `transport_from_str`.
fn state_from_str(s: &str) -> Option<ChargeState> {
    ChargeState::ALL.into_iter().find(|&c| state_str(c) == s)
}

/// Finds the `devices.id` for a `DeviceId`. `IS`, not `=`, makes a `None`
/// locator match a `NULL` column; SQLite's `UNIQUE` treats every `NULL` as
/// distinct, so lookups resolve the row here instead of via `ON CONFLICT`.
fn find_device_id(conn: &Connection, id: &DeviceId) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM devices WHERE name = ?1 AND transport = ?2 AND locator IS ?3",
        params![id.name, id.transport.as_str(), id.locator],
        |row| row.get(0),
    )
    .optional()
}

/// The newest `readings` row of a device: `at`, `percent`, `state`.
fn last_row(conn: &Connection, device_id: i64) -> rusqlite::Result<Option<(i64, i64, String)>> {
    conn.query_row(
        "SELECT at, percent, state FROM readings
          WHERE device_id = ?1 ORDER BY at DESC LIMIT 1",
        params![device_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
}

fn percent_from_db(percent: i64) -> u8 {
    u8::try_from(percent.clamp(0, 100)).unwrap_or(0)
}

fn upsert_seen(
    tx: &rusqlite::Transaction<'_>,
    id: &DeviceId,
    kind: DeviceKind,
    now: i64,
) -> anyhow::Result<Seen> {
    if let Some(row_id) = find_device_id(tx, id).context("looking up existing device row")? {
        // MAX(...) keeps last_seen monotonic even if the wall clock steps
        // backward (NTP correction, manual change): an inventory "last seen"
        // that visibly regresses is a worse failure mode than one that
        // briefly lags a backward step. `kind` is written whenever it
        // changes: a device classified wrongly once (or reclassified by a
        // later `guess_kind`) would otherwise keep the old type forever.
        tx.execute(
            "UPDATE devices SET last_seen = MAX(last_seen, ?1), kind = ?2
              WHERE id = ?3 AND (?1 - last_seen >= ?4 OR kind <> ?2)",
            params![now, kind.as_str(), row_id, LAST_SEEN_RESOLUTION_SECS],
        )
        .context("updating last_seen")?;
        return Ok(Seen::Existing);
    }
    match find_renamed(tx, id).context("looking up a renamed device row")? {
        Some((row_id, previous_name)) => {
            tx.execute(
                "UPDATE devices SET name = ?1, kind = ?2, last_seen = MAX(last_seen, ?3)
                  WHERE id = ?4",
                params![id.name, kind.as_str(), now, row_id],
            )
            .context("renaming device row")?;
            Ok(Seen::Renamed {
                from: previous_name,
            })
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
            Ok(Seen::Inserted)
        }
    }
}

impl Db {
    /// Records one discovery sweep's devices in a single transaction.
    pub(super) fn record_seen(
        &mut self,
        devices: &[(DeviceId, DeviceKind)],
        now: i64,
    ) -> anyhow::Result<Vec<Seen>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("beginning record_seen transaction")?;
        let outcomes = devices
            .iter()
            .map(|(id, kind)| upsert_seen(&tx, id, *kind, now))
            .collect::<anyhow::Result<Vec<_>>>()?;
        tx.commit().context("committing record_seen transaction")?;
        Ok(outcomes)
    }

    /// Stores a reading only if it differs from the device's last stored row,
    /// or that row is `READING_HEARTBEAT_SECS` old — `recent_readings` needs
    /// the change points, and those are exactly the changes.
    pub(super) fn record_reading(
        &mut self,
        id: &DeviceId,
        reading: BatteryReading,
        now: i64,
    ) -> anyhow::Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("beginning record_reading transaction")?;
        let Some(device_id) =
            find_device_id(&tx, id).context("looking up device row for reading")?
        else {
            // record_seen runs before a device is polled, so this does not
            // happen in practice, and is not worth an error to the caller.
            return Ok(());
        };
        let last = last_row(&tx, device_id).context("reading the last stored reading")?;
        let state = state_str(reading.state);
        if let Some((at, percent, stored_state)) = last
            && percent == i64::from(reading.percent)
            && stored_state == state
            && now.saturating_sub(at) < READING_HEARTBEAT_SECS
        {
            return Ok(());
        }
        // Timestamped as observed, not clamped forward like `last_seen`: after
        // a wall-clock step back it may overwrite one sample, which is harmless.
        tx.execute(
            "INSERT INTO readings (device_id, at, percent, state) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (device_id, at)
             DO UPDATE SET percent = excluded.percent, state = excluded.state",
            params![device_id, now, reading.percent, state],
        )
        .context("inserting reading")?;
        tx.commit().context("committing record_reading transaction")
    }

    pub(super) fn recent_readings(
        &self,
        id: &DeviceId,
        cap: usize,
    ) -> anyhow::Result<Vec<(i64, u8)>> {
        let Some(device_id) =
            find_device_id(&self.conn, id).context("looking up device row for history")?
        else {
            return Ok(Vec::new());
        };

        // Heartbeat rows and state-only changes repeat the last percent. Runs
        // of equal percent collapse to their first row, the same reduction the
        // supervisor applies to its live history.
        let mut stmt = self
            .conn
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
        stmt.query_map(params![device_id, cap as i64], |row| {
            Ok((row.get(0)?, percent_from_db(row.get(1)?)))
        })
        .context("querying recent readings")?
        .collect::<rusqlite::Result<_>>()
        .context("reading a history row")
    }

    pub(super) fn latest_reading(
        &self,
        id: &DeviceId,
    ) -> anyhow::Result<Option<(i64, BatteryReading)>> {
        let Some(device_id) = find_device_id(&self.conn, id)
            .context("looking up device row for the latest reading")?
        else {
            return Ok(None);
        };
        let last = last_row(&self.conn, device_id).context("reading the latest stored reading")?;
        Ok(last.and_then(|(at, percent, state)| {
            let reading = BatteryReading::new(percent_from_db(percent), state_from_str(&state)?);
            Some((at, reading))
        }))
    }

    pub(super) fn list_devices(&self) -> anyhow::Result<Vec<DeviceRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, name, transport, locator, kind, first_seen, last_seen \
                 FROM devices ORDER BY name",
            )
            .context("preparing list_devices query")?;
        let rows = stmt
            .query_map([], |row| {
                let transport: String = row.get(2)?;
                let kind: String = row.get(4)?;
                let name: String = row.get(1)?;
                let (Some(transport), Some(kind)) =
                    (transport_from_str(&transport), kind_from_str(&kind))
                else {
                    tracing::warn!(
                        device = %name,
                        "state store: skipping an inventory row with transport {transport:?}, kind {kind:?} this build does not know"
                    );
                    return Ok(None);
                };
                Ok(Some(DeviceRecord {
                    id: row.get(0)?,
                    device: DeviceId {
                        name,
                        transport,
                        locator: row.get(3)?,
                    },
                    kind,
                    first_seen: row.get(5)?,
                    last_seen: row.get(6)?,
                }))
            })
            .context("querying devices")?;
        let rows = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading a device row")?;
        Ok(rows.into_iter().flatten().collect())
    }

    pub(super) fn delete_device(&self, id: i64) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM devices WHERE id = ?1", params![id])
            .context("deleting device")?;
        Ok(())
    }

    pub(super) fn prune(&mut self, now: i64) -> anyhow::Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("beginning prune transaction")?;
        let cutoff = now.saturating_sub(RETENTION_SECS);
        tx.execute("DELETE FROM readings WHERE at < ?1", params![cutoff])
            .context("pruning old readings")?;
        tx.commit().context("committing prune transaction")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ScratchDir;

    fn id(name: &str, transport: Transport, locator: Option<&str>) -> DeviceId {
        DeviceId {
            name: name.to_owned(),
            transport,
            locator: locator.map(str::to_owned),
        }
    }

    fn seen(store: &mut Db, id: &DeviceId, kind: DeviceKind, now: i64) -> Seen {
        let mut outcomes = store.record_seen(&[(id.clone(), kind)], now).unwrap();
        assert_eq!(outcomes.len(), 1);
        outcomes.remove(0)
    }

    fn stored_readings(store: &Db) -> Vec<(i64, i64, String)> {
        store
            .conn
            .prepare("SELECT at, percent, state FROM readings ORDER BY at")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn opening_twice_leaves_schema_and_data_intact() {
        let scratch = ScratchDir::new("idempotent");
        let path = scratch.join("state.db");
        {
            let mut store = Db::open(&path).unwrap();
            let a = id("mouse", Transport::Sysfs, Some("a"));
            seen(&mut store, &a, DeviceKind::Mouse, 1000);
        }

        let store = Db::open(&path).unwrap();
        assert_eq!(schema_version(&store.conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(store.list_devices().unwrap().len(), 1);
    }

    /// The kind comes from `guess_kind`, which improves over time; a row
    /// classified wrongly once must not keep the old type forever.
    #[test]
    fn record_seen_adopts_a_corrected_kind() {
        let scratch = ScratchDir::new("kind-update");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let id = id("8BitDo Ultimate 2", Transport::Hidraw, Some("A1B2C3D4E5"));

        seen(&mut store, &id, DeviceKind::Other, 1000);
        seen(&mut store, &id, DeviceKind::Controller, 2000);

        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].kind, DeviceKind::Controller);
        assert_eq!(devices[0].first_seen, 1000);
    }

    /// A v1 file carries hidraw rows keyed by the node name, which the
    /// backends no longer produce. Reopening it must clear exactly those and
    /// leave every other transport alone.
    #[test]
    fn migration_to_v2_drops_hidraw_rows_keyed_by_node_name() {
        let scratch = ScratchDir::new("v2-migration");
        let path = scratch.join("state.db");
        {
            let mut store = Db::open(&path).unwrap();
            let node_keyed = id("8BitDo Ultimate 2", Transport::Hidraw, Some("hidraw13"));
            seen(&mut store, &node_keyed, DeviceKind::Controller, 1000);
            let serial_keyed = id("8BitDo Ultimate 2", Transport::Hidraw, Some("A1B2C3D4E5"));
            seen(&mut store, &serial_keyed, DeviceKind::Controller, 1000);
            let bluetooth = id(
                "NuPhy Air75",
                Transport::Bluetooth,
                Some("00:00:5E:00:53:02"),
            );
            seen(&mut store, &bluetooth, DeviceKind::Keyboard, 1000);
            store.conn.execute_batch("PRAGMA user_version = 1").unwrap();
        }

        let store = Db::open(&path).unwrap();
        let mut names: Vec<String> = store
            .list_devices()
            .unwrap()
            .into_iter()
            .map(|d| d.device.locator.unwrap_or_default())
            .collect();
        names.sort();
        assert_eq!(names, vec!["00:00:5E:00:53:02", "A1B2C3D4E5"]);
    }

    /// Same hardware identity, new display name: the row is renamed in place,
    /// keeping its first-seen date and its readings, instead of a second row
    /// appearing for what is one device.
    #[test]
    fn record_seen_renames_a_row_that_kept_its_locator() {
        let scratch = ScratchDir::new("rename");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let before = id("MX Anywhere 3", Transport::Sysfs, Some("00:00:5e:00:53:01"));
        assert_eq!(
            seen(&mut store, &before, DeviceKind::Mouse, 1000),
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
            seen(&mut store, &after, DeviceKind::Mouse, 2000),
            Seen::Renamed {
                from: "MX Anywhere 3".to_string()
            }
        );

        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device, after);
        assert_eq!(devices[0].first_seen, 1000);
        assert_eq!(store.recent_readings(&after, 10).unwrap().len(), 1);
    }

    /// Without a locator the only thing left to match on is the name, so a
    /// rename is indistinguishable from a different device: insert, never
    /// rename.
    #[test]
    fn record_seen_never_renames_a_row_with_no_locator() {
        let scratch = ScratchDir::new("rename-null-locator");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let before = id("headset", Transport::Bluetooth, None);
        seen(&mut store, &before, DeviceKind::Headset, 1000);

        let after = id("other headset", Transport::Bluetooth, None);
        assert_eq!(
            seen(&mut store, &after, DeviceKind::Headset, 2000),
            Seen::Inserted
        );
        assert_eq!(store.list_devices().unwrap().len(), 2);
    }

    /// A v2 file still keys sysfs rows by the power_supply directory name,
    /// which `hid-logitech-hidpp` rebuilds from a global counter on every
    /// reconnect. Reopening clears them and leaves the other transports alone.
    #[test]
    fn migration_to_v3_drops_sysfs_rows() {
        let scratch = ScratchDir::new("v3-migration");
        let path = scratch.join("state.db");
        {
            let mut store = Db::open(&path).unwrap();
            let sysfs = id("MX Anywhere 3", Transport::Sysfs, Some("hidpp_battery_6"));
            seen(&mut store, &sysfs, DeviceKind::Mouse, 1000);
            let bluetooth = id(
                "NuPhy Air75",
                Transport::Bluetooth,
                Some("00:00:5E:00:53:02"),
            );
            seen(&mut store, &bluetooth, DeviceKind::Keyboard, 1000);
            store.conn.execute_batch("PRAGMA user_version = 2").unwrap();
        }

        let store = Db::open(&path).unwrap();
        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device.transport, Transport::Bluetooth);
    }

    #[test]
    fn record_seen_upserts_preserving_first_seen_and_advancing_last_seen() {
        let scratch = ScratchDir::new("upsert");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));

        seen(&mut store, &a, DeviceKind::Mouse, 1000);
        seen(&mut store, &a, DeviceKind::Mouse, 2000);

        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].first_seen, 1000);
        assert_eq!(devices[0].last_seen, 2000);

        // A different locator on the same name is a different device.
        let b = id("mouse", Transport::Sysfs, Some("b"));
        seen(&mut store, &b, DeviceKind::Mouse, 3000);
        assert_eq!(store.list_devices().unwrap().len(), 2);
    }

    #[test]
    fn record_seen_dedups_none_locator_and_last_seen_never_regresses() {
        let scratch = ScratchDir::new("null-locator");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("headset", Transport::Bluetooth, None);

        seen(&mut store, &a, DeviceKind::Headset, 5000);
        // A clock step backward must not make last_seen regress...
        seen(&mut store, &a, DeviceKind::Headset, 1000);
        // ...and repeated None-locator upserts must stay one row, not two
        // (SQLite's UNIQUE constraint alone does not dedup NULL columns).
        let devices = store.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].last_seen, 5000);
    }

    #[test]
    fn list_devices_round_trips_every_kind_and_transport() {
        let scratch = ScratchDir::new("roundtrip-all");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let mut expected = Vec::new();
        for transport in Transport::ALL {
            for kind in DeviceKind::ALL {
                let device = id(
                    &format!("{}-{}", transport.as_str(), kind.as_str()),
                    transport,
                    None,
                );
                seen(&mut store, &device, kind, 42);
                expected.push((device, kind));
            }
        }

        let mut read: Vec<(DeviceId, DeviceKind)> = store
            .list_devices()
            .unwrap()
            .into_iter()
            .map(|r| (r.device, r.kind))
            .collect();
        read.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        expected.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        assert_eq!(read, expected);
    }

    /// A value this build cannot name (a newer build's variant) is left out,
    /// not read back as some other variant.
    #[test]
    fn list_devices_skips_a_row_it_cannot_decode() {
        let scratch = ScratchDir::new("unknown-enum");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let known = id("kbd", Transport::Bluetooth, None);
        seen(&mut store, &known, DeviceKind::Keyboard, 42);
        store
            .conn
            .execute_batch(
                "INSERT INTO devices (name, transport, locator, kind, first_seen, last_seen)
                 VALUES ('dongle', 'usb', NULL, 'mouse', 1, 1),
                        ('wand', 'bluetooth', NULL, 'stylus', 1, 1);",
            )
            .unwrap();

        let devices = store.list_devices().unwrap();
        let names: Vec<&str> = devices.iter().map(|d| d.device.name.as_str()).collect();
        assert_eq!(names, ["kbd"]);
    }

    #[test]
    fn delete_device_cascades_to_readings() {
        let scratch = ScratchDir::new("cascade");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));

        seen(&mut store, &a, DeviceKind::Mouse, 1000);
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
        assert!(stored_readings(&store).is_empty());
    }

    #[test]
    fn prune_deletes_rows_past_the_window_and_keeps_rows_inside_it() {
        let scratch = ScratchDir::new("retention");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        let now = 100_000_000;
        seen(&mut store, &a, DeviceKind::Mouse, now);

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
            .conn
            .prepare("SELECT at FROM readings ORDER BY at")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(remaining, vec![inside]);
    }

    #[test]
    fn recent_readings_collapses_repeated_percent_to_change_points() {
        let scratch = ScratchDir::new("dedup");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        seen(&mut store, &a, DeviceKind::Mouse, 0);

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
    }

    #[test]
    fn recent_readings_survives_reopen_ordered_and_capped() {
        let scratch = ScratchDir::new("seed");
        let path = scratch.join("state.db");
        let a = id("mouse", Transport::Sysfs, Some("a"));
        {
            let mut store = Db::open(&path).unwrap();
            seen(&mut store, &a, DeviceKind::Mouse, 0);
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
        let store = Db::open(&path).unwrap();
        let capped = store.recent_readings(&a, 3).unwrap();
        assert_eq!(capped, vec![(500, 75), (400, 76), (300, 77)]);
    }

    #[test]
    fn latest_reading_is_the_newest_row_heartbeat_included() {
        let scratch = ScratchDir::new("latest");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        let b = id("keyboard", Transport::Sysfs, Some("b"));
        seen(&mut store, &a, DeviceKind::Mouse, 0);
        seen(&mut store, &b, DeviceKind::Keyboard, 0);
        assert_eq!(store.latest_reading(&a).unwrap(), None);

        let full = BatteryReading::new(100, ChargeState::Full);
        for at in [0, READING_HEARTBEAT_SECS] {
            store.record_reading(&a, full, at).unwrap();
        }

        assert_eq!(
            store.latest_reading(&a).unwrap(),
            Some((READING_HEARTBEAT_SECS, full))
        );
        assert_eq!(store.latest_reading(&b).unwrap(), None);
    }

    #[test]
    fn repeated_identical_readings_store_one_row_plus_hourly_heartbeats() {
        let scratch = ScratchDir::new("heartbeat");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        seen(&mut store, &a, DeviceKind::Mouse, 0);

        for at in (0..=2 * READING_HEARTBEAT_SECS).step_by(60) {
            store
                .record_reading(&a, BatteryReading::new(80, ChargeState::Discharging), at)
                .unwrap();
        }

        let ats: Vec<i64> = stored_readings(&store).into_iter().map(|r| r.0).collect();
        assert_eq!(
            ats,
            vec![0, READING_HEARTBEAT_SECS, 2 * READING_HEARTBEAT_SECS]
        );
    }

    #[test]
    fn a_changed_percent_or_state_is_stored_at_once() {
        let scratch = ScratchDir::new("change");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        seen(&mut store, &a, DeviceKind::Mouse, 0);

        for (at, percent, state) in [
            (0, 80, ChargeState::Discharging),
            (60, 80, ChargeState::Discharging),
            (120, 79, ChargeState::Discharging),
            (180, 79, ChargeState::Charging),
            (240, 79, ChargeState::Charging),
        ] {
            store
                .record_reading(&a, BatteryReading::new(percent, state), at)
                .unwrap();
        }

        assert_eq!(
            stored_readings(&store),
            vec![
                (0, 80, "discharging".to_owned()),
                (120, 79, "discharging".to_owned()),
                (180, 79, "charging".to_owned()),
            ]
        );
    }

    #[test]
    fn last_seen_moves_only_past_its_resolution_but_kind_always_updates() {
        let scratch = ScratchDir::new("seen-resolution");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        let last_seen = |store: &Db| store.list_devices().unwrap()[0].last_seen;

        seen(&mut store, &a, DeviceKind::Other, 1000);
        seen(&mut store, &a, DeviceKind::Other, 1030);
        assert_eq!(last_seen(&store), 1000);

        seen(&mut store, &a, DeviceKind::Mouse, 1060);
        assert_eq!(store.list_devices().unwrap()[0].kind, DeviceKind::Mouse);

        seen(
            &mut store,
            &a,
            DeviceKind::Mouse,
            1060 + LAST_SEEN_RESOLUTION_SECS,
        );
        assert_eq!(last_seen(&store), 1060 + LAST_SEEN_RESOLUTION_SECS);
    }

    #[test]
    fn record_seen_reports_each_device_of_a_sweep_in_order() {
        let scratch = ScratchDir::new("seen-batch");
        let path = scratch.join("state.db");
        let mut store = Db::open(&path).unwrap();
        let a = id("mouse", Transport::Sysfs, Some("a"));
        let b = id("keyboard", Transport::Bluetooth, Some("b"));
        seen(&mut store, &a, DeviceKind::Mouse, 1000);

        let outcomes = store
            .record_seen(
                &[(b.clone(), DeviceKind::Keyboard), (a, DeviceKind::Mouse)],
                2000,
            )
            .unwrap();

        assert_eq!(outcomes, vec![Seen::Inserted, Seen::Existing]);
        assert_eq!(store.list_devices().unwrap().len(), 2);
    }

    /// A file written by the previous build (v3: no index on `readings.at`)
    /// opens, gains the index, and keeps its rows.
    #[test]
    fn a_v3_database_migrates_to_v4_keeping_its_rows() {
        let scratch = ScratchDir::new("v4-migration");
        let path = scratch.join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            create_schema(&conn).unwrap();
            conn.execute_batch(
                "INSERT INTO devices (id, name, transport, locator, kind, first_seen, last_seen)
                 VALUES (1, 'mouse', 'bluetooth', 'a', 'mouse', 10, 20);
                 INSERT INTO readings (device_id, at, percent, state)
                 VALUES (1, 10, 80, 'discharging'), (1, 20, 79, 'discharging');
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }

        let store = Db::open(&path).unwrap();
        assert_eq!(schema_version(&store.conn).unwrap(), 4);
        let plan: String = store
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN DELETE FROM readings WHERE at < 15",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("readings_at"),
            "prune does not use the index: {plan}"
        );
        let mouse = id("mouse", Transport::Bluetooth, Some("a"));
        assert_eq!(
            store.recent_readings(&mouse, 10).unwrap(),
            vec![(20, 79), (10, 80)]
        );
    }

    #[test]
    fn every_connection_syncs_at_normal_and_caps_the_wal() {
        let scratch = ScratchDir::new("pragmas");
        let path = scratch.join("state.db");
        let store = Db::open(&path).unwrap();
        let pragma = |name: &str| -> i64 {
            store
                .conn
                .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(pragma("synchronous"), 1, "NORMAL");
        assert_eq!(pragma("journal_size_limit"), JOURNAL_SIZE_LIMIT_BYTES);
    }
}
