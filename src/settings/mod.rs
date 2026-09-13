use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::autostart;
use crate::config::{self, Config, DeviceSettings, DisplayMode, TrayMode};
use crate::domain::DeviceInfo;

/// Seconds the "Changes saved." status line remains visible after a save.
const SAVED_VISIBLE_SECS: u64 = 2;

/// Global low-battery threshold range, percent. Below 5% the warning fires too
/// late to matter; above 50% it stops meaning "low".
const LOW_THRESHOLD_RANGE: std::ops::RangeInclusive<u8> = 5..=50;

/// Global poll interval range, seconds. The lower bound guards against waking
/// a HID device every second, which drains the battery it is meant to monitor.
const POLL_INTERVAL_RANGE: std::ops::RangeInclusive<u64> = 10..=3600;

struct SettingsApp {
    config: Config,
    devices: Vec<DeviceInfo>,
    /// Set to `Some(Instant::now())` on every successful save; cleared implicitly
    /// by comparing elapsed time on each frame.
    saved_at: Option<Instant>,
    /// Reflects `~/.config/autostart/rigbat.desktop` existence — not stored in Config.
    autostart_enabled: bool,
}

impl SettingsApp {
    /// Renders one section header: bold label followed by a small gap.
    fn section_header(ui: &mut egui::Ui, title: &str) {
        ui.strong(title);
        ui.add_space(4.0);
    }

    /// Copies the fields this window owns onto `target`, leaving everything
    /// else (in particular `primary_device`, owned by the tray menu) untouched.
    fn apply_to(&self, target: &mut Config) {
        target.display_mode = self.config.display_mode;
        target.tray_mode = self.config.tray_mode;
        target.shown_devices = self.config.shown_devices.clone();
        target.notifications_enabled = self.config.notifications_enabled;
        target.poll_interval_secs = self.config.poll_interval_secs;
        target.low_threshold = self.config.low_threshold;
        target.device_overrides = self.config.device_overrides.clone();
    }

    /// Saves the current config and flashes the "Changes saved." status for
    /// `SAVED_VISIBLE_SECS`. Logs on failure; the status line stays unchanged.
    ///
    /// Re-reads the on-disk config first and merges only the fields this window
    /// owns into it: the tray menu also writes `primary_device` on every device
    /// click, and a stale in-memory snapshot here would silently revert that.
    fn persist(&mut self, ui: &egui::Ui) {
        let mut on_disk = config::load();
        self.apply_to(&mut on_disk);
        match config::save(&on_disk) {
            Ok(()) => {
                self.config = on_disk;
                self.saved_at = Some(Instant::now());
                ui.ctx()
                    .request_repaint_after(Duration::from_secs(SAVED_VISIBLE_SECS));
            }
            Err(e) => tracing::error!("failed to save config: {e}"),
        }
    }

    /// Renders the per-device checkboxes that drive `shown_devices`. Each checkbox
    /// toggles whether that device gets a tray icon (PerDevice) or appears in the
    /// menu (PrimaryOnly). Unchecking a duplicate (e.g. the BT copy of a mouse that
    /// is also seen over USB) removes that one icon.
    fn render_device_picker(&mut self, ui: &mut egui::Ui) {
        if self.devices.is_empty() {
            ui.label(egui::RichText::new("No devices found. Connect a device and reopen.").weak());
            return;
        }

        let all_names: Vec<String> = self.devices.iter().map(|d| d.name.clone()).collect();
        for name in &all_names {
            let mut checked = self.config.is_shown(name);
            if ui.checkbox(&mut checked, name).changed() {
                // Rebuild the checked set after this toggle.
                let mut checked_set: HashSet<String> = all_names
                    .iter()
                    .filter(|n| self.config.is_shown(n))
                    .cloned()
                    .collect();
                if checked {
                    checked_set.insert(name.clone());
                } else {
                    checked_set.remove(name.as_str());
                }
                self.config.shown_devices = shown_after_toggle(&all_names, &checked_set);
                self.persist(ui);
            }
        }
    }

    /// Renders one CollapsingHeader per discovered device with optional
    /// threshold/interval overrides. Persists only on release (drag_stopped /
    /// lost_focus) or checkbox toggle, never on every dragged pixel.
    fn render_device_overrides(&mut self, ui: &mut egui::Ui) {
        if self.devices.is_empty() {
            ui.label(egui::RichText::new("No devices found. Connect a device and reopen.").weak());
            return;
        }

        let default_threshold = self.config.low_threshold;
        let default_interval = self.config.poll_interval_secs;
        let names: Vec<String> = self.devices.iter().map(|d| d.name.clone()).collect();

        for name in &names {
            let existing = self.config.device_overrides.get(name).cloned();
            let mut threshold_on = existing.as_ref().is_some_and(|d| d.low_threshold.is_some());
            let mut threshold = existing
                .as_ref()
                .and_then(|d| d.low_threshold)
                .unwrap_or(default_threshold);
            let mut interval_on = existing
                .as_ref()
                .is_some_and(|d| d.poll_interval_secs.is_some());
            let mut interval = existing
                .as_ref()
                .and_then(|d| d.poll_interval_secs)
                .unwrap_or(default_interval);

            let mut save = false;
            egui::CollapsingHeader::new(name)
                .id_salt(name)
                .show(ui, |ui| {
                    if ui
                        .checkbox(&mut threshold_on, "Override low battery threshold")
                        .changed()
                    {
                        save = true;
                    }
                    if threshold_on {
                        let resp = ui.add(
                            egui::Slider::new(&mut threshold, LOW_THRESHOLD_RANGE).suffix("%"),
                        );
                        save |= resp.drag_stopped() || resp.lost_focus();
                    }

                    if ui
                        .checkbox(&mut interval_on, "Override poll interval")
                        .changed()
                    {
                        save = true;
                    }
                    if interval_on {
                        let resp = ui.add(
                            egui::Slider::new(&mut interval, POLL_INTERVAL_RANGE).suffix(" s"),
                        );
                        save |= resp.drag_stopped() || resp.lost_focus();
                    }
                });

            if save {
                apply_device_override(
                    &mut self.config.device_overrides,
                    name,
                    threshold_on.then_some(threshold),
                    interval_on.then_some(interval),
                    default_threshold,
                    default_interval,
                );
                self.persist(ui);
            }
        }
    }
}

impl eframe::App for SettingsApp {
    /// Called each frame; `ui` is the root central panel provided by eframe 0.34.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Apply a 16 px inner margin on all sides per the design system. We
        // replace the default CentralPanel frame with one that only changes
        // inner_margin, keeping all other visual properties from the theme.
        let frame = egui::Frame::central_panel(ui.style()).inner_margin(16.0);
        frame.show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.render_sections(ui);
            });
        });
    }
}

impl SettingsApp {
    fn render_sections(&mut self, ui: &mut egui::Ui) {
        // ── Tray display ──────────────────────────────────────────────────────
        Self::section_header(ui, "Tray display");
        for mode in DisplayMode::ALL {
            if ui
                .radio_value(&mut self.config.display_mode, mode, mode.label())
                .changed()
            {
                // Persist immediately; the tray watches the file and re-renders.
                self.persist(ui);
            }
        }

        ui.add_space(8.0);
        let mut per_device = self.config.tray_mode == TrayMode::PerDevice;
        if ui
            .checkbox(&mut per_device, "Show one icon per device")
            .changed()
        {
            self.config.tray_mode = if per_device {
                TrayMode::PerDevice
            } else {
                TrayMode::PrimaryOnly
            };
            self.persist(ui);
        }

        if per_device {
            ui.indent("tray_device_picker", |ui| {
                ui.add_space(4.0);
                ui.strong("Devices in tray");
                ui.label(egui::RichText::new("Uncheck a device to remove its tray icon.").weak());
                self.render_device_picker(ui);
            });
        }

        // ── Battery ───────────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, "Battery");

        let mut threshold = self.config.low_threshold;
        let resp = ui.add(
            egui::Slider::new(&mut threshold, LOW_THRESHOLD_RANGE)
                .text("Low battery threshold")
                .suffix("%"),
        );
        if resp.drag_stopped() || resp.lost_focus() {
            self.config.low_threshold = threshold;
            self.persist(ui);
        }

        let mut interval = self.config.poll_interval_secs;
        let resp = ui
            .add(
                egui::Slider::new(&mut interval, POLL_INTERVAL_RANGE)
                    .text("Check every")
                    .suffix(" s"),
            )
            .on_hover_text(
                "Polling more often than this wakes the device constantly and drains its battery.",
            );
        if resp.drag_stopped() || resp.lost_focus() {
            self.config.poll_interval_secs = interval;
            self.persist(ui);
        }

        ui.add_space(8.0);
        ui.strong("Per-device overrides");
        self.render_device_overrides(ui);

        // ── Notifications ─────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, "Notifications");
        if ui
            .checkbox(
                &mut self.config.notifications_enabled,
                "Low battery notifications",
            )
            .changed()
        {
            self.persist(ui);
        }

        // ── Startup ───────────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, "Startup");
        if ui
            .checkbox(&mut self.autostart_enabled, "Start with session")
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

        // ── About ─────────────────────────────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, "About");
        ui.label(format!("rigbat {}", env!("CARGO_PKG_VERSION")));
        ui.hyperlink_to("Project page", env!("CARGO_PKG_REPOSITORY"));

        // ── Bottom row: status + Close ────────────────────────────────────────
        ui.add_space(8.0);
        ui.separator();

        ui.horizontal(|ui| {
            // Show "Changes saved." for SAVED_VISIBLE_SECS after the last save.
            let status = match self.saved_at {
                Some(t) if t.elapsed() < Duration::from_secs(SAVED_VISIBLE_SECS) => {
                    "Changes saved."
                }
                _ => "",
            };
            ui.weak(status);

            // Push the Close button to the right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
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

/// Returns the `shown_devices` value after a toggle. If every device in `all`
/// is present in `checked`, returns an empty `Vec` (canonical "show all").
/// Otherwise returns only the checked names in the order they appear in `all`.
fn shown_after_toggle(all: &[String], checked: &HashSet<String>) -> Vec<String> {
    if all.iter().all(|n| checked.contains(n)) {
        Vec::new()
    } else {
        all.iter()
            .filter(|n| checked.contains(n.as_str()))
            .cloned()
            .collect()
    }
}

/// Gathers connected device infos in a throwaway tokio runtime, dropped before
/// eframe starts. Returns an empty list if the runtime or discovery fails
/// (settings must still open so the user can change other options).
fn discover_devices() -> Vec<DeviceInfo> {
    let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    rt.block_on(async {
        crate::discovery::discover_all()
            .await
            .iter()
            .map(|s| s.device().clone())
            .collect()
    })
    // rt dropped here, before eframe::run_native
}

/// Opens the settings window. Blocks until the user closes it.
pub fn run() -> anyhow::Result<()> {
    let config = config::load();
    let devices = discover_devices();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 560.0])
            .with_min_inner_size([380.0, 320.0])
            .with_title("rigbat")
            .with_app_id("rigbat"),
        ..Default::default()
    };
    eframe::run_native(
        "rigbat",
        options,
        Box::new(|cc| {
            // Follow the system light/dark preference. Confirmed safe in the runtime-less settings
            // process on COSMIC/Wayland — portal theme queries do not hit the zbus-no-runtime path.
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            Ok(Box::new(SettingsApp {
                config,
                devices,
                saved_at: None,
                autostart_enabled: autostart::is_enabled(),
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_app_with(config: Config) -> SettingsApp {
        SettingsApp {
            config,
            devices: Vec::new(),
            saved_at: None,
            autostart_enabled: false,
        }
    }

    #[test]
    fn apply_to_preserves_primary_device() {
        let app = settings_app_with(Config {
            primary_device: None,
            ..Config::default()
        });
        let mut target = Config {
            primary_device: Some("mouse".to_string()),
            ..Config::default()
        };
        app.apply_to(&mut target);
        assert_eq!(target.primary_device, Some("mouse".to_string()));
    }

    #[test]
    fn apply_to_overwrites_owned_fields() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: Some(10),
            },
        );
        let app = settings_app_with(Config {
            display_mode: DisplayMode::PercentInIcon,
            shown_devices: vec!["mouse".to_string()],
            notifications_enabled: false,
            tray_mode: TrayMode::PerDevice,
            poll_interval_secs: 45,
            low_threshold: 15,
            device_overrides: overrides.clone(),
            ..Config::default()
        });
        let mut target = Config::default();
        app.apply_to(&mut target);
        assert_eq!(target.display_mode, DisplayMode::PercentInIcon);
        assert_eq!(target.tray_mode, TrayMode::PerDevice);
        assert_eq!(target.shown_devices, vec!["mouse".to_string()]);
        assert!(!target.notifications_enabled);
        assert_eq!(target.poll_interval_secs, 45);
        assert_eq!(target.low_threshold, 15);
        assert_eq!(target.device_overrides, overrides);
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
    fn shown_after_toggle_all_checked_collapses_to_empty() {
        let all = vec!["mouse".to_string(), "keyboard".to_string()];
        let checked: HashSet<String> = all.iter().cloned().collect();
        assert!(shown_after_toggle(&all, &checked).is_empty());
    }

    #[test]
    fn shown_after_toggle_partial_returns_checked_names() {
        let all = vec![
            "mouse".to_string(),
            "keyboard".to_string(),
            "headset".to_string(),
        ];
        let checked: HashSet<String> = ["mouse".to_string(), "headset".to_string()]
            .into_iter()
            .collect();
        let result = shown_after_toggle(&all, &checked);
        assert_eq!(result, vec!["mouse".to_string(), "headset".to_string()]);
    }

    #[test]
    fn shown_after_toggle_none_checked_returns_empty_vec() {
        let all = vec!["mouse".to_string(), "keyboard".to_string()];
        let checked: HashSet<String> = HashSet::new();
        let result = shown_after_toggle(&all, &checked);
        assert!(result.is_empty());
    }

    #[test]
    fn shown_after_toggle_preserves_order_from_all() {
        let all = vec![
            "mouse".to_string(),
            "keyboard".to_string(),
            "headset".to_string(),
        ];
        // checked in reverse insertion order — result must follow `all` order
        let checked: HashSet<String> = ["headset".to_string(), "mouse".to_string()]
            .into_iter()
            .collect();
        let result = shown_after_toggle(&all, &checked);
        assert_eq!(result, vec!["mouse".to_string(), "headset".to_string()]);
    }
}
