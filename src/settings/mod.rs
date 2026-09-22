mod devices;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_extras::{Column, Size, StripBuilder, TableBuilder};

use crate::autostart;
use crate::config::{self, Config, DeviceSettings, DisplayMode, TrayMode};
use crate::domain::{BatteryReading, DeviceInfo, Presence};
use crate::i18n::{self, Lang, fl, loader};
use crate::state;
use devices::{DeleteState, DeviceRow, SortColumn, SortState};

/// Seconds the "Changes saved." status line remains visible after a save.
const SAVED_VISIBLE_SECS: u64 = 2;

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

/// The window's opening size, and the smallest the user may make it.
///
/// The minimum is not a guess: below about 940 px the Devices tab's table
/// silently drops its two right-hand columns — `Tray icon` and the `Delete`
/// action — leaving no way to reach either, because the table has no
/// horizontal scrolling and a settings window should not need any.
/// `device_table_fits_at_the_minimum_window_width` pins it to that fact, so
/// adding a column fails a test rather than shrinking the window's promise.
const WINDOW_DEFAULT_SIZE: [f32; 2] = [980.0, 620.0];
const WINDOW_MIN_SIZE: [f32; 2] = [940.0, 360.0];

/// Row and header heights for the Devices tab table.
const TABLE_ROW_HEIGHT: f32 = 22.0;
const TABLE_HEADER_HEIGHT: f32 = 24.0;

/// Width of the separator column between `Tray icon` (a checkbox that only
/// hides an icon) and `Actions` (`Delete`, which destroys the inventory
/// record): visible space plus a vertical rule so the two controls do not
/// read as one action a stray click could confuse (T36).
const TOGGLE_ACTIONS_GAP_WIDTH: f32 = 20.0;

/// Height reserved for the shared status bar (separator + "Changes saved." +
/// Close) at the bottom of the window, below both tabs.
const STATUS_BAR_HEIGHT: f32 = 40.0;

/// Global low-battery threshold range, percent. Below 5% the warning fires too
/// late to matter; above 50% it stops meaning "low".
const LOW_THRESHOLD_RANGE: std::ops::RangeInclusive<u8> = 5..=50;

/// Global poll interval range, seconds. The lower bound guards against waking
/// a HID device every second, which drains the battery it is meant to monitor.
const POLL_INTERVAL_RANGE: std::ops::RangeInclusive<u64> = 10..=3600;

/// systemd targets `systemctl --user enable` links a unit into. Checked in
/// this order but either one enabling `rigbat.service` counts: which target
/// applies depends on the unit's own `WantedBy=`, not on anything this window
/// controls.
const SYSTEMD_WANTS_TARGETS: [&str; 2] = ["default.target.wants", "graphical-session.target.wants"];

/// The unit name `make service` installs and `systemctl --user enable`
/// operates on.
const SYSTEMD_UNIT_NAME: &str = "rigbat.service";

/// Returns `true` if `rigbat.service` is enabled for the systemd user manager
/// rooted at `unit_dir` (`$XDG_CONFIG_HOME/systemd/user`, falling back to
/// `~/.config/systemd/user`) — i.e. `systemctl --user enable` linked it into
/// `default.target.wants/` or `graphical-session.target.wants/`. A pure
/// filesystem check: no systemd dependency, no shelling out.
fn systemd_service_enabled_at(unit_dir: &Path) -> bool {
    SYSTEMD_WANTS_TARGETS
        .iter()
        .any(|target| unit_dir.join(target).join(SYSTEMD_UNIT_NAME).exists())
}

/// Returns `true` if `rigbat.service` is enabled, or `false` if the config
/// directory cannot be determined (no home directory in the environment).
fn systemd_service_enabled() -> bool {
    directories::BaseDirs::new()
        .map(|b| systemd_service_enabled_at(&b.config_dir().join("systemd").join("user")))
        .unwrap_or(false)
}

/// The window's two top-level sections. Hand-rolled `SelectableLabel` tab
/// bar, not `egui_dock` (a docking system for editor layouts, not a fixed
/// two-or-three-section switcher) and not a sidebar (GNOME HIG reserves the
/// sidebar pattern for apps with many destinations or their own iconography;
/// two sections is squarely view-switcher territory). Kept open for a third
/// tab without redesigning navigation — add a variant and a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Devices,
}

/// One scan's raw result: the current discovery-and-poll pass, plus a fresh
/// read of the persisted device inventory. `SettingsApp::apply_scan_result`
/// merges the two into the Devices tab's table rows; `General`'s device list
/// only needs the discovered half.
struct ScanResult {
    discovered: Vec<(DeviceInfo, Option<BatteryReading>)>,
    records: Vec<state::DeviceRecord>,
}

struct SettingsApp {
    config: Config,
    devices: Vec<DeviceInfo>,
    /// Set to `Some(Instant::now())` on every successful save; cleared implicitly
    /// by comparing elapsed time on each frame.
    saved_at: Option<Instant>,
    /// Reflects `~/.config/autostart/rigbat.desktop` existence — not stored in Config.
    autostart_enabled: bool,
    /// Whether `rigbat.service` is enabled in the systemd user manager,
    /// checked once when the window opens (T29): a second launch path to the
    /// same `rigbat tray` that the autostart checkbox must not silently
    /// duplicate.
    systemd_service_enabled: bool,
    /// Kept alive for the life of the window and used to spawn scans; never
    /// entered blockingly from `ui()`, which runs on the main thread.
    rt: Arc<tokio::runtime::Runtime>,
    /// Shared across scans so the system-bus connection they open is memoized.
    discovery_ctx: Arc<crate::discovery::Context>,
    /// `Some` while a scan's result is outstanding; taken (and cleared) once
    /// `try_recv` yields something.
    scan_rx: Option<mpsc::Receiver<ScanResult>>,
    /// True from the moment a scan is spawned until its result is applied.
    scanning: bool,
    tab: Tab,
    /// `None` if the state store failed to open (see `state::open`) — the
    /// Devices tab then shows only what the current scan finds, same as
    /// this window behaved before the inventory existed.
    store: Option<Arc<dyn state::Store>>,
    /// The Devices tab's backing list: every inventory record merged with
    /// the last scan. Search and sort are applied to a copy of this on
    /// render, never in place — `device_rows` itself always holds the full,
    /// unfiltered set.
    device_rows: Vec<DeviceRow>,
    device_search: String,
    device_sort: SortState,
    /// Device name the override detail panel is showing. Name, not
    /// `DeviceId`: `device_overrides` is keyed by name (see
    /// `apply_device_override`), and a name survives the selected device
    /// dropping out of the current scan.
    selected_device: Option<String>,
    delete_state: DeleteState,
}

impl SettingsApp {
    /// Renders one section header: bold label followed by a small gap.
    fn section_header(ui: &mut egui::Ui, title: &str) {
        ui.strong(title);
        ui.add_space(4.0);
    }

    /// Saves one user edit and flashes the "Changes saved." status for
    /// `SAVED_VISIBLE_SECS`. Logs on failure; the status line stays unchanged.
    ///
    /// `edit` names exactly the field the call site just changed — see
    /// `save_edit` for why. On success, `self.config` adopts the freshly
    /// saved config, so any field changed on disk by another process since
    /// this window opened is picked up too, not just the one this call
    /// touched.
    fn persist(&mut self, ui: &egui::Ui, edit: impl FnOnce(&mut Config)) {
        match save_edit(&config::load, &config::save, edit) {
            Ok(on_disk) => {
                self.config = on_disk;
                self.saved_at = Some(Instant::now());
                ui.ctx()
                    .request_repaint_after(Duration::from_secs(SAVED_VISIBLE_SECS));
            }
            Err(e) => tracing::error!("failed to save config: {e}"),
        }
    }

    /// Spawns one discovery-and-poll pass on `rt` if none is already in
    /// flight, plus a fresh read of the device inventory, wiring the result
    /// to a fresh channel and waking `egui_ctx` when it lands so the window
    /// updates without waiting for the next input event.
    ///
    /// Polls every discovered device (unlike the pre-T35 discovery-only
    /// scan) because the Devices tab needs each one's charge; this only
    /// runs on an explicit Refresh click or window open, not on a timer, so
    /// the extra device wake-up this costs is the same one-off the user just
    /// asked for, not the continuous drain `POLL_INTERVAL_RANGE` guards
    /// against. The inventory read is a direct, synchronous `Store` call
    /// made from inside this spawned task, never on the UI thread — the
    /// same "brief, blocking, from an async context" pattern
    /// `app::supervisor` already uses for `record_seen`/`record_reading`.
    fn spawn_scan(&mut self, egui_ctx: egui::Context) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        let ctx = Arc::clone(&self.discovery_ctx);
        let store = self.store.clone();
        self.rt.spawn(async move {
            let sweeps = crate::discovery::discover_all(&ctx).await;
            let sources = crate::discovery::flatten(sweeps);
            let discovered = crate::app::poll_once(sources).await;
            let records = match &store {
                Some(store) => store.list_devices().unwrap_or_else(|e| {
                    tracing::warn!("failed to read device inventory: {e:#}");
                    Vec::new()
                }),
                None => Vec::new(),
            };
            // The receiver is dropped if the window closed mid-scan; ignore that.
            let _ = tx.send(ScanResult {
                discovered,
                records,
            });
            egui_ctx.request_repaint();
        });
    }

    /// Drains a completed scan's result, if any, without blocking. Safe to
    /// call every frame.
    fn poll_scan(&mut self) {
        let Some(rx) = self.scan_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => self.apply_scan_result(result),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.scanning = false;
                self.scan_rx = None;
            }
        }
    }

    /// Applies a freshly completed scan's result. `self.devices` (the
    /// General tab's picker) and `self.device_rows` (the Devices tab's
    /// table) are both derived fresh; `hidden_devices` and
    /// `device_overrides` are keyed by device name and are left exactly as
    /// the user set them, whether or not the device set changed since the
    /// previous scan.
    fn apply_scan_result(&mut self, result: ScanResult) {
        self.devices = result
            .discovered
            .iter()
            .map(|(info, _)| info.clone())
            .collect();
        self.device_rows = devices::merge_devices(result.records, result.discovered);
        self.scanning = false;
        self.scan_rx = None;
    }

    /// Renders a "Refresh" button, disabled and relabeled while a scan is
    /// already in flight.
    ///
    /// Named the same as the tray menu's "Refresh" item (T36): the tray's
    /// item re-triggers a running daemon's discovery-and-poll, this one runs
    /// a one-shot discovery-and-poll in the settings process, but both mean
    /// "go look again now" from the user's side, and nothing about the
    /// difference is visible in this window — so it gets the same name.
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
                self.spawn_scan(egui_ctx.clone());
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
            self.persist(ui, move |target| {
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
            self.save_threshold_override(ui, name, on.then_some(value));
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
            self.save_interval_override(ui, name, on.then_some(value));
        }
    }

    /// Writes one device's threshold override, leaving its interval override
    /// as the on-disk config has it — the two controls are rendered
    /// separately, so neither may write the other's field from a snapshot.
    fn save_threshold_override(&mut self, ui: &egui::Ui, name: &str, threshold: Option<u8>) {
        let name = name.to_string();
        self.persist(ui, move |target| {
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
    fn save_interval_override(&mut self, ui: &egui::Ui, name: &str, interval: Option<u64>) {
        let name = name.to_string();
        self.persist(ui, move |target| {
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
    fn render_devices_tab(&mut self, ui: &mut egui::Ui) {
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
                self.persist(ui, move |target| {
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
                        self.delete_device(ui, store_id, &row.device.name);
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
    /// somewhere else and seen again. Store I/O happens directly on the UI thread,
    /// the same as `persist`'s `config::save`/`load`: a deliberate,
    /// infrequent, user-confirmed click, not the per-frame inventory read
    /// `spawn_scan` keeps off the UI thread.
    fn delete_device(&mut self, ui: &egui::Ui, store_id: i64, name: &str) {
        if let Some(store) = &self.store
            && let Err(e) = store.delete_device(store_id)
        {
            tracing::error!("failed to delete device {name:?} from inventory: {e}");
            self.delete_state = DeleteState::Idle;
            return;
        }

        let forgotten = name.to_string();
        self.persist(ui, move |target| {
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
    fn handle_escape(&mut self, ui: &egui::Ui) {
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

impl eframe::App for SettingsApp {
    /// Called each frame; `ui` is the root central panel provided by eframe 0.34.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_scan();
        self.handle_escape(ui);

        // Apply a 16 px inner margin on all sides per the design system. We
        // replace the default CentralPanel frame with one that only changes
        // inner_margin, keeping all other visual properties from the theme.
        let frame = egui::Frame::central_panel(ui.style()).inner_margin(16.0);
        frame.show(ui, |ui| {
            self.render_tab_bar(ui);
            ui.add_space(8.0);
            // A vertical strip, not plain top-to-bottom layout: `egui_extras`'s
            // table (and the strip that lays it out beside the detail panel)
            // claims all remaining height for itself — "if you want something
            // below the table, put it in a strip" (egui_extras::table's own
            // module doc). Reserving the status bar's row up front is what
            // leaves it anything to render into on the Devices tab.
            StripBuilder::new(ui)
                .size(Size::remainder())
                .size(Size::exact(STATUS_BAR_HEIGHT))
                .vertical(|mut strip| {
                    strip.cell(|ui| match self.tab {
                        Tab::General => {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                self.render_general_tab(ui);
                            });
                        }
                        Tab::Devices => self.render_devices_tab(ui),
                    });
                    strip.cell(|ui| self.render_status_bar(ui));
                });
        });
    }
}

impl SettingsApp {
    /// Hand-rolled tab bar: `SelectableLabel`s bound to `Tab`, not
    /// `egui_dock` (see `Tab`'s doc comment for why).
    fn render_tab_bar(&mut self, ui: &mut egui::Ui) {
        let l = loader(self.config.lang());
        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::General, fl!(l, "tab-general")),
                (Tab::Devices, fl!(l, "tab-devices")),
            ] {
                if ui.selectable_label(self.tab == tab, label).clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.separator();
    }

    fn render_general_tab(&mut self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let l = loader(lang);
        // ── Tray display ──────────────────────────────────────────────────────
        Self::section_header(ui, &fl!(l, "section-tray-display"));
        // Bound to a local copy, not to `self.config`: `persist` adopts the
        // saved config only when the write succeeds, so a failed save leaves
        // `self.config` as it was and the next frame redraws the real value.
        // Writing through `&mut self.config` instead left the window showing a
        // setting that is not on disk, with nothing to correct it.
        let mut display_mode = self.config.display_mode;
        for mode in DisplayMode::ALL {
            if ui
                .radio_value(&mut display_mode, mode, mode.label(lang))
                .changed()
            {
                // Persist immediately; the tray watches the file and re-renders.
                self.persist(ui, move |target| target.display_mode = mode);
            }
        }

        ui.add_space(8.0);
        let mut per_device = self.config.tray_mode == TrayMode::PerDevice;
        if ui
            .checkbox(&mut per_device, fl!(l, "tray-per-device"))
            .changed()
        {
            let tray_mode = if per_device {
                TrayMode::PerDevice
            } else {
                TrayMode::PrimaryOnly
            };
            self.persist(ui, move |target| target.tray_mode = tray_mode);
        }

        let hint = if per_device {
            fl!(l, "tray-per-device-hint")
        } else {
            aggregate_icon_hint(self.config.primary_device.as_deref(), lang)
        };
        // The pin is set from a device's row, so a pin naming a device the
        // Devices tab has no row for — one retired before the inventory
        // existed, or deleted since — would be unreachable without this.
        // Clearing is the only action that needs no row, which is why it is
        // the only one that lives here.
        let pinned = !per_device && self.config.primary_device.is_some();
        let mut clear_pin = false;
        ui.indent("tray_device_picker", |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(hint).weak());
                if pinned {
                    clear_pin = ui.small_button(fl!(l, "button-clear")).clicked();
                }
            });
        });
        if clear_pin {
            self.persist(ui, |target| target.primary_device = None);
        }

        // ── Battery ───────────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, &fl!(l, "section-defaults"));

        let mut threshold = self.config.low_threshold;
        let resp = ui.add(
            egui::Slider::new(&mut threshold, LOW_THRESHOLD_RANGE)
                .text(fl!(l, "default-low-threshold"))
                .suffix("%"),
        );
        if resp.drag_stopped() || resp.lost_focus() {
            self.persist(ui, move |target| target.low_threshold = threshold);
        }

        let mut interval = self.config.poll_interval_secs;
        let resp = ui
            .add(
                egui::Slider::new(&mut interval, POLL_INTERVAL_RANGE)
                    .text(fl!(l, "default-poll-interval"))
                    .suffix(fl!(l, "unit-seconds-suffix")),
            )
            .on_hover_text(fl!(l, "poll-interval-hint"));
        if resp.drag_stopped() || resp.lost_focus() {
            self.persist(ui, move |target| target.poll_interval_secs = interval);
        }
        ui.label(egui::RichText::new(fl!(l, "defaults-hint")).weak());

        // ── Notifications ─────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, &fl!(l, "section-notifications"));
        // A local copy for the same reason as `display_mode` above.
        let mut notifications_enabled = self.config.notifications_enabled;
        if ui
            .checkbox(&mut notifications_enabled, fl!(l, "notifications-enabled"))
            .changed()
        {
            self.persist(ui, move |target| {
                target.notifications_enabled = notifications_enabled;
            });
        }

        // ── Startup ───────────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, &fl!(l, "section-startup"));
        if self.systemd_service_enabled {
            ui.add_enabled_ui(false, |ui| {
                ui.checkbox(&mut self.autostart_enabled, fl!(l, "autostart-enabled"));
            });
            ui.label(egui::RichText::new(fl!(l, "autostart-managed-by-systemd")).weak());
        } else if ui
            .checkbox(&mut self.autostart_enabled, fl!(l, "autostart-enabled"))
            .changed()
        {
            match autostart::set_enabled(self.autostart_enabled) {
                Ok(()) => {
                    self.saved_at = Some(Instant::now());
                    ui.ctx()
                        .request_repaint_after(Duration::from_secs(SAVED_VISIBLE_SECS));
                }
                Err(e) => {
                    tracing::warn!("failed to update autostart: {e}");
                    // Revert the checkbox so it reflects the real filesystem state.
                    self.autostart_enabled = !self.autostart_enabled;
                }
            }
        }

        // ── Language ──────────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, &fl!(l, "section-language"));
        let mut choice = self.config.language.as_deref().and_then(Lang::from_tag);
        let system = format!(
            "{} ({})",
            fl!(l, "language-system"),
            i18n::system().native_name()
        );
        let selected = choice.map_or_else(|| system.clone(), |lang| lang.native_name().to_owned());
        let mut changed = false;
        egui::ComboBox::from_id_salt("language")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                changed |= ui
                    .selectable_value(&mut choice, None, system.as_str())
                    .changed();
                for lang in Lang::ALL {
                    changed |= ui
                        .selectable_value(&mut choice, Some(lang), lang.native_name())
                        .changed();
                }
            });
        if changed {
            self.persist(ui, move |target| {
                target.language = choice.map(|lang| lang.tag().to_owned());
            });
        }

        // ── About ─────────────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, &fl!(l, "section-about"));
        ui.label(format!("rigbat {}", env!("CARGO_PKG_VERSION")));
        ui.hyperlink_to(fl!(l, "about-project-page"), env!("CARGO_PKG_REPOSITORY"));
    }

    /// Status line + Close button, shared by both tabs (not just General's
    /// scroll area): the Devices tab's override panel saves too, and Close
    /// must stay reachable regardless of which tab is open.
    fn render_status_bar(&mut self, ui: &mut egui::Ui) {
        let l = loader(self.config.lang());
        ui.add_space(8.0);
        ui.separator();

        ui.horizontal(|ui| {
            // Show "Changes saved." for SAVED_VISIBLE_SECS after the last save.
            let status = match self.saved_at {
                Some(t) if t.elapsed() < Duration::from_secs(SAVED_VISIBLE_SECS) => {
                    fl!(l, "status-saved")
                }
                _ => String::new(),
            };
            ui.weak(status);

            // Push the Close button to the right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(fl!(l, "button-close")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
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

/// What the General tab says the single tray icon will show. Names the pinned
/// device when there is one, so the user can see the setting's current value
/// without opening the tab that owns the control.
fn aggregate_icon_hint(primary_device: Option<&str>, lang: Lang) -> String {
    let l = loader(lang);
    match primary_device {
        Some(name) => fl!(l, "tray-primary-hint-pinned", name = name),
        None => fl!(l, "tray-primary-hint-auto"),
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

/// Re-reads the on-disk config via `load_config`, applies `edit` — the one
/// change a call site just made — to that fresh copy, and writes the result
/// back via `save_config`. Returns the saved config on success.
///
/// Replaces the earlier `apply_to`, which copied every window-owned field
/// from the window's in-memory snapshot regardless of whether the user had
/// touched it in this window session. That blanket overwrite is what let the
/// tray's one-time `shown_devices` → `hidden_devices` migration be silently
/// undone: a window opened before the migration ran held `hidden_devices`
/// empty, and its next save — for any field, even an unrelated one — wrote
/// that stale empty list back over the migrated value, permanently, since
/// the migration cannot re-run once `shown_devices` is already empty. `edit`
/// makes that impossible by construction: every field other than the one it
/// names always comes from `load_config`, never from a window snapshot.
///
/// `load_config`/`save_config` are parameters, not `config::load`/`save`
/// called directly, so tests can point this at a temporary file instead of
/// the real `config::config_path()` — mirrors
/// `app::supervisor::migrate_shown_devices_once`.
fn save_edit<L, S>(
    load_config: &L,
    save_config: &S,
    edit: impl FnOnce(&mut Config),
) -> anyhow::Result<Config>
where
    L: Fn() -> Config,
    S: Fn(&Config) -> anyhow::Result<()>,
{
    let mut on_disk = load_config();
    edit(&mut on_disk);
    save_config(&on_disk)?;
    Ok(on_disk)
}

/// Opens the settings window. Blocks until the user closes it.
///
/// Keeps a tokio runtime alive for the life of the window (unlike the old
/// discover-once-and-drop approach) so devices that connect after the window
/// opens still show up: a scan is spawned on it at startup and again on
/// every "Refresh" click, never entered blockingly from `ui()`.
pub fn run() -> anyhow::Result<()> {
    use anyhow::Context as _;

    let config = config::load();
    // A second connection to the same database the tray writes through —
    // safe since the schema migration takes BEGIN IMMEDIATE plus
    // CREATE TABLE IF NOT EXISTS. `None` (no state directory, a corrupt
    // file) degrades to today's scan-only device list, same as the tray
    // treats a missing store as an optimisation, never a dependency.
    let store = state::open();
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building the tokio runtime for device discovery")?,
    );
    let discovery_ctx = Arc::new(crate::discovery::Context::new());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Sizes live next to the table they are derived from — see
            // `WINDOW_MIN_SIZE`.
            .with_inner_size(WINDOW_DEFAULT_SIZE)
            .with_min_inner_size(WINDOW_MIN_SIZE)
            .with_title("rigbat")
            .with_app_id("rigbat"),
        ..Default::default()
    };
    eframe::run_native(
        "rigbat",
        options,
        Box::new(move |cc| {
            // Follow the system light/dark preference. Confirmed safe in the runtime-less settings
            // process on COSMIC/Wayland — portal theme queries do not hit the zbus-no-runtime path.
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            let mut app = SettingsApp {
                config,
                devices: Vec::new(),
                saved_at: None,
                autostart_enabled: autostart::is_enabled(),
                systemd_service_enabled: systemd_service_enabled(),
                rt,
                discovery_ctx,
                scan_rx: None,
                scanning: false,
                tab: Tab::General,
                store,
                device_rows: Vec::new(),
                device_search: String::new(),
                device_sort: SortState::default(),
                selected_device: None,
                delete_state: DeleteState::default(),
            };
            app.spawn_scan(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique scratch directory under the OS temp dir for one test. Removed
    /// at the end of the test regardless of outcome.
    fn scratch_dir(test_name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rigbat-settings-test-{test_name}-{}-{n}",
            std::process::id()
        ))
    }

    #[test]
    fn systemd_service_enabled_at_false_when_dir_absent() {
        let dir = scratch_dir("dir-absent");
        assert!(!systemd_service_enabled_at(&dir));
    }

    #[test]
    fn systemd_service_enabled_at_false_when_no_symlink() {
        let dir = scratch_dir("no-symlink");
        std::fs::create_dir_all(dir.join("default.target.wants")).unwrap();
        assert!(!systemd_service_enabled_at(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn systemd_service_enabled_at_true_for_default_target_wants() {
        let dir = scratch_dir("default-target");
        let wants = dir.join("default.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        std::fs::write(wants.join("rigbat.service"), "").unwrap();
        assert!(systemd_service_enabled_at(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn systemd_service_enabled_at_true_for_graphical_session_target_wants() {
        let dir = scratch_dir("graphical-session-target");
        let wants = dir.join("graphical-session.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        std::fs::write(wants.join("rigbat.service"), "").unwrap();
        assert!(systemd_service_enabled_at(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn systemd_service_enabled_at_ignores_other_unit_names() {
        let dir = scratch_dir("other-unit");
        let wants = dir.join("default.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        std::fs::write(wants.join("other.service"), "").unwrap();
        assert!(!systemd_service_enabled_at(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The narrowest a string may be drawn and still count as readable. A
    /// couple of characters' worth: enough to fail a label clipped down to its
    /// container's padding, not so much that a short cell value fails.
    const MIN_READABLE_WIDTH: f32 = 24.0;

    /// Runs two frames of `contents` in a headless egui context and returns
    /// every string it painted *visibly* — text whose galley survives its own
    /// clip rectangle by at least `MIN_READABLE_WIDTH`.
    ///
    /// Two frames because `egui_extras`' table sizes itself from the previous
    /// frame's state, so the first one can lay out degenerately.
    ///
    /// Clipping is the whole point. R45 painted the device name with its full
    /// text, at a position inside a rectangle a few pixels wide, so a test
    /// that only collected galley strings passed against the broken build —
    /// verified by reintroducing the bug and watching it stay green. What the
    /// user saw was the clip.
    fn painted_text(contents: impl FnMut(&mut egui::Ui)) -> Vec<String> {
        painted_text_at(WINDOW_DEFAULT_SIZE, contents)
    }

    /// `painted_text` at a chosen window size. The root clip rectangle is the
    /// screen, so anything laid out past the window's edge is filtered out by
    /// the same rule that filters anything clipped inside it.
    fn painted_text_at(size: [f32; 2], contents: impl FnMut(&mut egui::Ui)) -> Vec<String> {
        text_at(size, contents, |_drawn, visible| {
            visible.width() >= MIN_READABLE_WIDTH && visible.height() > 0.0
        })
        .into_iter()
        .map(|(text, _)| text)
        .collect()
    }

    /// Strings drawn in full, not cut by their clip rectangle, with where they landed.
    fn fully_painted_text_at(
        size: [f32; 2],
        contents: impl FnMut(&mut egui::Ui),
    ) -> Vec<(String, egui::Rect)> {
        text_at(size, contents, |drawn, visible| {
            visible.width() + 0.5 >= drawn.width() && visible.height() + 0.5 >= drawn.height()
        })
    }

    fn text_at(
        size: [f32; 2],
        mut contents: impl FnMut(&mut egui::Ui),
        keep: impl Fn(egui::Rect, egui::Rect) -> bool,
    ) -> Vec<(String, egui::Rect)> {
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0], size[1]),
            )),
            ..Default::default()
        };
        let mut painted = Vec::new();
        for _ in 0..2 {
            let output = ctx.run_ui(input(), |ui| contents(ui));
            painted.clear();
            for clipped in output.shapes {
                collect_text(&clipped.shape, clipped.clip_rect, &keep, &mut painted);
            }
        }
        painted
    }

    fn collect_text(
        shape: &egui::Shape,
        clip: egui::Rect,
        keep: &impl Fn(egui::Rect, egui::Rect) -> bool,
        out: &mut Vec<(String, egui::Rect)>,
    ) {
        match shape {
            egui::Shape::Text(text) => {
                let drawn = egui::Rect::from_min_size(text.pos, text.galley.size());
                if keep(drawn, drawn.intersect(clip)) {
                    out.push((text.galley.text().to_owned(), drawn));
                }
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, clip, keep, out);
                }
            }
            _ => {}
        }
    }

    fn rows_for(names: &[&str]) -> Vec<devices::DeviceRow> {
        devices::merge_devices(
            Vec::new(),
            names.iter().map(|n| (device(n), None)).collect(),
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
        row.charge = Some(BatteryReading::new(
            90,
            crate::domain::ChargeState::Discharging,
        ));
        row.first_seen = Some(1_700_000_000 - 3 * 86_400);
        row.last_seen = Some(1_700_000_000 - 50 * 60);
        let mut armed = row.clone();
        armed.store_id = Some(2);
        armed.device.name = "NuPhy Air75".to_string();
        let rows = vec![row, armed];

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
                    painted.iter().any(|(t, _)| *t == text),
                    "{lang:?}: {text:?} is cut off or missing; fully painted: {painted:?}"
                );
            }
            let labels: Vec<_> = painted.iter().filter(|(t, _)| !t.is_empty()).collect();
            for (text, rect) in &labels {
                assert!(
                    rect.height() <= TABLE_ROW_HEIGHT,
                    "{lang:?}: {text:?} wraps onto a second line"
                );
            }
            for (i, (a, a_rect)) in labels.iter().enumerate() {
                for (b, b_rect) in &labels[i + 1..] {
                    let overlap = a_rect.intersect(*b_rect);
                    assert!(
                        overlap.width() <= 0.5 || overlap.height() <= 0.5,
                        "{lang:?}: {a:?} overlaps {b:?}"
                    );
                }
            }
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

    #[test]
    fn general_tab_offers_every_language() {
        let mut app = settings_app_with(Config {
            language: Some("ru".to_owned()),
            ..Config::default()
        });

        let painted = painted_text(|ui| app.render_general_tab(ui));

        assert!(
            painted.iter().any(|t| t == "Язык / Language"),
            "{painted:?}"
        );
        assert!(painted.iter().any(|t| t == "Русский"), "{painted:?}");
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

    fn settings_app_with(mut config: Config) -> SettingsApp {
        // Tests assert English text unless they pick a language; LANG must not decide.
        config.language.get_or_insert_with(|| "en".to_owned());
        SettingsApp {
            config,
            devices: Vec::new(),
            saved_at: None,
            autostart_enabled: false,
            systemd_service_enabled: false,
            rt: Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("building test tokio runtime"),
            ),
            discovery_ctx: Arc::new(crate::discovery::Context::new()),
            scan_rx: None,
            scanning: false,
            tab: Tab::General,
            store: None,
            device_rows: Vec::new(),
            device_search: String::new(),
            device_sort: SortState::default(),
            selected_device: None,
            delete_state: DeleteState::default(),
        }
    }

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            kind: crate::domain::DeviceKind::Mouse,
            transport: crate::domain::Transport::Hidraw,
            locator: None,
        }
    }

    /// Wraps a plain device list into a `ScanResult` with no reading and no
    /// inventory records — what `apply_scan_result`'s pre-T35 tests exercised
    /// before it started merging in the store's half.
    fn scan_result(devices: Vec<DeviceInfo>) -> ScanResult {
        ScanResult {
            discovered: devices.into_iter().map(|d| (d, None)).collect(),
            records: Vec::new(),
        }
    }

    /// Unique scratch config file path under the OS temp dir for one test.
    /// Never the real `config::config_path()` — `save_edit` tests must not
    /// touch `~/.config/rigbat/config.json`.
    fn scratch_config_path(test_name: &str) -> PathBuf {
        scratch_dir(test_name).join("config.json")
    }

    #[test]
    fn save_edit_writes_only_the_edited_field() {
        let path = scratch_config_path("writes-only-edited-field");
        config::save_to(&path, &Config::default()).unwrap();

        let load = || config::load_from(&path);
        let save = |cfg: &Config| config::save_to(&path, cfg);
        let result = save_edit(&load, &save, |target| target.low_threshold = 15).unwrap();

        assert_eq!(result.low_threshold, 15);
        assert_eq!(
            result.poll_interval_secs,
            Config::default().poll_interval_secs
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A field `edit` does not name — here `primary_device`, which no UI
    /// control ever sets — must survive untouched. Replaces the old
    /// `apply_to_preserves_primary_device`, which tested the same guarantee
    /// against the blanket-overwrite `apply_to` this function replaces.
    #[test]
    fn save_edit_preserves_fields_it_does_not_touch() {
        let path = scratch_config_path("preserves-untouched-fields");
        config::save_to(
            &path,
            &Config {
                primary_device: Some("mouse".to_string()),
                ..Config::default()
            },
        )
        .unwrap();

        let load = || config::load_from(&path);
        let save = |cfg: &Config| config::save_to(&path, cfg);
        let result = save_edit(&load, &save, |target| target.low_threshold = 10).unwrap();

        assert_eq!(result.primary_device, Some("mouse".to_string()));
        assert_eq!(result.low_threshold, 10);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Regression test for C1: a settings window's own `self.config` snapshot
    /// is never fed to `save_edit` — only `load_config`'s fresh read is. This
    /// reproduces the failing sequence directly: the window "opens" (saved
    /// once with `hidden_devices` empty), something else — the tray's
    /// one-time `shown_devices` migration, in this test's role — writes
    /// `hidden_devices` afterward, and the window then saves an unrelated
    /// field. The migrated value must survive.
    #[test]
    fn save_edit_stale_window_snapshot_does_not_clobber_concurrent_disk_write() {
        let path = scratch_config_path("stale-snapshot");
        config::save_to(&path, &Config::default()).unwrap();

        // The window would have opened here, holding hidden_devices == [].
        // It is never consulted below — only load_config is.

        config::save_to(
            &path,
            &Config {
                hidden_devices: vec!["keyboard".to_string()],
                ..Config::default()
            },
        )
        .unwrap();

        let load = || config::load_from(&path);
        let save = |cfg: &Config| config::save_to(&path, cfg);
        let result =
            save_edit(&load, &save, |target| target.notifications_enabled = false).unwrap();

        assert_eq!(result.hidden_devices, vec!["keyboard".to_string()]);
        assert!(!result.notifications_enabled);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
    fn aggregate_icon_hint_names_the_pinned_device() {
        assert!(aggregate_icon_hint(Some("MX Anywhere 3"), Lang::En).contains("MX Anywhere 3"));
    }

    #[test]
    fn aggregate_icon_hint_describes_the_automatic_choice() {
        assert!(aggregate_icon_hint(None, Lang::En).contains("first connected"));
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

    #[test]
    fn apply_scan_result_preserves_hidden_devices_and_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: Some(10),
            },
        );
        let mut app = settings_app_with(Config {
            hidden_devices: vec!["mouse".to_string()],
            device_overrides: overrides.clone(),
            ..Config::default()
        });
        app.apply_scan_result(scan_result(vec![device("mouse"), device("keyboard")]));
        assert_eq!(app.config.hidden_devices, vec!["mouse".to_string()]);
        assert_eq!(app.config.device_overrides, overrides);
        assert!(!app.scanning);
        assert!(app.scan_rx.is_none());
    }

    #[test]
    fn apply_scan_result_empty_hidden_devices_survives_device_set_change() {
        // hidden_devices == [] is "show all"; it must not flip to a concrete
        // list just because the discovered set changed.
        let mut app = settings_app_with(Config::default());
        app.apply_scan_result(scan_result(vec![device("mouse")]));
        assert!(app.config.hidden_devices.is_empty());
        app.apply_scan_result(scan_result(vec![device("mouse"), device("keyboard")]));
        assert!(app.config.hidden_devices.is_empty());
    }

    #[test]
    fn apply_scan_result_keeps_override_for_device_that_disappeared() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "headset".to_string(),
            DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(5),
            },
        );
        let mut app = settings_app_with(Config {
            device_overrides: overrides.clone(),
            ..Config::default()
        });
        // "headset" is not in this scan's result.
        app.apply_scan_result(scan_result(vec![device("mouse")]));
        assert_eq!(app.config.device_overrides, overrides);
        assert!(!app.devices.iter().any(|d| d.name == "headset"));
    }
}
