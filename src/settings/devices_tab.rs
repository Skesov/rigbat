use std::collections::HashMap;

use eframe::egui;
use egui_extras::{Column, Size, StripBuilder, TableBuilder};

use super::devices::{self, DeleteState, DeviceRow, SortColumn, SortState};
use super::{LOW_THRESHOLD_RANGE, SettingsApp, Tab, scan};
use crate::config::DeviceSettings;
use crate::domain::{Presence, TrayMode};
use crate::i18n::{Lang, fl, loader};
use crate::state;

/// Height reserved under the Devices tab table for the selected device's
/// settings, and only while one is selected.
///
/// Below the table rather than beside it: a side panel charges its width to
/// every frame, including the ones where nothing is selected, and the table is
/// nine columns wide already. It is a panel rather than an expanding row
/// because `TableBody::rows` renders homogeneous row heights for its
/// virtualisation to stay simple; the selection survives sort and filter
/// because it is keyed by device name, not by row index.
const DEVICE_DETAIL_HEIGHT: f32 = 104.0;

/// Row and header heights for the Devices tab table.
const TABLE_ROW_HEIGHT: f32 = 22.0;
const TABLE_HEADER_HEIGHT: f32 = 24.0;

/// Width of the separator column between `Tray icon` (a checkbox that only
/// hides an icon) and `Actions` (`Delete`, which destroys the inventory
/// record): visible space plus a vertical rule so the two controls do not
/// read as one action a stray click could confuse (T36).
const TOGGLE_ACTIONS_GAP_WIDTH: f32 = 20.0;

/// Global poll interval range, seconds. The lower bound guards against waking
/// a HID device every second, which drains the battery it is meant to monitor.
const POLL_INTERVAL_RANGE: std::ops::RangeInclusive<u64> = 10..=3600;

impl SettingsApp {
    /// Renders a "Refresh" button, disabled and relabeled while a scan is
    /// already in flight.
    ///
    /// Named the same as the tray menu's "Refresh" item (T36): with a tray
    /// running it triggers the same re-poll, and without one it runs a
    /// one-shot discovery-and-poll in the settings process — both mean "go
    /// look again now" from the user's side.
    fn render_refresh_button(&mut self, ui: &mut egui::Ui) {
        let egui_ctx = ui.ctx().clone();
        let l = loader(self.config.lang());
        ui.add_enabled_ui(!self.scanning, |ui| {
            let label = if self.scanning {
                fl!(l, "button-refreshing")
            } else {
                fl!(l, "button-refresh")
            };
            if ui.button(label).clicked() {
                self.spawn_scan(egui_ctx.clone(), scan::Scan::Refresh);
            }
        });
    }

    /// Renders the threshold/interval override controls for one device, by
    /// name, in the Devices tab's detail panel. Persists only on release
    /// (`drag_stopped`/`lost_focus`) or checkbox toggle, never on every
    /// dragged pixel — same rule the pre-T35 collapsing-header version
    /// followed, this is that same body applied to one selected device
    /// instead of looped over every discovered one.
    ///
    /// Unchecked, each checkbox's own label states the effective (global)
    /// value it falls back to — e.g. "Use default (20%)" (T36) — so the
    /// relationship to the General tab's sliders is visible in the control
    /// itself, not only in a separate line of helper text.
    /// The aggregate icon's device picker (R27): `primary_device` steered the
    /// single-icon mode since M2 but became unreachable when T22 removed the
    /// tray-menu control that wrote it, leaving it editable only by hand in
    /// `config.json`.
    ///
    /// It lives here rather than as a table column because it is an action on
    /// one device, not a property of every row — a tenth column for a setting
    /// that applies to exactly one device at a time would cost every row width
    /// to show a value that is empty in all but one of them. The General tab
    /// names the current choice and points here, so the setting is discoverable
    /// from the section whose behaviour it changes without duplicating the
    /// control.
    fn render_primary_control(&mut self, ui: &mut egui::Ui, name: &str) {
        let l = loader(self.config.lang());
        let mut is_primary = self.config.primary_device.as_deref() == Some(name);
        if ui
            .checkbox(&mut is_primary, fl!(l, "detail-use-for-single-icon"))
            .changed()
        {
            let name = name.to_string();
            self.persist(move |target| {
                toggle_primary(&mut target.primary_device, &name, is_primary);
            });
        }
        if self.config.tray_mode == TrayMode::PerDevice {
            ui.label(egui::RichText::new(fl!(l, "detail-single-icon-hint")).weak());
        }
    }

    /// The selected device's low-battery threshold: a checkbox that reads
    /// `Use default (20%)` until it is ticked, then the override's slider.
    /// Split from the poll-interval control so the two can sit side by side in
    /// the detail band; both write through `apply_device_override`, which
    /// drops an override equal to the current default rather than storing a
    /// value that only looks like a decision.
    fn render_threshold_override(&mut self, ui: &mut egui::Ui, name: &str) {
        let default_threshold = self.config.low_threshold;
        let existing = self.config.device_overrides.get(name).cloned();
        let mut on = existing.as_ref().is_some_and(|d| d.low_threshold.is_some());
        let mut value = existing
            .as_ref()
            .and_then(|d| d.low_threshold)
            .unwrap_or(default_threshold);

        let l = loader(self.config.lang());
        let label = if on {
            fl!(l, "detail-override-threshold")
        } else {
            fl!(l, "detail-default-threshold", percent = default_threshold)
        };
        let mut save = ui.checkbox(&mut on, label).changed();
        if on {
            let resp = ui.add(egui::Slider::new(&mut value, LOW_THRESHOLD_RANGE).suffix("%"));
            save |= resp.drag_stopped() || resp.lost_focus();
        }

        if save {
            self.save_threshold_override(name, on.then_some(value));
        }
    }

    /// The selected device's poll interval; the threshold control's twin.
    fn render_interval_override(&mut self, ui: &mut egui::Ui, name: &str) {
        let default_interval = self.config.poll_interval_secs;
        let existing = self.config.device_overrides.get(name).cloned();
        let mut on = existing
            .as_ref()
            .is_some_and(|d| d.poll_interval_secs.is_some());
        let mut value = existing
            .as_ref()
            .and_then(|d| d.poll_interval_secs)
            .unwrap_or(default_interval);

        let l = loader(self.config.lang());
        let label = if on {
            fl!(l, "detail-override-interval")
        } else {
            fl!(l, "detail-default-interval", secs = default_interval)
        };
        let mut save = ui.checkbox(&mut on, label).changed();
        if on {
            let resp = ui.add(
                egui::Slider::new(&mut value, POLL_INTERVAL_RANGE)
                    .suffix(fl!(l, "unit-seconds-suffix")),
            );
            save |= resp.drag_stopped() || resp.lost_focus();
        }

        if save {
            self.save_interval_override(name, on.then_some(value));
        }
    }

    /// Writes one device's threshold override, leaving its interval override
    /// as the on-disk config has it — the two controls are rendered
    /// separately, so neither may write the other's field from a snapshot.
    fn save_threshold_override(&mut self, name: &str, threshold: Option<u8>) {
        let name = name.to_string();
        self.persist(move |target| {
            // Defaults come from `target`, the config being written, not from
            // the window's snapshot: `apply_device_override` drops an override
            // equal to the current default, and a second settings window (there
            // is no single-instance guard) can have changed that default since
            // this one opened.
            let default_threshold = target.low_threshold;
            let default_interval = target.poll_interval_secs;
            let interval = target
                .device_overrides
                .get(&name)
                .and_then(|d| d.poll_interval_secs);
            apply_device_override(
                &mut target.device_overrides,
                &name,
                threshold,
                interval,
                default_threshold,
                default_interval,
            );
        });
    }

    /// The interval half of `save_threshold_override`.
    fn save_interval_override(&mut self, name: &str, interval: Option<u64>) {
        let name = name.to_string();
        self.persist(move |target| {
            let default_threshold = target.low_threshold;
            let default_interval = target.poll_interval_secs;
            let threshold = target
                .device_overrides
                .get(&name)
                .and_then(|d| d.low_threshold);
            apply_device_override(
                &mut target.device_overrides,
                &name,
                threshold,
                interval,
                default_threshold,
                default_interval,
            );
        });
    }

    /// The Devices tab: search box, Refresh, then the inventory table beside
    /// the selected device's override panel. Source is the union
    /// `apply_scan_result` already merged into `self.device_rows` — union,
    /// not `self.devices` alone, is the whole point of T35: a device the
    /// inventory remembers but the current scan did not find must still
    /// show up here.
    pub(super) fn render_devices_tab(&mut self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let l = loader(lang);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.device_search)
                    .hint_text(fl!(l, "device-search-hint"))
                    .desired_width(220.0),
            );
            self.render_refresh_button(ui);
        });
        ui.add_space(8.0);

        let filtered = devices::filter_rows(&self.device_rows, &self.device_search);
        if filtered.is_empty() {
            let message = if self.device_rows.is_empty() {
                fl!(l, "devices-empty")
            } else {
                fl!(l, "devices-no-match")
            };
            ui.label(egui::RichText::new(message).weak());
            return;
        }

        let mut rows = filtered;
        devices::sort_rows(&mut rows, self.device_sort, lang);
        let now = state::now_unix();

        // The detail band claims no height at all while nothing is selected,
        // so an unselected table is not paying for a panel showing a sentence
        // about what selecting would do.
        let detail_height = if self.selected_device.is_some() {
            DEVICE_DETAIL_HEIGHT
        } else {
            0.0
        };
        StripBuilder::new(ui)
            .size(Size::remainder().at_least(120.0))
            .size(Size::exact(detail_height))
            .vertical(|mut strip| {
                strip.cell(|ui| self.render_device_table(ui, &rows, now));
                strip.cell(|ui| self.render_device_detail(ui));
            });
    }

    /// The inventory table itself: sticky header with click-to-sort columns,
    /// a virtualised body (`TableBody::rows` — the inventory grows without
    /// bound, so only visible rows are built), resizable columns.
    fn render_device_table(&mut self, ui: &mut egui::Ui, rows: &[DeviceRow], now: i64) {
        let l = loader(self.config.lang());
        let sort = self.device_sort;
        let mut clicked_sort: Option<SortColumn> = None;

        TableBuilder::new(ui)
            .id_salt("devices_table")
            .striped(true)
            .resizable(true)
            .column(Column::initial(190.0).at_least(110.0).resizable(true))
            .column(Column::initial(76.0).at_least(60.0).resizable(true))
            .column(Column::initial(86.0).at_least(70.0).resizable(true))
            .column(Column::initial(116.0).at_least(90.0).resizable(true))
            .column(Column::initial(92.0).at_least(70.0).resizable(true))
            .column(Column::initial(82.0).at_least(70.0).resizable(true))
            .column(Column::initial(82.0).at_least(70.0).resizable(true))
            .column(Column::initial(62.0).at_least(56.0).resizable(false))
            .column(Column::exact(TOGGLE_ACTIONS_GAP_WIDTH).resizable(false))
            .column(Column::initial(76.0).at_least(70.0).resizable(false))
            .header(TABLE_HEADER_HEIGHT, |mut header| {
                let columns: [(String, Option<SortColumn>); 10] = [
                    (fl!(l, "col-name"), Some(SortColumn::Name)),
                    (fl!(l, "col-type"), Some(SortColumn::Type)),
                    (fl!(l, "col-connection"), Some(SortColumn::Transport)),
                    (fl!(l, "col-charge"), Some(SortColumn::Charge)),
                    (fl!(l, "col-status"), Some(SortColumn::Presence)),
                    (fl!(l, "col-first-seen"), Some(SortColumn::FirstSeen)),
                    (fl!(l, "col-last-seen"), Some(SortColumn::LastSeen)),
                    (fl!(l, "col-tray-icon"), None),
                    (String::new(), None),
                    (fl!(l, "col-actions"), None),
                ];
                for (label, sort_column) in columns {
                    header.col(|ui| match sort_column {
                        Some(column) => {
                            if ui.button(header_label(&label, column, sort)).clicked() {
                                clicked_sort = Some(column);
                            }
                        }
                        None => {
                            ui.label(label);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(TABLE_ROW_HEIGHT, rows.len(), |mut table_row| {
                    let index = table_row.index();
                    if let Some(row) = rows.get(index) {
                        self.render_device_row(&mut table_row, row, now);
                    }
                });
            });

        if let Some(column) = clicked_sort {
            self.device_sort = self.device_sort.clicked(column);
        }
    }

    /// Renders one table row's ten cells, in the same order as
    /// `render_device_table`'s header.
    fn render_device_row(
        &mut self,
        table_row: &mut egui_extras::TableRow<'_, '_>,
        row: &DeviceRow,
        now: i64,
    ) {
        let lang = self.config.lang();
        let l = loader(lang);
        table_row.col(|ui| {
            let is_selected = self.selected_device.as_deref() == Some(row.device.name.as_str());
            // An empty selectable button covering the cell carries the row's
            // selection background, hover feedback and click; the name is then
            // drawn inside it. `add_sized` is what makes it cover the cell —
            // a button whose text is empty is otherwise a few pixels wide, and
            // the name drawn into that rect truncates away to nothing.
            let resp = ui
                .add_sized(
                    ui.available_size(),
                    egui::Button::selectable(is_selected, ""),
                )
                .on_hover_text(&row.device.name);
            // Left-aligned like every other column, which `Ui::put` would not
            // be: it centres what it places. Truncated, not wrapped, because
            // the row height is fixed and a second line spills into the row
            // below.
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(resp.rect.shrink2(egui::vec2(6.0, 0.0)))
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |ui| {
                    ui.add(
                        egui::Label::new(&row.device.name)
                            .truncate()
                            .selectable(false),
                    );
                },
            );
            if resp.clicked() {
                self.selected_device = Some(row.device.name.clone());
            }
        });
        table_row.col(|ui| {
            ui.label(row.kind.label(lang));
        });
        table_row.col(|ui| {
            ui.label(row.device.transport.as_str());
        });
        table_row.col(|ui| {
            ui.label(devices::charge_cell_text(row.charge, lang));
        });
        table_row.col(|ui| {
            let (label, weak) = match row.presence {
                Presence::Online => (fl!(l, "presence-online"), false),
                Presence::Unreachable => (fl!(l, "presence-unreachable"), false),
                Presence::NoAccess => (fl!(l, "presence-no-access"), false),
                Presence::Disconnected => (fl!(l, "presence-disconnected"), true),
            };
            let text = egui::RichText::new(label);
            ui.label(if weak { text.weak() } else { text });
        });
        table_row.col(|ui| render_seen_cell(ui, row.first_seen, now, lang));
        table_row.col(|ui| render_seen_cell(ui, row.last_seen, now, lang));
        table_row.col(|ui| {
            let mut shown = self.config.is_shown(&row.device.name);
            if ui.checkbox(&mut shown, "").changed() {
                let name = row.device.name.clone();
                self.persist(move |target| {
                    toggle_hidden(&mut target.hidden_devices, &name, shown);
                });
            }
        });
        // Separator column (T36): visible space plus a rule between the
        // Tray icon toggle and Delete so the two never read as one action.
        table_row.col(|ui| {
            ui.add(egui::Separator::default().vertical());
        });
        table_row.col(|ui| self.render_delete_cell(ui, row));
    }

    /// Delete needs a deliberate second click: the first arms
    /// `self.delete_state` and swaps the cell to Yes/No in place,
    /// per T35 ("do not delete on first click"). A row the scan discovered
    /// but the inventory has not persisted yet (`store_id: None`) has
    /// nothing to delete.
    ///
    /// Only the armed `Confirm` is tinted: a red `Delete` on every row turns
    /// the column into a wall of warnings for an action nobody asked for yet.
    fn render_delete_cell(&mut self, ui: &mut egui::Ui, row: &DeviceRow) {
        let l = loader(self.config.lang());
        match devices::delete_cell(self.delete_state, row.store_id) {
            devices::DeleteCell::Unavailable => {
                ui.label("—");
            }
            devices::DeleteCell::Confirm => {
                let Some(store_id) = row.store_id else { return };
                ui.horizontal(|ui| {
                    if destructive_small_button(ui, &fl!(l, "button-confirm")).clicked() {
                        self.delete_device(store_id, &row.device.name);
                    }
                    if ui.small_button(fl!(l, "button-cancel")).clicked() {
                        self.delete_state = DeleteState::Idle;
                    }
                });
            }
            devices::DeleteCell::Arm => {
                let Some(store_id) = row.store_id else { return };
                if neutral_small_button(ui, &fl!(l, "button-delete")).clicked() {
                    self.delete_state = DeleteState::Confirming(store_id);
                }
            }
        }
    }

    /// Forgets a device (T34): deletes its inventory row and readings, its
    /// `hidden_devices` entry — nothing left to show it as hidden once it no
    /// longer exists — and its pin on the aggregate icon, which would
    /// otherwise reattach itself the moment a sold device is plugged in
    /// somewhere else and seen again. The UI thread waits for the store, the
    /// same as `persist`'s config write: a deliberate, infrequent,
    /// user-confirmed click, not the per-frame inventory read `spawn_scan`
    /// keeps off the UI thread.
    fn delete_device(&mut self, store_id: i64, name: &str) {
        if let Some(store) = &self.store
            && let Err(e) = store.delete_device_blocking(store_id)
        {
            tracing::error!("failed to delete device {name:?} from inventory: {e}");
            self.delete_state = DeleteState::Idle;
            return;
        }

        let forgotten = name.to_string();
        self.persist(move |target| {
            toggle_hidden(&mut target.hidden_devices, &forgotten, true);
            toggle_primary(&mut target.primary_device, &forgotten, false);
        });
        devices::remove_row(&mut self.device_rows, store_id);
        if self.selected_device.as_deref() == Some(name) {
            self.selected_device = None;
        }
        self.delete_state = DeleteState::Idle;
    }

    /// Escape, resolved by `devices::escape_action`: dismiss the armed delete
    /// confirmation, else clear the device search, else close the window.
    ///
    /// The escalation matters more than the closing does — a window that
    /// closed on the first Escape would discard an armed confirmation by
    /// doing the one thing that looks like "never mind" and is not.
    pub(super) fn handle_escape(&mut self, ui: &egui::Ui) {
        if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            return;
        }
        let search_active = self.tab == Tab::Devices && !self.device_search.is_empty();
        match devices::escape_action(self.delete_state != DeleteState::Idle, search_active) {
            devices::EscapeAction::CancelDelete => self.delete_state = DeleteState::Idle,
            devices::EscapeAction::ClearSearch => self.device_search.clear(),
            devices::EscapeAction::CloseWindow => {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close)
            }
        }
    }

    /// The band under the table: the selected device's name, its tray-icon
    /// pin, and its threshold/interval overrides. Renders nothing at all when
    /// no row is selected — see `DEVICE_DETAIL_HEIGHT` for why it sits here
    /// rather than beside the table.
    fn render_device_detail(&mut self, ui: &mut egui::Ui) {
        let Some(name) = self.selected_device.clone() else {
            return;
        };
        ui.add_space(8.0);
        // Framed: an unframed block of controls under a table reads as content
        // belonging to the window rather than to the row that is selected.
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.strong(&name);
                ui.add_space(12.0);
                self.render_primary_control(ui, &name);
            });
            ui.add_space(4.0);
            ui.columns(2, |columns| {
                self.render_threshold_override(&mut columns[0], &name);
                self.render_interval_override(&mut columns[1], &name);
            });
        });
    }
}

/// Header cell text for a sortable column: the plain label, plus a direction
/// arrow when `column` is the active sort column. GNOME HIG: ascending shows
/// the arrow pointing down, descending flips it to pointing up.
fn header_label(label: &str, column: SortColumn, sort: SortState) -> String {
    if sort.column != column {
        return label.to_string();
    }
    let arrow = match sort.direction {
        devices::SortDirection::Ascending => "\u{23f7}",
        devices::SortDirection::Descending => "\u{23f6}",
    };
    format!("{label} {arrow}")
}

/// A `small_button` tinted with the theme's error colour (T36): reinforces —
/// does not replace — the two-step confirm that already marks the action as
/// destructive.
///
/// Only the armed step is tinted. A red `Delete` on every row turns the whole
/// column into a wall of warnings for an action nobody has asked for yet.
fn destructive_small_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let color = ui.visuals().error_fg_color;
    ui.add(egui::Button::new(egui::RichText::new(label).color(color)).small())
}

fn neutral_small_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(label).small())
}

/// Renders a First-seen/Last-seen cell: the relative age, with the absolute
/// UTC date as a hover tooltip (judgement call, not GNOME-sourced — see
/// T35's spec). A missing timestamp (a discovered-but-not-yet-recorded
/// device) renders as a plain dash.
fn render_seen_cell(ui: &mut egui::Ui, at: Option<i64>, now: i64, lang: Lang) {
    match at {
        Some(at) => {
            let relative = devices::relative_label(now, at, lang);
            let absolute = devices::absolute_date_label(at);
            ui.label(relative).on_hover_text(absolute);
        }
        None => {
            ui.label("—");
        }
    }
}

/// Applies a device's desired threshold/interval override to `overrides`.
///
/// A `None` field means "no override for that field"; a value equal to its
/// global default is treated the same as `None` so the config never carries a
/// redundant override. The device's entry is removed once both fields resolve
/// to "no override", keeping `device_overrides` free of empty placeholders.
fn apply_device_override(
    overrides: &mut HashMap<String, DeviceSettings>,
    name: &str,
    threshold: Option<u8>,
    interval: Option<u64>,
    default_threshold: u8,
    default_interval: u64,
) {
    let threshold = threshold.filter(|&t| t != default_threshold);
    let interval = interval.filter(|&i| i != default_interval);
    if threshold.is_none() && interval.is_none() {
        overrides.remove(name);
    } else {
        overrides.insert(
            name.to_string(),
            DeviceSettings {
                poll_interval_secs: interval,
                low_threshold: threshold,
            },
        );
    }
}

/// Toggles whether `name` is hidden: `show = true` removes it from
/// `hidden_devices` (if present), `show = false` adds it (if absent).
/// Exactly one entry changes; every other entry — including one for a device
/// that is not in the currently-discovered list — is left untouched. That is
/// the structural fix for the bug this replaces: a partial view of the
/// roster can never damage entries it does not display, because it never
/// rebuilds the list at all.
fn toggle_hidden(hidden_devices: &mut Vec<String>, name: &str, show: bool) {
    if show {
        hidden_devices.retain(|n| n != name);
    } else if !hidden_devices.iter().any(|n| n == name) {
        hidden_devices.push(name.to_string());
    }
}

/// Sets or clears the device the aggregate tray icon features.
///
/// `make_primary = true` pins `name`, replacing whatever was pinned before —
/// exactly one device can be featured, so this is a move, not an addition.
/// `false` clears the pin only if `name` is the device currently pinned:
/// unchecking the box on a device that was never primary must not silently
/// unpin a different one.
fn toggle_primary(primary_device: &mut Option<String>, name: &str, make_primary: bool) {
    if make_primary {
        *primary_device = Some(name.to_string());
    } else if primary_device.as_deref() == Some(name) {
        *primary_device = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::PollOutcome;
    use crate::egui_test::{
        assert_single_lines_without_overlap, fully_painted_text_at, painted_text_at,
    };
    use crate::settings::tests::{device, settings_app_with};
    use crate::settings::{WINDOW_DEFAULT_SIZE, WINDOW_MIN_SIZE};

    fn painted_text(contents: impl FnMut(&mut egui::Ui)) -> Vec<String> {
        painted_text_at(WINDOW_DEFAULT_SIZE, contents)
    }

    fn rows_for(names: &[&str]) -> Vec<devices::DeviceRow> {
        devices::merge_devices(
            Vec::new(),
            names
                .iter()
                .map(|n| (device(n), PollOutcome::Failed))
                .collect(),
        )
    }

    /// The regression R45 left in the shipped build: the name cell drew its
    /// selectable button, then the name into that button's rect — which was
    /// only as wide as the button's padding, so nothing was left to read.
    #[test]
    fn device_table_paints_every_device_name() {
        let mut app = settings_app_with(Config::default());
        let rows = rows_for(&["MX Anywhere 3", "NuPhy Air75"]);

        let painted = painted_text(|ui| app.render_device_table(ui, &rows, 1_700_000_000));

        for name in ["MX Anywhere 3", "NuPhy Air75"] {
            assert!(
                painted.iter().any(|t| t == name),
                "{name:?} was not painted; got {painted:?}"
            );
        }
    }

    /// The window promises a minimum size; at that size the table must still
    /// show the two columns that carry its only actions. Below roughly 940 px
    /// `Tray icon` and `Actions` fall off the right edge, and the table has no
    /// horizontal scrolling to reach them with — measured, not assumed: at
    /// 720 px (the previous minimum) both headers are gone, along with
    /// `Last seen`.
    #[test]
    fn device_table_fits_at_the_minimum_window_width() {
        let mut app = settings_app_with(Config::default());
        let rows = rows_for(&["SteelSeries Arctis Nova Pro Wireless Headset"]);

        let painted = painted_text_at(WINDOW_MIN_SIZE, |ui| {
            app.render_device_table(ui, &rows, 1_700_000_000)
        });

        for header in ["Name ⏷", "Charge", "Last seen", "Tray icon", "Actions"] {
            assert!(
                painted.iter().any(|t| t == header),
                "{header:?} is not visible at the minimum window width; got {painted:?}"
            );
        }
    }

    /// Russian runs 20–35% longer than English. Columns do not clip, so too long
    /// a label wraps onto extra lines, runs into its neighbour, or pushes a column
    /// off the window's right edge.
    #[test]
    fn device_table_text_is_not_clipped_in_any_language() {
        let mut row = rows_for(&["MX Anywhere 3"]).remove(0);
        row.store_id = Some(1);
        row.presence = Presence::Disconnected;
        row.charge = Some(crate::domain::BatteryReading::new(
            90,
            crate::domain::ChargeState::Discharging,
        ));
        row.first_seen = Some(1_700_000_000 - 3 * 86_400);
        row.last_seen = Some(1_700_000_000 - 50 * 60);
        let mut armed = row.clone();
        armed.store_id = Some(2);
        armed.device.name = "NuPhy Air75".to_string();
        let mut denied = row.clone();
        denied.store_id = Some(3);
        denied.device.name = "Aerox 5".to_string();
        denied.presence = Presence::NoAccess;
        let rows = vec![row, armed, denied];

        for lang in Lang::ALL {
            let mut app = settings_app_with(Config {
                language: Some(lang.tag().to_owned()),
                ..Config::default()
            });
            app.delete_state = DeleteState::Confirming(2);

            let painted = fully_painted_text_at(WINDOW_MIN_SIZE, |ui| {
                app.render_device_table(ui, &rows, 1_700_000_000)
            });

            let l = loader(lang);
            let mut expected = vec![format!("{} \u{23f7}", l.get("col-name"))];
            for id in [
                "col-type",
                "col-connection",
                "col-charge",
                "col-status",
                "col-first-seen",
                "col-last-seen",
                "col-tray-icon",
                "col-actions",
                "presence-disconnected",
                "presence-no-access",
                "button-delete",
                "button-confirm",
                "button-cancel",
            ] {
                expected.push(l.get(id));
            }
            expected.push(crate::domain::DeviceKind::Mouse.label(lang));
            expected.push(devices::charge_cell_text(rows[0].charge, lang));
            expected.push(devices::relative_label(
                1_700_000_000,
                1_700_000_000 - 3 * 86_400,
                lang,
            ));
            expected.push(devices::relative_label(
                1_700_000_000,
                1_700_000_000 - 50 * 60,
                lang,
            ));
            for text in expected {
                assert!(
                    painted.iter().any(|p| p.text == text),
                    "{lang:?}: {text:?} is cut off or missing; fully painted: {painted:?}"
                );
            }
            assert_single_lines_without_overlap(&painted);
        }
    }

    #[test]
    fn device_table_renders_in_russian() {
        let mut app = settings_app_with(Config {
            language: Some("ru".to_owned()),
            ..Config::default()
        });
        let rows = rows_for(&["MX Anywhere 3"]);

        let painted = painted_text_at(WINDOW_MIN_SIZE, |ui| {
            app.render_device_table(ui, &rows, 1_700_000_000)
        });

        for text in ["Название ⏷", "Заряд", "Замечено", "Действия", "мышь"]
        {
            assert!(painted.iter().any(|t| t == text), "{text:?}: {painted:?}");
        }
    }

    /// The band under the table draws nothing at all until a row is selected
    /// — not a placeholder explaining that selecting a row would fill it.
    #[test]
    fn device_detail_paints_nothing_until_a_row_is_selected() {
        let mut app = settings_app_with(Config::default());

        let painted = painted_text(|ui| app.render_device_detail(ui));

        assert!(
            painted.is_empty(),
            "expected nothing painted, got {painted:?}"
        );
    }

    #[test]
    fn device_detail_paints_the_selected_device_and_its_controls() {
        let mut app = settings_app_with(Config::default());
        app.selected_device = Some("MX Anywhere 3".to_string());

        let painted = painted_text(|ui| app.render_device_detail(ui));

        assert!(painted.iter().any(|t| t == "MX Anywhere 3"), "{painted:?}");
        assert!(
            painted.iter().any(|t| t == "Use for the single tray icon"),
            "{painted:?}"
        );
        assert!(
            painted.iter().any(|t| t.starts_with("Use default (20%")),
            "{painted:?}"
        );
    }

    #[test]
    fn apply_device_override_sets_both_fields() {
        let mut overrides = HashMap::new();
        apply_device_override(&mut overrides, "mouse", Some(10), Some(30), 20, 60);
        assert_eq!(
            overrides.get("mouse"),
            Some(&DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: Some(10),
            })
        );
    }

    #[test]
    fn apply_device_override_clearing_removes_key() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: Some(10),
            },
        );
        apply_device_override(&mut overrides, "mouse", None, None, 20, 60);
        assert!(!overrides.contains_key("mouse"));
    }

    #[test]
    fn apply_device_override_equal_to_default_removes_key() {
        let mut overrides = HashMap::new();
        apply_device_override(&mut overrides, "mouse", Some(20), Some(60), 20, 60);
        assert!(!overrides.contains_key("mouse"));
    }

    #[test]
    fn apply_device_override_partial_keeps_only_non_default_field() {
        let mut overrides = HashMap::new();
        apply_device_override(&mut overrides, "mouse", Some(15), Some(60), 20, 60);
        assert_eq!(
            overrides.get("mouse"),
            Some(&DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(15),
            })
        );
    }

    #[test]
    fn toggle_hidden_unchecking_adds_entry() {
        let mut hidden = Vec::new();
        toggle_hidden(&mut hidden, "mouse", false);
        assert_eq!(hidden, vec!["mouse".to_string()]);
    }

    #[test]
    fn toggle_primary_pins_the_named_device() {
        let mut primary = None;
        toggle_primary(&mut primary, "mouse", true);
        assert_eq!(primary, Some("mouse".to_string()));
    }

    #[test]
    fn toggle_primary_replaces_the_previous_pin() {
        let mut primary = Some("mouse".to_string());
        toggle_primary(&mut primary, "keyboard", true);
        assert_eq!(primary, Some("keyboard".to_string()));
    }

    #[test]
    fn toggle_primary_unpinning_clears_the_pin() {
        let mut primary = Some("mouse".to_string());
        toggle_primary(&mut primary, "mouse", false);
        assert_eq!(primary, None);
    }

    /// Unchecking the box on a device that was never primary must leave the
    /// device that is alone.
    #[test]
    fn toggle_primary_unpinning_another_device_is_a_no_op() {
        let mut primary = Some("mouse".to_string());
        toggle_primary(&mut primary, "keyboard", false);
        assert_eq!(primary, Some("mouse".to_string()));
    }

    #[test]
    fn toggle_hidden_checking_removes_entry() {
        let mut hidden = vec!["mouse".to_string()];
        toggle_hidden(&mut hidden, "mouse", true);
        assert!(hidden.is_empty());
    }

    #[test]
    fn toggle_hidden_leaves_unrelated_entries_untouched() {
        // Regression test for the bug this replaces: a device not in the
        // currently-discovered list ("offline-device") must survive a toggle
        // on an unrelated device. The old `shown_after_toggle` rebuilt the
        // whole list from the visible roster and silently dropped anything
        // absent from it.
        let mut hidden = vec!["offline-device".to_string()];
        toggle_hidden(&mut hidden, "mouse", false);
        assert_eq!(
            hidden,
            vec!["offline-device".to_string(), "mouse".to_string()]
        );
        toggle_hidden(&mut hidden, "mouse", true);
        assert_eq!(hidden, vec!["offline-device".to_string()]);
    }

    #[test]
    fn toggle_hidden_checking_already_shown_is_noop() {
        let mut hidden = vec!["keyboard".to_string()];
        toggle_hidden(&mut hidden, "mouse", true);
        assert_eq!(hidden, vec!["keyboard".to_string()]);
    }

    #[test]
    fn toggle_hidden_unchecking_already_hidden_is_noop() {
        let mut hidden = vec!["mouse".to_string()];
        toggle_hidden(&mut hidden, "mouse", false);
        assert_eq!(hidden, vec!["mouse".to_string()]);
    }
}
