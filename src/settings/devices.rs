//! State behind the Devices tab (T35): merges the persisted device inventory
//! (`state::Store::list_devices`) with the current discovery scan into one
//! row per device, plus the pure filter/order/delete-confirmation/escape
//! logic the tab renders from, independent of `egui`.

use std::time::Duration;

use super::scan::ScannedDevice;
use crate::domain::{BatteryReading, DeviceId, DeviceKind, Presence};
use crate::domain::{format_age, roster_order};
use crate::i18n::Lang;
use crate::state::DeviceRecord;

/// One row of the Devices tab: the union of a device's persisted
/// inventory record (if any) and its status in the most recent discovery
/// scan (if any). Either half can be missing — freshly discovered and not
/// yet recorded (`store_id: None`), or recorded but not currently
/// discoverable (`presence: Disconnected`, `charge: None`) — but never both,
/// since a device with neither would not be a row at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    /// The inventory row's SQLite id — `Remove`'s handle. `None` for a
    /// device this scan found that `record_seen` has not persisted yet.
    pub store_id: Option<i64>,
    pub device: DeviceId,
    pub kind: DeviceKind,
    pub charge: Option<BatteryReading>,
    pub presence: Presence,
    /// The running tray's remaining-time estimate.
    pub remaining: Option<Duration>,
    /// Unix seconds of the running tray's last reading.
    pub read_at: Option<i64>,
    /// Unix seconds. `None` for a device with no inventory row yet.
    pub first_seen: Option<i64>,
    /// Unix seconds. `None` for a device with no inventory row yet.
    pub last_seen: Option<i64>,
}

/// Merges every inventory record with the current discovery scan into one
/// row per `DeviceId`. The discovered half supplies `presence`, `charge`,
/// `remaining` and `read_at`: a device the scan found is `Online` (poll succeeded),
/// `Unreachable` (poll failed) or `NoAccess` regardless of what the inventory last
/// recorded; a device the scan did not find stays `Disconnected`, keeping
/// whatever the inventory last knew about it.
pub fn merge_devices(records: Vec<DeviceRecord>, discovered: Vec<ScannedDevice>) -> Vec<DeviceRow> {
    let mut rows: Vec<DeviceRow> = records
        .into_iter()
        .map(|r| DeviceRow {
            store_id: Some(r.id),
            device: r.device,
            kind: r.kind,
            charge: None,
            presence: Presence::Disconnected,
            remaining: None,
            read_at: None,
            first_seen: Some(r.first_seen),
            last_seen: Some(r.last_seen),
        })
        .collect();

    for scanned in discovered {
        let (reading, presence) = (scanned.outcome.reading(), scanned.outcome.presence());
        let id = scanned.info.id();
        match rows.iter_mut().find(|r| r.device == id) {
            Some(row) => {
                row.charge = reading;
                row.presence = presence;
                row.remaining = scanned.remaining;
                row.read_at = scanned.read_at;
            }
            None => rows.push(DeviceRow {
                store_id: None,
                device: id,
                kind: scanned.info.kind,
                charge: reading,
                presence,
                remaining: scanned.remaining,
                read_at: scanned.read_at,
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

/// Online first, then by name, ignoring case — the roster order of every
/// surface. Stable, so same-named devices keep the merge order.
pub fn order_rows(rows: &mut [DeviceRow]) {
    rows.sort_by_cached_key(|r| roster_order(&r.device.name, r.presence));
}

/// What an expanded row offers for removing its device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteCell {
    /// The row exists only in this scan; the inventory has no row to delete.
    Unavailable,
    /// `Remove…`, which arms the confirmation and nothing else.
    Arm,
    /// `Remove` / `Cancel`: this row, and only this row, is armed.
    Confirm,
}

/// Decides which of the three states the row is in. Extracted from the
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
/// implements for a preferences window, and the reason an armed "Remove"
/// confirmation cannot be escaped into a closed window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeAction {
    CancelDelete,
    Collapse,
    ClearSearch,
    CloseWindow,
}

/// What the window has open that Escape can dismiss. `expanded` and
/// `search_active` are false on any tab but Devices, where dismissing them
/// would act on something the user cannot see.
#[derive(Debug, Clone, Copy, Default)]
pub struct Dismissible {
    pub delete_armed: bool,
    pub expanded: bool,
    pub search_active: bool,
}

pub fn escape_action(open: Dismissible) -> EscapeAction {
    if open.delete_armed {
        EscapeAction::CancelDelete
    } else if open.expanded {
        EscapeAction::Collapse
    } else if open.search_active {
        EscapeAction::ClearSearch
    } else {
        EscapeAction::CloseWindow
    }
}

/// Explicit confirm-before-delete state for `Remove…`: a destructive,
/// irreversible action needs a deliberate second step, not a first click.
/// One shared slot rather than a per-row flag — only one row can be
/// mid-confirmation at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeleteState {
    #[default]
    Idle,
    Confirming(i64),
}

/// Removes the inventory row `store_id` from the tab's backing list. The
/// caller runs this only after `state::Store::delete_device` (and the
/// matching `hidden_devices` cleanup) has already succeeded.
pub fn remove_row(rows: &mut Vec<DeviceRow>, store_id: i64) {
    rows.retain(|r| r.store_id != Some(store_id));
}

/// Formats a unix-seconds timestamp as a relative age ("3d ago"), relative
/// to `now` (also unix seconds). Reuses
/// `domain::format_age`, which already renders this exact vocabulary for
/// retained readings, instead of a second implementation.
pub fn relative_label(now: i64, at: i64, lang: Lang) -> String {
    let age = Duration::from_secs(now.saturating_sub(at).max(0).unsigned_abs());
    format_age(age, lang)
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

    fn open(delete_armed: bool, expanded: bool, search_active: bool) -> Dismissible {
        Dismissible {
            delete_armed,
            expanded,
            search_active,
        }
    }

    #[test]
    fn escape_closes_when_nothing_is_open() {
        assert_eq!(
            escape_action(open(false, false, false)),
            EscapeAction::CloseWindow
        );
    }

    #[test]
    fn escape_clears_the_search_before_closing() {
        assert_eq!(
            escape_action(open(false, false, true)),
            EscapeAction::ClearSearch
        );
    }

    #[test]
    fn escape_collapses_a_row_before_clearing_the_search() {
        assert_eq!(
            escape_action(open(false, true, true)),
            EscapeAction::Collapse
        );
    }

    /// An armed delete outranks everything else: Escape must not close the
    /// window while a confirmation is waiting.
    #[test]
    fn escape_cancels_an_armed_delete_first() {
        assert_eq!(
            escape_action(open(true, true, true)),
            EscapeAction::CancelDelete
        );
        assert_eq!(
            escape_action(open(true, false, false)),
            EscapeAction::CancelDelete
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeviceInfo, PollOutcome, Transport};

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

    fn scanned(info: DeviceInfo, outcome: PollOutcome) -> ScannedDevice {
        ScannedDevice {
            info,
            outcome,
            remaining: None,
            read_at: None,
        }
    }

    fn row(name: &str, transport: Transport, locator: Option<&str>) -> DeviceRow {
        DeviceRow {
            store_id: None,
            device: id(name, transport, locator),
            kind: DeviceKind::Mouse,
            charge: None,
            presence: Presence::Online,
            remaining: None,
            read_at: None,
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
            vec![scanned(
                info("mouse", DeviceKind::Mouse, Transport::Sysfs, None),
                PollOutcome::Reading(reading(80)),
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
            vec![scanned(
                info("mouse", DeviceKind::Mouse, Transport::Sysfs, None),
                PollOutcome::Failed,
            )],
        );
        assert_eq!(rows[0].presence, Presence::Unreachable);
        assert_eq!(rows[0].charge, None);
    }

    #[test]
    fn merge_devices_discovered_access_denial_is_no_access() {
        let rows = merge_devices(
            vec![record(1, "mouse", Transport::Hidraw, 100, 200)],
            vec![scanned(
                info("mouse", DeviceKind::Mouse, Transport::Hidraw, None),
                PollOutcome::NoAccess,
            )],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].presence, Presence::NoAccess);
        assert_eq!(rows[0].charge, None);
    }

    #[test]
    fn merge_devices_matching_device_gets_presence_and_charge_from_discovery() {
        let rows = merge_devices(
            vec![record(1, "mouse", Transport::Sysfs, 100, 200)],
            vec![scanned(
                info("mouse", DeviceKind::Mouse, Transport::Sysfs, None),
                PollOutcome::Reading(reading(55)),
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
    fn merge_devices_carries_the_trays_estimate_and_reading_time() {
        let rows = merge_devices(
            vec![record(1, "mouse", Transport::Sysfs, 100, 200)],
            vec![ScannedDevice {
                remaining: Some(Duration::from_secs(7 * 3600)),
                read_at: Some(150),
                ..scanned(
                    info("mouse", DeviceKind::Mouse, Transport::Sysfs, None),
                    PollOutcome::Reading(reading(55)),
                )
            }],
        );
        assert_eq!(rows[0].remaining, Some(Duration::from_secs(7 * 3600)));
        assert_eq!(rows[0].read_at, Some(150));
    }

    #[test]
    fn merge_devices_same_name_different_transport_stays_two_rows() {
        let rows = merge_devices(
            vec![record(1, "mouse", Transport::Sysfs, 100, 200)],
            vec![scanned(
                info("mouse", DeviceKind::Mouse, Transport::Bluetooth, None),
                PollOutcome::Reading(reading(10)),
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

    #[test]
    fn online_rows_come_first_then_by_name_ignoring_case() {
        let mut gone = row("alpha", Transport::Sysfs, None);
        gone.presence = Presence::Disconnected;
        let mut rows = vec![
            gone,
            row("zebra", Transport::Sysfs, None),
            row("Mouse", Transport::Sysfs, None),
        ];
        order_rows(&mut rows);
        let names: Vec<&str> = rows.iter().map(|r| r.device.name.as_str()).collect();
        assert_eq!(names, ["Mouse", "zebra", "alpha"]);
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

    // --- relative_label / absolute_date_label -------------------------------

    #[test]
    fn relative_label_matches_format_age() {
        assert_eq!(relative_label(1000, 1000, Lang::En), "just now");
        assert_eq!(relative_label(1000 + 3600, 1000, Lang::En), "1h ago");
    }

    #[test]
    fn relative_label_clock_step_backward_does_not_panic() {
        // at > now (a clock step, or a first-seen just recorded this second)
        // must not underflow; it renders as "just now" rather than erroring.
        assert_eq!(relative_label(1000, 5000, Lang::En), "just now");
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
