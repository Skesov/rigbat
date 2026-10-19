use std::time::Duration;

use eframe::egui;

use super::devices::{self, DeleteCell, DeleteState, DeviceRow, EscapeAction, OpenThings};
use super::general_tab::{interval_combo, interval_label, threshold_slider};
use super::{ActionProblem, SettingsApp, Tab, open_uri, scan, widgets};
use crate::config::{Config, DeviceSettings};
use crate::domain::{
    DeviceId, INSTALL_UDEV_RULE, Presence, PrimaryStatus, TrayMode, charge_value, classify,
    status_note,
};
use crate::gui;
use crate::i18n::{Lang, fl, loader};
use crate::state;

/// Above this many devices the tab offers a search field.
const SEARCH_MIN_DEVICES: usize = 8;
const SEARCH_WIDTH: f32 = 220.0;
const RESET: &str = "\u{21BA}";
/// The README section on udev access.
const PERMISSIONS_DOCS: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "#permissions");

/// What the Remove row's buttons asked for this frame.
enum RemoveStep {
    Arm,
    Confirm,
    Cancel,
}

impl SettingsApp {
    fn render_refresh_button(&mut self, ui: &mut egui::Ui) {
        let egui_ctx = ui.ctx().clone();
        let l = loader(self.config.lang());
        let progress = self
            .scanning
            .is_some_and(|since| gui::shows_progress(ui.ctx(), since));
        ui.add_enabled_ui(self.scanning.is_none(), |ui| {
            let label = if progress {
                fl!(l, "button-refreshing")
            } else {
                fl!(l, "button-refresh")
            };
            if ui.button(label).clicked() {
                self.spawn_scan(egui_ctx.clone(), scan::Scan::Refresh);
            }
        });
    }

    pub(super) fn render_devices_tab(&mut self, ui: &mut egui::Ui) {
        widgets::page(ui, "devices-tab", |ui| self.render_device_groups(ui));
    }

    fn render_device_groups(&mut self, ui: &mut egui::Ui) {
        let l = loader(self.config.lang());
        let searchable = self.device_rows.len() > SEARCH_MIN_DEVICES;
        ui.horizontal(|ui| {
            if searchable {
                ui.add(
                    egui::TextEdit::singleline(&mut self.device_search)
                        .hint_text(fl!(l, "device-search-hint"))
                        .desired_width(SEARCH_WIDTH),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.render_refresh_button(ui);
            });
        });
        if self.tray_unanswered {
            let warn = gui::status_colors_for(ui, self.config.palette).warn;
            let text = fl!(l, "devices-tray-unanswered");
            let label = ui.label(egui::RichText::new(&text).color(warn));
            gui::live_region(ui.ctx(), label.id, &text);
        }
        ui.add_space(widgets::TOOLBAR_GAP);
        self.render_no_access(ui);

        let query = if searchable {
            self.device_search.as_str()
        } else {
            ""
        };
        let mut rows = devices::filter_rows(&self.device_rows, query);
        if rows.is_empty() {
            let message = if self.device_rows.is_empty() {
                fl!(l, "devices-empty")
            } else {
                fl!(l, "devices-no-match")
            };
            ui.label(widgets::secondary(ui, &message));
            return;
        }
        devices::order_rows(&mut rows);
        let (connected, seen): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|row| row.presence != Presence::Disconnected);
        let visuals = ui.visuals().clone();
        let colors = gui::status_colors_for(ui, self.config.palette);
        let now = state::now_unix();
        for (title, group) in [
            (fl!(l, "devices-connected"), connected),
            (fl!(l, "devices-seen-before"), seen),
        ] {
            if group.is_empty() {
                continue;
            }
            widgets::group(ui, &title, None, |rows| {
                for row in &group {
                    self.render_device(rows, row, (&visuals, &colors), now);
                }
            });
        }
    }

    /// What a device without access lacks and the command that grants it.
    fn render_no_access(&self, ui: &mut egui::Ui) {
        let count = self
            .device_rows
            .iter()
            .filter(|row| row.presence == Presence::NoAccess)
            .count();
        if count == 0 {
            return;
        }
        let l = loader(self.config.lang());
        let warn = gui::status_colors_for(ui, self.config.palette).warn;
        let title = fl!(l, "devices-no-access-title", count = count);
        let (copy, copied) = (fl!(l, "button-copy"), fl!(l, "button-copied"));
        let mut help = false;
        widgets::banner(ui, &title, warn, |ui| {
            ui.label(fl!(l, "devices-no-access-body"));
            widgets::command(ui, INSTALL_UDEV_RULE, &copy, &copied);
            help = ui.link(fl!(l, "devices-no-access-help")).clicked();
        });
        if help {
            open_uri::open(&self.rt, &self.open_uri, ui.ctx(), PERMISSIONS_DOCS);
        }
    }

    /// One device: the expander line, and its settings while expanded.
    fn render_device(
        &mut self,
        rows: &mut widgets::Rows<'_>,
        row: &DeviceRow,
        look: (&egui::Visuals, &gui::StatusColors),
        now: i64,
    ) {
        let lang = self.config.lang();
        let l = loader(lang);
        let name = row.device.name.as_str();
        let connected = row.presence != Presence::Disconnected;
        let (value, note) = self.device_value(row, look, now);
        let hover = device_about(row, lang);
        let header = widgets::Expander {
            id: egui::Id::new(("device-row", &row.device)),
            glyph: gui::kind_glyph(row.kind),
            title: name,
            note: note.as_deref(),
            value,
            hover: &hover,
            expanded: self.expanded_device.as_ref() == Some(&row.device),
        };
        let label = fl!(l, "device-show-in-tray");
        let (response, shown) = rows.expander(header, |ui| {
            let mut shown = self.config.is_shown(name);
            (connected
                && widgets::switch(
                    ui,
                    device_switch_id("shown", &row.device),
                    &mut shown,
                    &label,
                )
                .changed())
            .then_some(shown)
        });
        if let Some(shown) = shown {
            let name = name.to_owned();
            self.persist(move |target| set_shown(&mut target.hidden_devices, &name, shown));
        }
        if response.clicked() {
            self.toggle_expanded(&row.device);
        }
        if self.expanded_device.as_ref() == Some(&row.device) {
            rows.nested(|rows| self.render_device_settings(rows, row));
        }
    }

    /// The collapsed row's value and note: the dashboard's words for a
    /// connected device, when it was last seen for any other. The estimate and
    /// the reading's age exist only when a running tray was read.
    fn device_value(
        &self,
        row: &DeviceRow,
        (visuals, colors): (&egui::Visuals, &gui::StatusColors),
        now: i64,
    ) -> (egui::RichText, Option<String>) {
        let lang = self.config.lang();
        if row.presence == Presence::Disconnected {
            let seen = row.last_seen.map_or_else(
                || "—".to_owned(),
                |at| {
                    let age = devices::relative_label(now, at, lang);
                    fl!(loader(lang), "device-seen-ago", age = age.as_str())
                },
            );
            return (
                gui::charge_value_text(visuals, colors, seen, false, false),
                None,
            );
        }
        let threshold = self.config.effective_low_threshold(&row.device.name);
        let status = classify(row.charge, threshold);
        let text = charge_value(
            row.presence,
            row.charge.map(|r| r.percent),
            row.charge.map(|r| r.state),
            status,
            lang,
        );
        let low = matches!(status, PrimaryStatus::Low { .. });
        let online = row.presence == Presence::Online;
        let seen_ago = row
            .read_at
            .map(|at| Duration::from_secs(u64::try_from(now.saturating_sub(at)).unwrap_or(0)));
        let note = status_note(row.presence, row.remaining, seen_ago, lang);
        (
            gui::charge_value_text(visuals, colors, text, low, online),
            note,
        )
    }

    /// One row open at a time; an armed Remove does not survive the move.
    fn toggle_expanded(&mut self, device: &DeviceId) {
        self.expanded_device = if self.expanded_device.as_ref() == Some(device) {
            None
        } else {
            Some(device.clone())
        };
        self.delete_state = DeleteState::Idle;
    }

    fn render_device_settings(&mut self, rows: &mut widgets::Rows<'_>, row: &DeviceRow) {
        let lang = self.config.lang();
        let about = device_about(row, lang);
        rows.row(
            &fl!(loader(lang), "device-about"),
            widgets::subtitle(&about),
            |_| (),
        );
        self.render_pin_row(rows, &row.device);
        let own = self
            .config
            .device_overrides
            .get(&row.device.name)
            .cloned()
            .unwrap_or_default();
        self.render_threshold_row(rows, &row.device.name, &own);
        self.render_interval_row(rows, &row.device, &own);
        self.render_remove_row(rows, row);
    }

    fn render_pin_row(&mut self, rows: &mut widgets::Rows<'_>, device: &DeviceId) {
        let l = loader(self.config.lang());
        let name = device.name.as_str();
        let title = fl!(l, "device-pin");
        let hint =
            (self.config.tray_mode == TrayMode::PerDevice).then(|| fl!(l, "device-pin-hint"));
        let mut pinned = self.config.primary_device.as_deref() == Some(name);
        let toggled = rows.row(
            &title,
            |ui| {
                if let Some(hint) = &hint {
                    ui.label(widgets::secondary(ui, hint));
                }
            },
            |ui| {
                widgets::switch(ui, device_switch_id("pin", device), &mut pinned, &title).changed()
            },
        );
        if toggled {
            let name = name.to_owned();
            self.persist(move |target| toggle_primary(&mut target.primary_device, &name, pinned));
        }
    }

    fn render_threshold_row(
        &mut self,
        rows: &mut widgets::Rows<'_>,
        name: &str,
        own: &DeviceSettings,
    ) {
        let lang = self.config.lang();
        let default_threshold = self.config.low_threshold;
        let mut threshold = own.low_threshold.unwrap_or(default_threshold);
        let mut reset = false;
        let committed = rows.row(
            &fl!(loader(lang), "default-low-threshold"),
            |ui| {
                reset = default_subtitle(
                    ui,
                    own.low_threshold.is_some(),
                    &format!("{default_threshold}%"),
                    lang,
                )
            },
            |ui| threshold_slider(ui, &mut threshold),
        );
        if reset {
            self.save_override(name, |own| own.low_threshold = None);
        } else if committed {
            self.save_override(name, |own| own.low_threshold = Some(threshold));
        }
    }

    fn render_interval_row(
        &mut self,
        rows: &mut widgets::Rows<'_>,
        device: &DeviceId,
        own: &DeviceSettings,
    ) {
        let lang = self.config.lang();
        let default_interval = self.config.poll_interval_secs;
        let current = own.poll_interval_secs.unwrap_or(default_interval);
        let mut reset = false;
        let chosen = rows.row(
            &fl!(loader(lang), "default-poll-interval"),
            |ui| {
                let default = interval_label(default_interval, lang);
                reset = default_subtitle(ui, own.poll_interval_secs.is_some(), &default, lang);
            },
            |ui| interval_combo(ui, ("device-interval", device), current, lang),
        );
        if reset {
            self.save_override(&device.name, |own| own.poll_interval_secs = None);
        } else if let Some(interval) = chosen {
            self.save_override(&device.name, |own| own.poll_interval_secs = Some(interval));
        }
    }

    /// `Remove…` arms, then `Remove` deletes: never on the first click.
    /// A device the inventory has not recorded yet has nothing to remove.
    fn render_remove_row(&mut self, rows: &mut widgets::Rows<'_>, row: &DeviceRow) {
        let Some(store_id) = row.store_id else {
            return;
        };
        let cell = devices::delete_cell(self.delete_state, row.store_id);
        let l = loader(self.config.lang());
        let hint = fl!(l, "device-remove-hint");
        let step = rows.row(
            &fl!(l, "device-remove-title"),
            widgets::subtitle(&hint),
            |ui| match cell {
                DeleteCell::Unavailable => None,
                DeleteCell::Arm => ui
                    .button(fl!(l, "device-remove"))
                    .clicked()
                    .then_some(RemoveStep::Arm),
                DeleteCell::Confirm => {
                    let error = gui::status_colors_for(ui, self.config.palette).low;
                    let confirm = egui::RichText::new(fl!(l, "device-remove-confirm")).color(error);
                    let confirmed = ui.button(confirm).clicked();
                    let cancelled = ui.button(fl!(l, "button-cancel")).clicked();
                    if confirmed {
                        Some(RemoveStep::Confirm)
                    } else {
                        cancelled.then_some(RemoveStep::Cancel)
                    }
                }
            },
        );
        match step {
            Some(RemoveStep::Arm) => self.delete_state = DeleteState::Confirming(store_id),
            Some(RemoveStep::Confirm) => self.delete_device(store_id, &row.device),
            Some(RemoveStep::Cancel) => self.delete_state = DeleteState::Idle,
            None => {}
        }
    }

    /// Edits one field of a device's overrides in the config being written:
    /// the other field and the defaults come from the on-disk config, which
    /// may have changed since this window opened.
    fn save_override(&mut self, name: &str, edit: impl FnOnce(&mut DeviceSettings)) {
        let name = name.to_string();
        self.persist(move |target| apply_device_override(target, &name, edit));
    }

    /// Deletes the inventory row and readings, the `hidden_devices` entry and
    /// the pin, which would reattach to a sold device seen again. Waits on the
    /// store on the UI thread, as `persist` does: a rare, confirmed click.
    fn delete_device(&mut self, store_id: i64, device: &DeviceId) {
        let deleted = match &self.store {
            Some(store) => store.delete_device_blocking(store_id),
            None => Ok(()),
        };
        self.finish_delete(store_id, device, deleted);
    }

    /// Forgets the device once the inventory dropped it; says so in the window when it did not.
    fn finish_delete(&mut self, store_id: i64, device: &DeviceId, deleted: anyhow::Result<()>) {
        let name = device.name.as_str();
        self.clear_problem(|p| matches!(p, ActionProblem::Remove { .. }));
        if let Err(e) = deleted {
            tracing::error!("failed to delete device {name:?} from inventory: {e:#}");
            self.action_problem = Some(ActionProblem::Remove {
                name: name.to_owned(),
                detail: format!("{e:#}"),
            });
            self.delete_state = DeleteState::Idle;
            return;
        }

        let forgotten = name.to_string();
        self.persist(move |target| {
            set_shown(&mut target.hidden_devices, &forgotten, true);
            toggle_primary(&mut target.primary_device, &forgotten, false);
        });
        devices::remove_row(&mut self.device_rows, store_id);
        if self.expanded_device.as_ref() == Some(device) {
            self.expanded_device = None;
        }
        self.delete_state = DeleteState::Idle;
    }

    pub(super) fn handle_escape(&mut self, ui: &egui::Ui) {
        if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            return;
        }
        let on_devices = self.tab == Tab::Devices;
        let open = OpenThings {
            delete_armed: self.delete_state != DeleteState::Idle,
            expanded: on_devices && self.expanded_device.is_some(),
            search_active: on_devices && !self.device_search.is_empty(),
        };
        match devices::escape_action(open) {
            EscapeAction::CancelDelete => self.delete_state = DeleteState::Idle,
            EscapeAction::Collapse => self.expanded_device = None,
            EscapeAction::ClearSearch => self.device_search.clear(),
            EscapeAction::CloseWindow => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }
}

/// Kind and transport, and the date a device not connected now was last seen.
fn device_about(row: &DeviceRow, lang: Lang) -> String {
    let mut about = format!(
        "{} · {}",
        row.kind.label(lang),
        row.device.transport.as_str()
    );
    if let (Presence::Disconnected, Some(at)) = (row.presence, row.last_seen) {
        let date = devices::absolute_date_label(at);
        let seen = fl!(loader(lang), "device-last-seen", date = date.as_str());
        about.push_str(&format!(" · {seen}"));
    }
    about
}

/// "The default for all devices", or the default's value and a reset button
/// once the device has its own; true when the reset was clicked.
fn default_subtitle(ui: &mut egui::Ui, overridden: bool, default: &str, lang: Lang) -> bool {
    let l = loader(lang);
    if !overridden {
        ui.label(widgets::secondary(ui, &fl!(l, "device-uses-default")));
        return false;
    }
    ui.horizontal_wrapped(|ui| {
        let text = fl!(l, "device-default-value", value = default);
        ui.label(widgets::secondary(ui, &text));
        let reset = fl!(l, "device-reset");
        let response = ui
            .add(egui::Button::new(RESET).min_size(egui::Vec2::splat(gui::MIN_TARGET)))
            .on_hover_text(&reset);
        let enabled = ui.is_enabled();
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &reset));
        response.clicked()
    })
    .inner
}

/// Global so a test can find the switch it clicks.
fn device_switch_id(purpose: &str, device: &DeviceId) -> egui::Id {
    egui::Id::new(("device-switch", purpose, device))
}

/// Applies `edit` to `name`'s overrides in `config`. A field equal to its
/// global default is no override, and an entry left with neither is removed,
/// so the config never carries a redundant override.
fn apply_device_override(config: &mut Config, name: &str, edit: impl FnOnce(&mut DeviceSettings)) {
    let mut own = config
        .device_overrides
        .get(name)
        .cloned()
        .unwrap_or_default();
    edit(&mut own);
    own.low_threshold = own.low_threshold.filter(|&t| t != config.low_threshold);
    own.poll_interval_secs = own
        .poll_interval_secs
        .filter(|&i| i != config.poll_interval_secs);
    if own == DeviceSettings::default() {
        config.device_overrides.remove(name);
    } else {
        config.device_overrides.insert(name.to_string(), own);
    }
}

/// Exactly one entry changes: `shown` removes `name` from `hidden_devices`,
/// else adds it; every other entry — including one for a device that is not
/// in the currently-discovered list — is left untouched.
fn set_shown(hidden_devices: &mut Vec<String>, name: &str, shown: bool) {
    if shown {
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
    use crate::config;
    use crate::domain::{
        BatteryReading, CHARGING_SIGN, ChargeState, DeviceKind, LOW_SIGN, Transport, format_age,
        format_coarse, state_label,
    };
    use crate::egui_test::{
        TEXT_SCALES, assert_no_overlap, assert_whole, click_at, fully_painted_text_at,
        painted_text_at, run_frame,
    };
    use crate::settings::WINDOW_MIN_SIZE;
    use crate::settings::tests::{app_saving_to, settings_app_with, tab_size};
    use std::collections::HashMap;

    /// The narrowest the window gets, tall enough that nothing scrolls.
    const TEST_SIZE: [f32; 2] = [WINDOW_MIN_SIZE[0], 1400.0];
    const DAY: i64 = 86_400;

    fn device_row(
        name: &str,
        kind: DeviceKind,
        presence: Presence,
        charge: Option<(u8, ChargeState)>,
        store_id: Option<i64>,
    ) -> DeviceRow {
        DeviceRow {
            store_id,
            device: DeviceId {
                name: name.to_owned(),
                transport: Transport::Hidraw,
                locator: None,
            },
            kind,
            charge: charge.map(|(percent, state)| BatteryReading::new(percent, state)),
            presence,
            remaining: None,
            read_at: None,
            first_seen: store_id.map(|_| state::now_unix() - 30 * DAY),
            last_seen: store_id.map(|_| state::now_unix() - 2 * DAY),
        }
    }

    fn keyboard() -> DeviceRow {
        let charge = Some((39, ChargeState::Discharging));
        DeviceRow {
            remaining: Some(Duration::from_secs(7 * 3600)),
            ..device_row(
                "NuPhy Air75 V2",
                DeviceKind::Keyboard,
                Presence::Online,
                charge,
                Some(1),
            )
        }
    }

    fn earbuds() -> DeviceRow {
        let charge = None;
        device_row(
            "Nothing Ear (2)",
            DeviceKind::Headset,
            Presence::Disconnected,
            charge,
            Some(7),
        )
    }

    /// One row per state the tab distinguishes.
    fn every_state() -> Vec<DeviceRow> {
        use ChargeState::{Charging, Discharging, Full};
        vec![
            keyboard(),
            device_row(
                "Aerox 5 Wireless",
                DeviceKind::Mouse,
                Presence::Online,
                Some((15, Discharging)),
                Some(2),
            ),
            device_row(
                "Arctis Nova 7",
                DeviceKind::Headset,
                Presence::Online,
                Some((40, Charging)),
                None,
            ),
            device_row(
                "8BitDo Ultimate 2C",
                DeviceKind::Controller,
                Presence::Online,
                Some((100, Full)),
                Some(3),
            ),
            device_row(
                "Aerox 3",
                DeviceKind::Mouse,
                Presence::NoAccess,
                None,
                Some(4),
            ),
            DeviceRow {
                read_at: Some(state::now_unix() - 2 * 3600),
                ..device_row(
                    "MX Anywhere 3",
                    DeviceKind::Mouse,
                    Presence::Unreachable,
                    None,
                    Some(5),
                )
            },
            earbuds(),
        ]
    }

    fn app_in(lang: Lang, config: Config) -> SettingsApp {
        let mut app = settings_app_with(Config {
            language: Some(lang.tag().to_owned()),
            ..config
        });
        app.tab = Tab::Devices;
        app.device_rows = every_state();
        app
    }

    fn seen_ago(lang: Lang) -> String {
        let age = devices::relative_label(state::now_unix(), state::now_unix() - 2 * DAY, lang);
        fl!(loader(lang), "device-seen-ago", age = age.as_str())
    }

    #[test]
    fn device_rows_are_whole_in_every_language_and_text_scale() {
        for (lang, scale) in Lang::ALL
            .into_iter()
            .flat_map(|lang| TEXT_SCALES.map(|scale| (lang, scale)))
        {
            let mut app = app_in(lang, Config::default());

            let size = tab_size(scale, TEST_SIZE[1]);
            let painted = fully_painted_text_at(size, |ui| app.render_devices_tab(ui));

            let l = loader(lang);
            let estimate = format_coarse(Duration::from_secs(7 * 3600), lang);
            let age = format_age(Duration::from_secs(2 * 3600), lang);
            let mut expected: Vec<String> = [
                "devices-connected",
                "devices-seen-before",
                "button-refresh",
                "note-no-access",
            ]
            .map(|id| l.get(id))
            .into();
            let offline = PrimaryStatus::Offline;
            expected.extend([
                charge_value(Presence::NoAccess, None, None, offline, lang),
                charge_value(Presence::Unreachable, None, None, offline, lang),
            ]);
            expected.extend(every_state().into_iter().map(|row| row.device.name));
            expected.extend([
                "39%".to_owned(),
                format!("{LOW_SIGN} 15%"),
                format!("{CHARGING_SIGN} 40%"),
                format!("100% · {}", state_label(ChargeState::Full, lang)),
                seen_ago(lang),
                fl!(l, "note-remaining", estimate = estimate.as_str()),
                fl!(l, "note-last-reading", age = age.as_str()),
            ]);
            assert_whole(&painted, &expected, &format!("{lang:?} at {scale}"));
        }
    }

    #[test]
    fn an_expanded_row_paints_its_settings_in_every_language_and_text_scale() {
        for (lang, scale, armed) in Lang::ALL
            .into_iter()
            .flat_map(|lang| TEXT_SCALES.map(|scale| (lang, scale)))
            .flat_map(|(lang, scale)| [false, true].map(|armed| (lang, scale, armed)))
        {
            let mut overrides = HashMap::new();
            overrides.insert(
                keyboard().device.name,
                DeviceSettings {
                    poll_interval_secs: Some(300),
                    low_threshold: Some(10),
                },
            );
            let mut app = app_in(
                lang,
                Config {
                    tray_mode: TrayMode::PerDevice,
                    device_overrides: overrides,
                    ..Config::default()
                },
            );
            app.expanded_device = Some(keyboard().device);
            if armed {
                app.delete_state = DeleteState::Confirming(1);
            }

            let size = tab_size(scale, 1800.0);
            let painted = fully_painted_text_at(size, |ui| app.render_devices_tab(ui));

            let l = loader(lang);
            let mut expected: Vec<String> = [
                "device-pin",
                "device-pin-hint",
                "default-low-threshold",
                "default-poll-interval",
                "device-remove-title",
                "device-remove-hint",
            ]
            .map(|id| l.get(id))
            .into();
            if armed {
                expected.extend(["device-remove-confirm", "button-cancel"].map(|id| l.get(id)));
            } else {
                expected.push(l.get("device-remove"));
            }
            let default_interval = interval_label(60, lang);
            expected.extend([
                fl!(l, "device-default-value", value = "20%"),
                fl!(l, "device-default-value", value = default_interval.as_str()),
                interval_label(300, lang),
                RESET.to_owned(),
                "10".to_owned(),
                "%".to_owned(),
            ]);
            assert_whole(&painted, &expected, &format!("{lang:?} at {scale}"));
        }
    }

    #[test]
    fn every_devices_tab_target_is_at_least_24_points() {
        for armed in [false, true] {
            let mut overrides = HashMap::new();
            overrides.insert(
                keyboard().device.name,
                DeviceSettings {
                    poll_interval_secs: Some(300),
                    low_threshold: Some(10),
                },
            );
            let mut app = app_in(
                Lang::En,
                Config {
                    device_overrides: overrides,
                    ..Config::default()
                },
            );
            app.expanded_device = Some(keyboard().device);
            if armed {
                app.delete_state = DeleteState::Confirming(1);
            }
            let targets = crate::egui_test::targets_at(tab_size(1.0, 1800.0), |ui| {
                app.render_devices_tab(ui);
            });
            assert!(
                targets
                    .iter()
                    .any(|t| t.name.as_deref() == Some("Use the default")),
                "{targets:?}"
            );
            crate::egui_test::assert_targets_at_least(&targets, gui::MIN_TARGET);
        }
    }

    /// A device without access gets a banner with the command that grants it
    /// and a link to the README, not only "run rigbat doctor".
    #[test]
    fn no_access_shows_the_fix_command_and_the_docs_in_every_language_and_text_scale() {
        for (lang, scale) in Lang::ALL
            .into_iter()
            .flat_map(|lang| TEXT_SCALES.map(|scale| (lang, scale)))
        {
            let mut app = app_in(lang, Config::default());
            let size = tab_size(scale, 2000.0);
            let painted = fully_painted_text_at(size, |ui| app.render_devices_tab(ui));
            let l = loader(lang);
            let expected = [
                fl!(l, "devices-no-access-title", count = 1),
                l.get("devices-no-access-body"),
                INSTALL_UDEV_RULE.to_owned(),
                l.get("button-copy"),
                l.get("devices-no-access-help"),
            ];
            assert_whole(&painted, &expected, &format!("{lang:?} at {scale}"));
        }

        let mut app = app_in(Lang::En, Config::default());
        app.device_rows
            .retain(|row| row.presence != Presence::NoAccess);
        let painted = fully_painted_text_at(TEST_SIZE, |ui| app.render_devices_tab(ui));
        assert!(!painted.iter().any(|p| p.text == INSTALL_UDEV_RULE));
    }

    #[test]
    fn the_no_access_link_opens_the_permissions_section() {
        let (tx, opened) = std::sync::mpsc::channel();
        let mut app = app_in(Lang::En, Config::default());
        app.open_uri = std::sync::Arc::new(move |url| {
            tx.send(url).expect("the test holds the receiver");
            Box::pin(std::future::ready(Ok(())))
        });
        let ctx = egui::Context::default();
        let output = run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| app.render_devices_tab(ui));
        let link = text_rects(&output, "How to fix")[0];
        click_at(&ctx, TEST_SIZE, link.center(), |ui| {
            app.render_devices_tab(ui)
        });
        assert_eq!(opened.try_recv().ok().as_deref(), Some(PERMISSIONS_DOCS));
        assert!(PERMISSIONS_DOCS.ends_with("#permissions"));
    }

    /// "Refreshing…" replaces "Refresh" only once a scan has run `PROGRESS_DELAY`;
    /// the button is disabled from the start.
    #[test]
    fn the_refresh_label_changes_only_after_the_progress_delay() {
        let mut app = app_in(Lang::En, Config::default());
        let label = |app: &mut SettingsApp| {
            let painted = painted_text_at(TEST_SIZE, |ui| app.render_devices_tab(ui));
            ["Refresh", "Refreshing…"]
                .into_iter()
                .find(|t| painted.iter().any(|p| p == t))
        };

        // Started in the future: a slow runner cannot use up the delay mid-render.
        app.scanning = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
        assert_eq!(label(&mut app), Some("Refresh"));

        app.scanning = Some(std::time::Instant::now() - gui::PROGRESS_DELAY);
        assert_eq!(label(&mut app), Some("Refreshing…"));

        app.scanning = None;
        assert_eq!(label(&mut app), Some("Refresh"));
    }

    /// A removal the inventory refused keeps the row and says so in the window.
    #[test]
    fn a_failed_removal_is_shown_in_every_language() {
        for lang in Lang::ALL {
            let mut app = app_in(lang, Config::default());
            let row = keyboard();
            app.expanded_device = Some(row.device.clone());
            app.delete_state = DeleteState::Confirming(1);
            app.finish_delete(1, &row.device, Err(anyhow::anyhow!("database is locked")));

            assert_eq!(app.delete_state, DeleteState::Idle);
            assert!(app.device_rows.iter().any(|r| r.device == row.device));
            let painted = fully_painted_text_at(TEST_SIZE, |ui| {
                app.render_problems(ui);
                app.render_devices_tab(ui);
            });
            let l = loader(lang);
            let expected = [
                fl!(l, "problem-remove", name = row.device.name.as_str()),
                l.get("problem-remove-fix"),
                "database is locked".to_owned(),
            ];
            assert_whole(&painted, &expected, &format!("{lang:?}"));

            app.finish_delete(1, &row.device, Ok(()));
            assert_eq!(app.action_problem, None);
        }
    }

    /// Kind, transport and the last-seen date are painted in the open row, not
    /// only in the hover text a pointer has to find.
    #[test]
    fn an_open_row_paints_its_kind_transport_and_last_seen_date() {
        for lang in Lang::ALL {
            let l = loader(lang);
            for row in [keyboard(), earbuds()] {
                let mut app = app_in(lang, Config::default());
                app.expanded_device = Some(row.device.clone());

                let painted = fully_painted_text_at(TEST_SIZE, |ui| app.render_devices_tab(ui));

                let mut about = format!("{} · hidraw", row.kind.label(lang));
                if row.presence == Presence::Disconnected {
                    let date = devices::absolute_date_label(row.last_seen.expect("seen before"));
                    about = format!(
                        "{about} · {}",
                        fl!(l, "device-last-seen", date = date.as_str())
                    );
                }
                for text in [l.get("device-about"), about] {
                    assert!(
                        painted.iter().any(|p| p.text == text),
                        "{lang:?}: {text:?} is cut off or missing: {painted:?}"
                    );
                }
                assert_no_overlap(&painted);
            }
        }
    }

    fn header(ctx: &egui::Context, row: &DeviceRow) -> egui::Response {
        ctx.read_response(egui::Id::new(("device-row", &row.device)))
            .expect("the row was laid out")
    }

    fn escape() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    #[test]
    fn a_click_expands_one_row_at_a_time_and_escape_collapses_it() {
        let mut app = app_in(Lang::En, Config::default());
        let ctx = egui::Context::default();
        let frame = |app: &mut SettingsApp, ui: &mut egui::Ui| {
            app.handle_escape(ui);
            app.render_devices_tab(ui);
        };
        run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| frame(&mut app, ui));

        let first = header(&ctx, &keyboard()).rect.center();
        click_at(&ctx, TEST_SIZE, first, |ui| frame(&mut app, ui));
        assert_eq!(app.expanded_device, Some(keyboard().device));
        let pin = fl!(loader(Lang::En), "device-pin");
        let painted = painted_text_at(TEST_SIZE, |ui| frame(&mut app, ui));
        assert_eq!(
            painted.iter().filter(|t| **t == pin).count(),
            1,
            "{painted:?}"
        );

        run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| frame(&mut app, ui));
        let second = header(&ctx, &earbuds()).rect.center();
        click_at(&ctx, TEST_SIZE, second, |ui| frame(&mut app, ui));
        assert_eq!(app.expanded_device, Some(earbuds().device));

        let output = run_frame(&ctx, TEST_SIZE, vec![escape()], |ui| frame(&mut app, ui));
        assert_eq!(app.expanded_device, None);
        assert!(
            !output
                .viewport_output
                .values()
                .any(|v| v.commands.contains(&egui::ViewportCommand::Close)),
            "Escape closed the window instead of collapsing the row"
        );
    }

    #[test]
    fn the_tray_switch_edits_hidden_devices_without_expanding_the_row() {
        let (mut app, path) = app_saving_to("device-shown-switch", Config::default());
        app.tab = Tab::Devices;
        app.device_rows = every_state();
        let ctx = egui::Context::default();
        run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| app.render_devices_tab(ui));
        let switch = ctx
            .read_response(device_switch_id("shown", &keyboard().device))
            .expect("the switch was laid out");

        click_at(&ctx, TEST_SIZE, switch.rect.center(), |ui| {
            app.render_devices_tab(ui)
        });

        assert_eq!(
            config::load_from(&path).hidden_devices,
            [keyboard().device.name]
        );
        assert_eq!(app.expanded_device, None);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Where `text` was painted this frame, top to bottom.
    fn text_rects(output: &egui::FullOutput, text: &str) -> Vec<egui::Rect> {
        fn walk(shape: &egui::Shape, text: &str, out: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Text(t) if t.galley.text() == text => {
                    out.push(t.galley.rect.translate(t.pos.to_vec2()));
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, text, out)),
                _ => {}
            }
        }
        let mut rects = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, text, &mut rects);
        }
        rects.sort_by(|a, b| a.top().total_cmp(&b.top()));
        rects
    }

    #[test]
    fn pin_interval_and_reset_edit_the_config() {
        let name = keyboard().device.name;
        let mut overrides = HashMap::new();
        overrides.insert(
            name.clone(),
            DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(10),
            },
        );
        let (mut app, path) = app_saving_to(
            "device-settings",
            Config {
                device_overrides: overrides,
                ..Config::default()
            },
        );
        app.tab = Tab::Devices;
        app.device_rows = every_state();
        app.expanded_device = Some(keyboard().device);
        let ctx = egui::Context::default();
        let size = TEST_SIZE;
        for _ in 0..2 {
            run_frame(&ctx, size, Vec::new(), |ui| app.render_devices_tab(ui));
        }

        let pin = ctx
            .read_response(device_switch_id("pin", &keyboard().device))
            .expect("the pin switch was laid out");
        click_at(&ctx, size, pin.rect.center(), |ui| {
            app.render_devices_tab(ui)
        });
        assert_eq!(config::load_from(&path).primary_device, Some(name.clone()));

        let output = run_frame(&ctx, size, Vec::new(), |ui| app.render_devices_tab(ui));
        let combo = text_rects(&output, &interval_label(60, Lang::En))[0];
        click_at(&ctx, size, combo.center(), |ui| app.render_devices_tab(ui));
        let output = run_frame(&ctx, size, Vec::new(), |ui| app.render_devices_tab(ui));
        let choice = *text_rects(&output, "5\u{a0}min")
            .last()
            .expect("the list opened");
        click_at(&ctx, size, choice.center(), |ui| app.render_devices_tab(ui));
        let saved = config::load_from(&path).device_overrides;
        assert_eq!(
            saved.get(&name),
            Some(&DeviceSettings {
                poll_interval_secs: Some(300),
                low_threshold: Some(10),
            })
        );

        let output = run_frame(&ctx, size, Vec::new(), |ui| app.render_devices_tab(ui));
        let resets = text_rects(&output, RESET);
        assert_eq!(resets.len(), 2, "both settings are the device's own");
        click_at(&ctx, size, resets[0].center(), |ui| {
            app.render_devices_tab(ui)
        });
        let saved = config::load_from(&path).device_overrides;
        assert_eq!(
            saved.get(&name),
            Some(&DeviceSettings {
                poll_interval_secs: Some(300),
                low_threshold: None,
            })
        );

        let output = run_frame(&ctx, size, Vec::new(), |ui| app.render_devices_tab(ui));
        let reset = text_rects(&output, RESET)[0];
        click_at(&ctx, size, reset.center(), |ui| app.render_devices_tab(ui));
        assert!(config::load_from(&path).device_overrides.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_seen_before_row_shows_its_age_and_offers_remove_but_no_switch() {
        let (mut app, path) = app_saving_to("device-remove", Config::default());
        app.tab = Tab::Devices;
        app.device_rows = every_state();
        let ctx = egui::Context::default();
        let output = run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| app.render_devices_tab(ui));

        assert_eq!(text_rects(&output, &seen_ago(Lang::En)).len(), 1);
        assert!(
            ctx.read_response(device_switch_id("shown", &earbuds().device))
                .is_none()
        );
        assert!(
            ctx.read_response(device_switch_id("shown", &keyboard().device))
                .is_some()
        );

        app.expanded_device = Some(earbuds().device);
        run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| app.render_devices_tab(ui));
        let output = run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| app.render_devices_tab(ui));
        let remove = text_rects(&output, "Remove…")[0];
        click_at(&ctx, TEST_SIZE, remove.center(), |ui| {
            app.render_devices_tab(ui)
        });
        assert_eq!(app.delete_state, DeleteState::Confirming(7));
        assert!(app.device_rows.iter().any(|r| r.device == earbuds().device));

        let output = run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| app.render_devices_tab(ui));
        let confirm = text_rects(&output, "Remove")[0];
        click_at(&ctx, TEST_SIZE, confirm.center(), |ui| {
            app.render_devices_tab(ui)
        });
        assert!(!app.device_rows.iter().any(|r| r.device == earbuds().device));
        assert_eq!(app.expanded_device, None);
        assert_eq!(app.delete_state, DeleteState::Idle);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_search_field_appears_only_for_a_long_list() {
        let hint = fl!(loader(Lang::En), "device-search-hint");
        let mut app = app_in(Lang::En, Config::default());
        for count in [SEARCH_MIN_DEVICES, SEARCH_MIN_DEVICES + 1] {
            app.device_rows = (0..count)
                .map(|i| {
                    device_row(
                        &format!("Mouse {i}"),
                        DeviceKind::Mouse,
                        Presence::Online,
                        None,
                        None,
                    )
                })
                .collect();
            let painted = painted_text_at(TEST_SIZE, |ui| app.render_devices_tab(ui));
            assert_eq!(
                painted.contains(&hint),
                count > SEARCH_MIN_DEVICES,
                "{count}: {painted:?}"
            );
        }
    }

    #[test]
    fn with_no_devices_the_tab_says_so() {
        let mut app = app_in(Lang::En, Config::default());
        app.device_rows.clear();
        let painted = painted_text_at(TEST_SIZE, |ui| app.render_devices_tab(ui));
        assert!(
            painted.contains(&fl!(loader(Lang::En), "devices-empty")),
            "{painted:?}"
        );
        assert!(!painted.contains(&fl!(loader(Lang::En), "devices-connected")));
    }

    #[test]
    fn an_unanswered_tray_is_shown_above_the_groups_in_every_language() {
        for (lang, text) in [
            (Lang::En, "The running tray did not answer"),
            (Lang::Ru, "Запущенный трей не ответил"),
        ] {
            let mut app = app_in(lang, Config::default());
            app.tray_unanswered = true;

            let painted = painted_text_at(TEST_SIZE, |ui| app.render_devices_tab(ui));

            assert!(
                painted.iter().any(|t| t.starts_with(text)),
                "{lang:?}: status missing; got {painted:?}"
            );
            assert!(painted.iter().any(|t| t == "MX Anywhere 3"), "{lang:?}");
        }
    }

    #[test]
    fn every_kind_paints_its_emoji_whole() {
        let mut app = app_in(Lang::En, Config::default());
        app.device_rows = crate::egui_test::KINDS
            .into_iter()
            .enumerate()
            .map(|(i, kind)| {
                let charge = Some((50, ChargeState::Discharging));
                let name = format!("device {i}");
                device_row(&name, kind, Presence::Online, charge, Some(i as i64))
            })
            .collect();
        let ctx = egui::Context::default();
        let mut output = None;
        for _ in 0..2 {
            output = Some(run_frame(&ctx, TEST_SIZE, Vec::new(), |ui| {
                app.render_devices_tab(ui)
            }));
        }
        let painted = crate::egui_test::painted(&output.expect("a frame"));
        for (i, kind) in crate::egui_test::KINDS.into_iter().enumerate() {
            let find = |text: &str| {
                let found: Vec<_> = painted.iter().filter(|p| p.text == text).collect();
                assert_eq!(found.len(), 1, "{kind:?} {text:?}: {painted:?}");
                found[0].rect
            };
            let glyph = find(gui::kind_glyph(kind));
            let title = find(&format!("device {i}"));
            assert!(
                glyph.left() >= title.left() - gui::GLYPH_COLUMN && glyph.right() <= title.left(),
                "{kind:?} leaves its column: {glyph:?}, title at {title:?}"
            );
        }
    }

    #[test]
    fn the_reset_glyph_is_in_the_bundled_fonts() {
        let ctx = egui::Context::default();
        crate::egui_test::empty_pass(&ctx);
        let font = egui::TextStyle::Button.resolve(&ctx.global_style());
        ctx.fonts_mut(|fonts| assert!(crate::egui_test::has_glyphs(fonts, &font, RESET)));
    }

    /// Defaults of 20 % and 60 s.
    fn defaults() -> Config {
        Config {
            low_threshold: 20,
            poll_interval_secs: 60,
            ..Config::default()
        }
    }

    fn set(threshold: Option<u8>, interval: Option<u64>) -> impl FnOnce(&mut DeviceSettings) {
        move |own| {
            own.low_threshold = threshold;
            own.poll_interval_secs = interval;
        }
    }

    #[test]
    fn apply_device_override_sets_both_fields() {
        let mut config = defaults();
        apply_device_override(&mut config, "mouse", set(Some(10), Some(30)));
        assert_eq!(
            config.device_overrides.get("mouse"),
            Some(&DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: Some(10),
            })
        );
    }

    #[test]
    fn apply_device_override_clearing_removes_key() {
        let mut config = defaults();
        config.device_overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: Some(10),
            },
        );
        apply_device_override(&mut config, "mouse", set(None, None));
        assert!(!config.device_overrides.contains_key("mouse"));
    }

    #[test]
    fn apply_device_override_equal_to_default_removes_key() {
        let mut config = defaults();
        apply_device_override(&mut config, "mouse", set(Some(20), Some(60)));
        assert!(!config.device_overrides.contains_key("mouse"));
    }

    #[test]
    fn apply_device_override_partial_keeps_only_non_default_field() {
        let mut config = defaults();
        apply_device_override(&mut config, "mouse", set(Some(15), Some(60)));
        assert_eq!(
            config.device_overrides.get("mouse"),
            Some(&DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(15),
            })
        );
    }

    #[test]
    fn apply_device_override_edits_one_field_and_keeps_the_other() {
        let mut config = defaults();
        apply_device_override(&mut config, "mouse", set(Some(10), Some(30)));
        apply_device_override(&mut config, "mouse", |own| own.low_threshold = None);
        assert_eq!(
            config.device_overrides.get("mouse"),
            Some(&DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: None,
            })
        );
    }

    #[test]
    fn set_shown_unchecking_adds_entry() {
        let mut hidden = Vec::new();
        set_shown(&mut hidden, "mouse", false);
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
    fn set_shown_checking_removes_entry() {
        let mut hidden = vec!["mouse".to_string()];
        set_shown(&mut hidden, "mouse", true);
        assert!(hidden.is_empty());
    }

    #[test]
    fn set_shown_leaves_unrelated_entries_untouched() {
        // An entry for a device that is not in the roster survives a toggle.
        let mut hidden = vec!["offline-device".to_string()];
        set_shown(&mut hidden, "mouse", false);
        assert_eq!(
            hidden,
            vec!["offline-device".to_string(), "mouse".to_string()]
        );
        set_shown(&mut hidden, "mouse", true);
        assert_eq!(hidden, vec!["offline-device".to_string()]);
    }

    #[test]
    fn set_shown_checking_already_shown_is_noop() {
        let mut hidden = vec!["keyboard".to_string()];
        set_shown(&mut hidden, "mouse", true);
        assert_eq!(hidden, vec!["keyboard".to_string()]);
    }

    #[test]
    fn set_shown_unchecking_already_hidden_is_noop() {
        let mut hidden = vec!["mouse".to_string()];
        set_shown(&mut hidden, "mouse", false);
        assert_eq!(hidden, vec!["mouse".to_string()]);
    }
}
