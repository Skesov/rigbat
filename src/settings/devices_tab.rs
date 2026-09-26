use std::collections::HashMap;
use std::time::Duration;

use eframe::egui;

use super::devices::{self, DeleteCell, DeleteState, DeviceRow, Dismissible, EscapeAction};
use super::general_tab::{interval_combo, interval_label, threshold_slider};
use super::{SettingsApp, Tab, scan, widgets};
use crate::config::DeviceSettings;
use crate::domain::{
    DeviceId, Presence, PrimaryStatus, TrayMode, charge_value, classify, status_note,
};
use crate::gui;
use crate::i18n::{Lang, fl, loader};
use crate::state;

/// Above this many devices the tab offers a search field.
const SEARCH_MIN_DEVICES: usize = 8;
const SEARCH_WIDTH: f32 = 220.0;
const RESET: &str = "\u{21BA}";

/// What the Remove row's buttons asked for this frame.
enum RemoveStep {
    Arm,
    Confirm,
    Cancel,
}

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

    /// The Devices tab: every device `apply_scan_result` merged into
    /// `self.device_rows` — the inventory and the current scan, so a device
    /// the scan did not find still shows — as expander rows in two groups.
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
            let warn = gui::status_colors(ui.visuals(), self.config.palette).warn;
            ui.label(egui::RichText::new(fl!(l, "devices-tray-unanswered")).color(warn));
        }
        ui.add_space(widgets::TOOLBAR_GAP);

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
                    self.render_device(rows, row, &visuals, now);
                }
            });
        }
    }

    /// One device: the expander line, and its settings while expanded.
    fn render_device(
        &mut self,
        rows: &mut widgets::Rows<'_>,
        row: &DeviceRow,
        visuals: &egui::Visuals,
        now: i64,
    ) {
        let lang = self.config.lang();
        let l = loader(lang);
        let name = row.device.name.as_str();
        let connected = row.presence != Presence::Disconnected;
        let (value, note) = self.device_value(row, visuals, now);
        let mut hover = format!(
            "{} · {}",
            row.kind.label(lang),
            row.device.transport.as_str()
        );
        if let (false, Some(at)) = (connected, row.last_seen) {
            hover.push_str(&format!(" · {}", devices::absolute_date_label(at)));
        }
        let header = widgets::Expander {
            id: egui::Id::new(("device-row", &row.device)),
            kind: row.kind,
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
            self.persist(move |target| toggle_hidden(&mut target.hidden_devices, &name, shown));
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
        visuals: &egui::Visuals,
        now: i64,
    ) -> (egui::RichText, Option<String>) {
        let lang = self.config.lang();
        let colors = gui::status_colors(visuals, self.config.palette);
        if row.presence == Presence::Disconnected {
            let seen = row.last_seen.map_or_else(
                || "—".to_owned(),
                |at| {
                    let age = devices::relative_label(now, at, lang);
                    fl!(loader(lang), "device-seen-ago", age = age.as_str())
                },
            );
            return (
                gui::charge_value_text(visuals, &colors, seen, false, false),
                None,
            );
        }
        let threshold = self
            .config
            .device_overrides
            .get(&row.device.name)
            .and_then(|d| d.low_threshold)
            .unwrap_or(self.config.low_threshold);
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
            gui::charge_value_text(visuals, &colors, text, low, online),
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
        let l = loader(lang);
        let name = row.device.name.as_str();

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
                widgets::switch(
                    ui,
                    device_switch_id("pin", &row.device),
                    &mut pinned,
                    &title,
                )
                .changed()
            },
        );
        if toggled {
            let name = name.to_owned();
            self.persist(move |target| toggle_primary(&mut target.primary_device, &name, pinned));
        }

        let own = self
            .config
            .device_overrides
            .get(name)
            .cloned()
            .unwrap_or_default();
        let default_threshold = self.config.low_threshold;
        let mut threshold = own.low_threshold.unwrap_or(default_threshold);
        let mut reset = false;
        let committed = rows.row(
            &fl!(l, "default-low-threshold"),
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
            self.save_threshold_override(name, None);
        } else if committed {
            self.save_threshold_override(name, Some(threshold));
        }

        let default_interval = self.config.poll_interval_secs;
        let current = own.poll_interval_secs.unwrap_or(default_interval);
        let mut reset = false;
        let chosen = rows.row(
            &fl!(l, "default-poll-interval"),
            |ui| {
                let default = interval_label(default_interval, lang);
                reset = default_subtitle(ui, own.poll_interval_secs.is_some(), &default, lang);
            },
            |ui| interval_combo(ui, ("device-interval", &row.device), current, lang),
        );
        if reset {
            self.save_interval_override(name, None);
        } else if let Some(interval) = chosen {
            self.save_interval_override(name, Some(interval));
        }

        self.render_remove_row(rows, row);
    }

    /// `Remove…` arms, then `Remove` deletes (T35: never on the first click).
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
                    let error = gui::status_colors(ui.visuals(), self.config.palette).low;
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

    /// Forgets a device (T34): deletes its inventory row and readings, its
    /// `hidden_devices` entry — nothing left to show it as hidden once it no
    /// longer exists — and its pin on the aggregate icon, which would
    /// otherwise reattach itself the moment a sold device is plugged in
    /// somewhere else and seen again. The UI thread waits for the store, the
    /// same as `persist`'s config write: a deliberate, infrequent,
    /// user-confirmed click, not the per-frame inventory read `spawn_scan`
    /// keeps off the UI thread.
    fn delete_device(&mut self, store_id: i64, device: &DeviceId) {
        let name = device.name.as_str();
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
        if self.expanded_device.as_ref() == Some(device) {
            self.expanded_device = None;
        }
        self.delete_state = DeleteState::Idle;
    }

    /// Escape, resolved by `devices::escape_action`: dismiss the armed
    /// removal, else collapse the open row, else clear the device search,
    /// else close the window.
    ///
    /// The escalation matters more than the closing does — a window that
    /// closed on the first Escape would discard an armed confirmation by
    /// doing the one thing that looks like "never mind" and is not.
    pub(super) fn handle_escape(&mut self, ui: &egui::Ui) {
        if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            return;
        }
        let on_devices = self.tab == Tab::Devices;
        let open = Dismissible {
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
        let response = ui.small_button(RESET).on_hover_text(&reset);
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
    use crate::config::{self, Config};
    use crate::domain::{
        BatteryReading, CHARGING_SIGN, ChargeState, DeviceKind, LOW_SIGN, Transport, format_age,
        format_coarse, state_label,
    };
    use crate::egui_test::{
        assert_no_overlap, click_at, fully_painted_text_at, painted_text_at, run_frame,
    };
    use crate::settings::WINDOW_MIN_SIZE;
    use crate::settings::tests::{app_saving_to, settings_app_with};

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

    fn assert_whole_on_one_line(
        painted: &[crate::egui_test::Painted],
        expected: &[String],
        lang: Lang,
    ) {
        for text in expected {
            let lines = painted.iter().find(|p| &p.text == text).map(|p| p.lines);
            assert_eq!(
                lines,
                Some(1),
                "{lang:?}: {text:?} is cut off, missing or wrapped: {painted:?}"
            );
        }
        assert_no_overlap(painted);
    }

    fn seen_ago(lang: Lang) -> String {
        let age = devices::relative_label(state::now_unix(), state::now_unix() - 2 * DAY, lang);
        fl!(loader(lang), "device-seen-ago", age = age.as_str())
    }

    #[test]
    fn device_rows_are_whole_on_one_line_in_every_language() {
        for lang in Lang::ALL {
            let mut app = app_in(lang, Config::default());

            let painted = fully_painted_text_at(TEST_SIZE, |ui| app.render_devices_tab(ui));

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
            assert_whole_on_one_line(&painted, &expected, lang);
        }
    }

    #[test]
    fn an_expanded_row_paints_its_settings_in_every_language() {
        for lang in Lang::ALL {
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

                let painted = fully_painted_text_at(TEST_SIZE, |ui| app.render_devices_tab(ui));

                let l = loader(lang);
                let mut expected: Vec<String> = [
                    "device-pin",
                    "default-low-threshold",
                    "default-poll-interval",
                    "device-remove-title",
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
                assert_whole_on_one_line(&painted, &expected, lang);
                for hint in ["device-pin-hint", "device-remove-hint"] {
                    let hint = l.get(hint);
                    assert!(
                        painted.iter().any(|p| p.text == hint),
                        "{lang:?}: {hint:?} missing"
                    );
                }
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
        run_frame(&ctx, size, Vec::new(), |ui| app.render_devices_tab(ui));

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
        let choice = *text_rects(&output, "5 min")
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
    fn every_kind_paints_the_tray_icon_glyph_whole() {
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
        let glyphs = crate::egui_test::painted_kind_glyphs(&output.expect("a frame"));
        for kind in crate::egui_test::KINDS {
            let painted: Vec<_> = glyphs.iter().filter(|(k, ..)| *k == kind).collect();
            assert_eq!(painted.len(), 1, "{kind:?}: {glyphs:?}");
            let (_, rect, cut) = painted[0];
            assert!(!cut, "{kind:?} cut: {rect:?}");
            assert!(
                rect.width() > 0.0 && rect.width() <= gui::GLYPH_SIZE + 1.0,
                "{rect:?}"
            );
        }
    }
    #[test]
    fn the_reset_glyph_is_in_the_bundled_fonts() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        let font = egui::TextStyle::Button.resolve(&ctx.global_style());
        ctx.fonts_mut(|fonts| assert!(fonts.has_glyphs(&font, RESET)));
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
