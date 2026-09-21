//! State machine behind the Devices tab's inventory table (T35): merges the
//! persisted device inventory (`state::Store::list_devices`) with the
//! current discovery scan into one row per device, plus the pure
//! filter/sort/delete-confirmation logic the table renders from. Kept
//! independent of `egui` so it is unit-testable — the table widget itself is
//! not (see `settings::tests`).

use std::cmp::Ordering;
use std::time::Duration;

use crate::domain::{BatteryReading, DeviceId, DeviceInfo, DeviceKind, Presence};
use crate::domain::{format_age, state_str};
use crate::state::DeviceRecord;

/// One row of the Devices tab table: the union of a device's persisted
/// inventory record (if any) and its status in the most recent discovery
/// scan (if any). Either half can be missing — freshly discovered and not
/// yet recorded (`store_id: None`), or recorded but not currently
/// discoverable (`presence: Disconnected`, `charge: None`) — but never both,
/// since a device with neither would not be a row at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    /// The inventory row's SQLite id — `Delete`'s handle. `None` for a
    /// device this scan found that `record_seen` has not persisted yet.
    pub store_id: Option<i64>,
    pub device: DeviceId,
    pub kind: DeviceKind,
    pub charge: Option<BatteryReading>,
    pub presence: Presence,
    /// Unix seconds. `None` for a device with no inventory row yet.
    pub first_seen: Option<i64>,
    /// Unix seconds. `None` for a device with no inventory row yet.
    pub last_seen: Option<i64>,
}

/// Merges every inventory record with the current discovery scan into one
/// row per `DeviceId`. The discovered half supplies `presence` and
/// `charge`: a device the scan found is `Online` (poll succeeded) or
/// `Unreachable` (poll failed) regardless of what the inventory last
/// recorded; a device the scan did not find stays `Disconnected`, keeping
/// whatever the inventory last knew about it.
pub fn merge_devices(
    records: Vec<DeviceRecord>,
    discovered: Vec<(DeviceInfo, Option<BatteryReading>)>,
) -> Vec<DeviceRow> {
    let mut rows: Vec<DeviceRow> = records
        .into_iter()
        .map(|r| DeviceRow {
            store_id: Some(r.id),
            device: r.device,
            kind: r.kind,
            charge: None,
            presence: Presence::Disconnected,
            first_seen: Some(r.first_seen),
            last_seen: Some(r.last_seen),
        })
        .collect();

    for (info, reading) in discovered {
        let presence = if reading.is_some() {
            Presence::Online
        } else {
            Presence::Unreachable
        };
        let id = info.id();
        match rows.iter_mut().find(|r| r.device == id) {
            Some(row) => {
                row.charge = reading;
                row.presence = presence;
            }
            None => rows.push(DeviceRow {
                store_id: None,
                device: id,
                kind: info.kind,
                charge: reading,
                presence,
                first_seen: None,
                last_seen: None,
            }),
        }
    }
    rows
}

/// Keeps only rows whose device name contains `query`, case-insensitively.
/// An empty (or whitespace-only) query keeps every row.
pub fn filter_rows(rows: &[DeviceRow], query: &str) -> Vec<DeviceRow> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|r| r.device.name.to_lowercase().contains(&query))
        .cloned()
        .collect()
}

/// The table's sortable columns. `Shown` and `Delete` are actions, not data
/// the row is ordered by, so they have no variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Type,
    Transport,
    Charge,
    Presence,
    FirstSeen,
    LastSeen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    fn flipped(self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }

    fn apply(self, ord: Ordering) -> Ordering {
        match self {
            SortDirection::Ascending => ord,
            SortDirection::Descending => ord.reverse(),
        }
    }
}

/// `(column, direction)` — kept in `SettingsApp` and applied to the backing
/// `Vec` before it is handed to `TableBuilder`, since `egui_extras` has no
/// sort support of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortState {
    pub column: SortColumn,
    pub direction: SortDirection,
}

impl Default for SortState {
    /// Name, ascending — the order `list_devices` already returns, so the
    /// table looks sorted before the user has clicked anything.
    fn default() -> Self {
        Self {
            column: SortColumn::Name,
            direction: SortDirection::Ascending,
        }
    }
}

impl SortState {
    /// GNOME HIG: the first click on a column header sorts ascending; a
    /// second click on the *same* column reverses it. Clicking a different
    /// column starts that column over at ascending.
    pub fn clicked(self, column: SortColumn) -> Self {
        if self.column == column {
            Self {
                column,
                direction: self.direction.flipped(),
            }
        } else {
            Self {
                column,
                direction: SortDirection::Ascending,
            }
        }
    }
}

/// Sorts `rows` in place by `sort.column`/`sort.direction`. A row with no
/// charge, first-seen, or last-seen value sorts after every row that has
/// one, in both directions — an unknown value is neither smaller nor larger
/// than a known one, so direction cannot move it. Ties (e.g. two devices
/// sharing a name over different transports) break on the full device
/// identity, so the order is deterministic run to run.
pub fn sort_rows(rows: &mut [DeviceRow], sort: SortState) {
    rows.sort_by(|a, b| {
        let primary = match sort.column {
            SortColumn::Name => sort.direction.apply(
                a.device
                    .name
                    .to_lowercase()
                    .cmp(&b.device.name.to_lowercase()),
            ),
            SortColumn::Type => sort.direction.apply(a.kind.as_str().cmp(b.kind.as_str())),
            SortColumn::Transport => sort
                .direction
                .apply(a.device.transport.as_str().cmp(b.device.transport.as_str())),
            SortColumn::Presence => sort
                .direction
                .apply(presence_rank(a.presence).cmp(&presence_rank(b.presence))),
            SortColumn::Charge => cmp_missing_last(
                a.charge.map(|r| r.percent),
                b.charge.map(|r| r.percent),
                sort.direction,
            ),
            SortColumn::FirstSeen => cmp_missing_last(a.first_seen, b.first_seen, sort.direction),
            SortColumn::LastSeen => cmp_missing_last(a.last_seen, b.last_seen, sort.direction),
        };
        primary.then_with(|| tie_break(a, b))
    });
}

fn presence_rank(p: Presence) -> u8 {
    match p {
        Presence::Online => 0,
        Presence::Unreachable => 1,
        Presence::Disconnected => 2,
    }
}

fn cmp_missing_last<T: Ord>(a: Option<T>, b: Option<T>, direction: SortDirection) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => direction.apply(a.cmp(&b)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn tie_break(a: &DeviceRow, b: &DeviceRow) -> Ordering {
    (
        a.device.name.as_str(),
        a.device.transport.as_str(),
        a.device.locator.as_deref(),
    )
        .cmp(&(
            b.device.name.as_str(),
            b.device.transport.as_str(),
            b.device.locator.as_deref(),
        ))
}

/// What the `Actions` cell offers for one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteCell {
    /// The row exists only in this scan; the inventory has no row to delete.
    Unavailable,
    /// The neutral `Delete` button, which arms the confirmation and nothing else.
    Arm,
    /// `Confirm` / `Cancel`: this row, and only this row, is armed.
    Confirm,
}

/// Decides which of the three states the cell is in. Extracted from the
/// rendering so the "never delete on the first click" rule is testable: the
/// test that used to carry that name asserted `DeleteState::Confirming(7)`
/// equals itself and would have passed with the confirmation removed.
pub fn delete_cell(state: DeleteState, store_id: Option<i64>) -> DeleteCell {
    match store_id {
        None => DeleteCell::Unavailable,
        Some(id) if state == DeleteState::Confirming(id) => DeleteCell::Confirm,
        Some(_) => DeleteCell::Arm,
    }
}

/// What pressing Escape does, given what the window currently has open.
///
/// Escape dismisses the most transient thing first and only closes the window
/// when there is nothing left to dismiss — the behaviour every desktop toolkit
/// implements for a preferences window, and the reason an armed "Delete"
/// confirmation cannot be escaped into a closed window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeAction {
    CancelDelete,
    ClearSearch,
    CloseWindow,
}

/// `delete_armed` is whether a row's delete is waiting for confirmation;
/// `search_active` is whether the Devices tab's search box holds text (it is
/// false on any other tab, where clearing it would dismiss something the user
/// cannot see).
pub fn escape_action(delete_armed: bool, search_active: bool) -> EscapeAction {
    if delete_armed {
        EscapeAction::CancelDelete
    } else if search_active {
        EscapeAction::ClearSearch
    } else {
        EscapeAction::CloseWindow
    }
}

/// Explicit confirm-before-delete state for the table's Delete cell: a
/// destructive, irreversible action needs a deliberate second step, not a
/// first click. One shared slot rather than a per-row flag — only one row's
/// Delete cell can be mid-confirmation at a time; clicking Delete on a
/// different row just moves it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeleteState {
    #[default]
    Idle,
    Confirming(i64),
}

/// Removes the inventory row `store_id` from the table's backing list. The
/// caller runs this only after `state::Store::delete_device` (and the
/// matching `hidden_devices` cleanup) has already succeeded.
pub fn remove_row(rows: &mut Vec<DeviceRow>, store_id: i64) {
    rows.retain(|r| r.store_id != Some(store_id));
}

/// Formats a unix-seconds timestamp as a relative age ("3d ago") for the
/// table cell, relative to `now` (also unix seconds). Reuses
/// `domain::format_age`, which already renders this exact vocabulary for
/// retained readings, instead of a second implementation.
pub fn relative_label(now: i64, at: i64) -> String {
    let age = Duration::from_secs(now.saturating_sub(at).max(0).unsigned_abs());
    format_age(age)
}

/// Formats the `Charge` cell: percentage plus charge state (T36), e.g.
/// `"90%  discharging"`. Reuses `domain::state_str` so a device never reads
/// two ways in two places — the tray menu and this table spell the same
/// state identically. A device with no reading keeps the existing dash.
pub fn charge_cell_text(charge: Option<BatteryReading>) -> String {
    match charge {
        Some(r) => format!("{}%  {}", r.percent, state_str(r.state)),
        None => "—".to_string(),
    }
}

/// Formats a unix-seconds timestamp as an absolute UTC date ("2026-09-20")
/// for the relative label's hover tooltip. Hand-rolled rather than pulling
/// in `chrono`/`time` for one tooltip: `civil_from_days` is Howard Hinnant's
/// public-domain days-since-epoch-to-Gregorian-date algorithm
/// (howardhinnant.github.io/date_algorithms.html).
pub fn absolute_date_label(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097).unsigned_abs();
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    #[test]
    fn escape_closes_when_nothing_is_open() {
        assert_eq!(escape_action(false, false), EscapeAction::CloseWindow);
    }

    #[test]
    fn escape_clears_the_search_before_closing() {
        assert_eq!(escape_action(false, true), EscapeAction::ClearSearch);
    }

    /// An armed delete outranks the search box: Escape must not close the
    /// window while a confirmation is waiting.
    #[test]
    fn escape_cancels_an_armed_delete_first() {
        assert_eq!(escape_action(true, true), EscapeAction::CancelDelete);
        assert_eq!(escape_action(true, false), EscapeAction::CancelDelete);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Transport;

    fn id(name: &str, transport: Transport, locator: Option<&str>) -> DeviceId {
        DeviceId {
            name: name.to_string(),
            transport,
            locator: locator.map(str::to_string),
        }
    }

    fn info(
        name: &str,
        kind: DeviceKind,
        transport: Transport,
        locator: Option<&str>,
    ) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            kind,
            transport,
            locator: locator.map(str::to_string),
        }
    }

    fn record(
        store_id: i64,
        name: &str,
        transport: Transport,
        first_seen: i64,
        last_seen: i64,
    ) -> DeviceRecord {
        DeviceRecord {
            id: store_id,
            device: id(name, transport, None),
            kind: DeviceKind::Mouse,
            first_seen,
            last_seen,
        }
    }

    fn reading(percent: u8) -> BatteryReading {
        BatteryReading::new(percent, crate::domain::ChargeState::Discharging)
    }

    fn row(name: &str, transport: Transport, locator: Option<&str>) -> DeviceRow {
        DeviceRow {
            store_id: None,
            device: id(name, transport, locator),
            kind: DeviceKind::Mouse,
            charge: None,
            presence: Presence::Online,
            first_seen: None,
            last_seen: None,
        }
    }

    // --- merge_devices -------------------------------------------------

    #[test]
    fn merge_devices_record_only_is_disconnected_with_no_charge() {
        let rows = merge_devices(vec![record(1, "mouse", Transport::Sysfs, 100, 200)], vec![]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].store_id, Some(1));
        assert_eq!(rows[0].presence, Presence::Disconnected);
        assert_eq!(rows[0].charge, None);
        assert_eq!(rows[0].first_seen, Some(100));
        assert_eq!(rows[0].last_seen, Some(200));
    }

    #[test]
    fn merge_devices_discovered_only_has_no_store_id() {
        let rows = merge_devices(
            vec![],
            vec![(
                info("mouse", DeviceKind::Mouse, Transport::Sysfs, None),
                Some(reading(80)),
            )],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].store_id, None);
        assert_eq!(rows[0].presence, Presence::Online);
        assert_eq!(rows[0].charge, Some(reading(80)));
        assert_eq!(rows[0].first_seen, None);
    }

    #[test]
    fn merge_devices_discovered_poll_failure_is_unreachable() {
        let rows = merge_devices(
            vec![],
            vec![(
                info("mouse", DeviceKind::Mouse, Transport::Sysfs, None),
                None,
            )],
        );
        assert_eq!(rows[0].presence, Presence::Unreachable);
        assert_eq!(rows[0].charge, None);
    }

    #[test]
    fn merge_devices_matching_device_gets_presence_and_charge_from_discovery() {
        let rows = merge_devices(
            vec![record(1, "mouse", Transport::Sysfs, 100, 200)],
            vec![(
                info("mouse", DeviceKind::Mouse, Transport::Sysfs, None),
                Some(reading(55)),
            )],
        );
        assert_eq!(rows.len(), 1, "one row per DeviceId, not two");
        assert_eq!(rows[0].store_id, Some(1));
        assert_eq!(rows[0].presence, Presence::Online);
        assert_eq!(rows[0].charge, Some(reading(55)));
        // Inventory-only fields survive the merge untouched.
        assert_eq!(rows[0].first_seen, Some(100));
        assert_eq!(rows[0].last_seen, Some(200));
    }

    #[test]
    fn merge_devices_same_name_different_transport_stays_two_rows() {
        let rows = merge_devices(
            vec![record(1, "mouse", Transport::Sysfs, 100, 200)],
            vec![(
                info("mouse", DeviceKind::Mouse, Transport::Bluetooth, None),
                Some(reading(10)),
            )],
        );
        assert_eq!(rows.len(), 2);
    }

    // --- filter_rows -----------------------------------------------------

    #[test]
    fn filter_rows_empty_query_keeps_all() {
        let rows = vec![
            row("mouse", Transport::Sysfs, None),
            row("keyboard", Transport::Sysfs, None),
        ];
        assert_eq!(filter_rows(&rows, "").len(), 2);
        assert_eq!(filter_rows(&rows, "   ").len(), 2);
    }

    #[test]
    fn filter_rows_matches_case_insensitively_by_substring() {
        let rows = vec![
            row("NuPhy Air75", Transport::Sysfs, None),
            row("MX Master", Transport::Sysfs, None),
        ];
        let found = filter_rows(&rows, "air");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].device.name, "NuPhy Air75");
    }

    #[test]
    fn filter_rows_no_match_is_empty() {
        let rows = vec![row("mouse", Transport::Sysfs, None)];
        assert!(filter_rows(&rows, "headset").is_empty());
    }

    // --- sort_rows ---------------------------------------------------------

    fn names(rows: &[DeviceRow]) -> Vec<&str> {
        rows.iter().map(|r| r.device.name.as_str()).collect()
    }

    #[test]
    fn sort_rows_by_name_both_directions() {
        let mut rows = vec![
            row("zebra", Transport::Sysfs, None),
            row("alpha", Transport::Sysfs, None),
        ];
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Name,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(names(&rows), vec!["alpha", "zebra"]);
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Name,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(names(&rows), vec!["zebra", "alpha"]);
    }

    #[test]
    fn sort_rows_by_type_both_directions() {
        let mut a = row("a", Transport::Sysfs, None);
        a.kind = DeviceKind::Mouse;
        let mut b = row("b", Transport::Sysfs, None);
        b.kind = DeviceKind::Keyboard;
        let mut rows = vec![a, b];
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Type,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(rows[0].kind, DeviceKind::Keyboard);
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Type,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(rows[0].kind, DeviceKind::Mouse);
    }

    #[test]
    fn sort_rows_by_transport_both_directions() {
        let mut rows = vec![
            row("a", Transport::Sysfs, None),
            row("b", Transport::Bluetooth, None),
        ];
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Transport,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(rows[0].device.transport, Transport::Bluetooth);
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Transport,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(rows[0].device.transport, Transport::Sysfs);
    }

    #[test]
    fn sort_rows_by_presence_both_directions() {
        let mut online = row("a", Transport::Sysfs, None);
        online.presence = Presence::Online;
        let mut gone = row("b", Transport::Sysfs, None);
        gone.presence = Presence::Disconnected;
        let mut rows = vec![gone, online];
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Presence,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(rows[0].presence, Presence::Online);
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Presence,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(rows[0].presence, Presence::Disconnected);
    }

    #[test]
    fn sort_rows_by_charge_missing_value_sorts_last_both_directions() {
        let mut has_charge = row("a", Transport::Sysfs, None);
        has_charge.charge = Some(reading(50));
        let mut no_charge = row("b", Transport::Sysfs, None);
        no_charge.charge = None;
        let mut rows = vec![no_charge, has_charge];

        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Charge,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(rows[0].device.name, "a");
        assert_eq!(
            rows[1].device.name, "b",
            "missing charge sorts last ascending"
        );

        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Charge,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(rows[0].device.name, "a");
        assert_eq!(
            rows[1].device.name, "b",
            "missing charge sorts last descending too"
        );
    }

    #[test]
    fn sort_rows_by_first_seen_both_directions() {
        let mut old = row("old", Transport::Sysfs, None);
        old.first_seen = Some(100);
        let mut new = row("new", Transport::Sysfs, None);
        new.first_seen = Some(200);
        let mut rows = vec![new, old];
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::FirstSeen,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(names(&rows), vec!["old", "new"]);
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::FirstSeen,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(names(&rows), vec!["new", "old"]);
    }

    #[test]
    fn sort_rows_by_last_seen_both_directions() {
        let mut old = row("old", Transport::Sysfs, None);
        old.last_seen = Some(100);
        let mut new = row("new", Transport::Sysfs, None);
        new.last_seen = Some(200);
        let mut rows = vec![old, new];
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::LastSeen,
                direction: SortDirection::Descending,
            },
        );
        assert_eq!(names(&rows), vec!["new", "old"]);
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::LastSeen,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(names(&rows), vec!["old", "new"]);
    }

    #[test]
    fn sort_rows_ties_break_on_locator_without_panicking() {
        // Two devices named "mouse" over the same transport, one with no
        // locator — the tie-break must handle the missing locator, not
        // panic on it.
        let mut rows = vec![
            row("mouse", Transport::Sysfs, Some("b")),
            row("mouse", Transport::Sysfs, None),
        ];
        sort_rows(
            &mut rows,
            SortState {
                column: SortColumn::Name,
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(
            rows[0].device.locator, None,
            "None sorts before Some(\"b\")"
        );
        assert_eq!(rows[1].device.locator, Some("b".to_string()));
    }

    #[test]
    fn sort_state_clicked_same_column_flips_direction() {
        let sort = SortState::default();
        assert_eq!(sort.direction, SortDirection::Ascending);
        let sort = sort.clicked(SortColumn::Name);
        assert_eq!(sort.direction, SortDirection::Descending);
        let sort = sort.clicked(SortColumn::Name);
        assert_eq!(sort.direction, SortDirection::Ascending);
    }

    #[test]
    fn sort_state_clicked_different_column_resets_to_ascending() {
        let sort = SortState {
            column: SortColumn::Name,
            direction: SortDirection::Descending,
        };
        let sort = sort.clicked(SortColumn::Charge);
        assert_eq!(sort.column, SortColumn::Charge);
        assert_eq!(sort.direction, SortDirection::Ascending);
    }

    // --- delete confirmation + removal --------------------------------------

    #[test]
    fn delete_state_defaults_idle() {
        assert_eq!(DeleteState::default(), DeleteState::Idle);
    }

    /// The rule this replaces a tautology for: one click never deletes.
    #[test]
    fn first_click_only_arms_the_confirmation() {
        assert_eq!(delete_cell(DeleteState::Idle, Some(7)), DeleteCell::Arm);
    }

    #[test]
    fn the_armed_row_is_the_only_one_offering_confirm() {
        assert_eq!(
            delete_cell(DeleteState::Confirming(7), Some(7)),
            DeleteCell::Confirm
        );
        assert_eq!(
            delete_cell(DeleteState::Confirming(7), Some(8)),
            DeleteCell::Arm,
            "arming one row must not arm its neighbours"
        );
    }

    /// A device the current scan found but the inventory has not persisted yet
    /// has no row to delete.
    #[test]
    fn a_row_with_no_inventory_id_offers_nothing_to_delete() {
        assert_eq!(
            delete_cell(DeleteState::Idle, None),
            DeleteCell::Unavailable
        );
        assert_eq!(
            delete_cell(DeleteState::Confirming(7), None),
            DeleteCell::Unavailable
        );
    }

    /// Confirming is what removes the row, and it removes exactly one.
    #[test]
    fn confirming_removes_only_the_named_row() {
        let mut rows = vec![
            row("mouse", Transport::Sysfs, None),
            row("keyboard", Transport::Bluetooth, None),
        ];
        rows[0].store_id = Some(7);
        rows[1].store_id = Some(8);

        remove_row(&mut rows, 7);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device.name, "keyboard");
    }

    #[test]
    fn remove_row_leaves_other_rows_untouched() {
        let mut rows = vec![
            row("mouse", Transport::Sysfs, None),
            row("keyboard", Transport::Sysfs, None),
        ];
        rows[0].store_id = Some(1);
        rows[1].store_id = Some(2);
        remove_row(&mut rows, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device.name, "keyboard");
    }

    #[test]
    fn remove_row_ignores_unknown_id() {
        let mut rows = vec![row("mouse", Transport::Sysfs, None)];
        rows[0].store_id = Some(1);
        remove_row(&mut rows, 999);
        assert_eq!(rows.len(), 1);
    }

    // --- charge_cell_text ----------------------------------------------------

    #[test]
    fn charge_cell_text_discharging() {
        assert_eq!(
            charge_cell_text(Some(BatteryReading::new(
                90,
                crate::domain::ChargeState::Discharging
            ))),
            "90%  discharging"
        );
    }

    #[test]
    fn charge_cell_text_charging() {
        assert_eq!(
            charge_cell_text(Some(BatteryReading::new(
                42,
                crate::domain::ChargeState::Charging
            ))),
            "42%  charging"
        );
    }

    #[test]
    fn charge_cell_text_full() {
        assert_eq!(
            charge_cell_text(Some(BatteryReading::new(
                100,
                crate::domain::ChargeState::Full
            ))),
            "100%  full"
        );
    }

    #[test]
    fn charge_cell_text_missing_reading_is_dash() {
        assert_eq!(charge_cell_text(None), "—");
    }

    // --- relative_label / absolute_date_label -------------------------------

    #[test]
    fn relative_label_matches_format_age() {
        assert_eq!(relative_label(1000, 1000), "just now");
        assert_eq!(relative_label(1000 + 3600, 1000), "1h ago");
    }

    #[test]
    fn relative_label_clock_step_backward_does_not_panic() {
        // at > now (a clock step, or a first-seen just recorded this second)
        // must not underflow; it renders as "just now" rather than erroring.
        assert_eq!(relative_label(1000, 5000), "just now");
    }

    #[test]
    fn absolute_date_label_epoch() {
        assert_eq!(absolute_date_label(0), "1970-01-01");
        assert_eq!(absolute_date_label(86_400), "1970-01-02");
    }

    #[test]
    fn absolute_date_label_known_dates() {
        assert_eq!(absolute_date_label(1_735_689_600), "2025-01-01");
        assert_eq!(absolute_date_label(1_789_776_000), "2026-09-19");
    }

    #[test]
    fn absolute_date_label_before_epoch() {
        assert_eq!(absolute_date_label(-1), "1969-12-31");
    }
}
