use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};
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
    scan_rx: Option<mpsc::Receiver<Vec<DeviceInfo>>>,
    /// True from the moment a scan is spawned until its result is applied.
    scanning: bool,
}

impl SettingsApp {
    /// Renders one section header: bold label followed by a small gap.
    fn section_header(ui: &mut egui::Ui, title: &str) {
        ui.strong(title);
        ui.add_space(4.0);
    }

    /// Copies the fields this window owns onto `target`, leaving everything
    /// else untouched. `primary_device` is deliberately not one of them: no UI
    /// sets it, and the tray resolves the aggregate icon without it when it is
    /// `None`.
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
    /// owns into it, so a hand-edited `primary_device` (or any future field this
    /// window does not display) survives a save from a stale in-memory snapshot.
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

    /// Spawns one discovery pass on `rt` if none is already in flight, wiring
    /// its result to a fresh channel and waking `egui_ctx` when it lands so the
    /// window updates without waiting for the next input event.
    fn spawn_scan(&mut self, egui_ctx: egui::Context) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        let ctx = Arc::clone(&self.discovery_ctx);
        self.rt.spawn(async move {
            let devices: Vec<DeviceInfo> = crate::discovery::discover_all(&ctx)
                .await
                .iter()
                .map(|s| s.device().clone())
                .collect();
            // The receiver is dropped if the window closed mid-scan; ignore that.
            let _ = tx.send(devices);
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
            Ok(devices) => self.apply_scan_result(devices),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.scanning = false;
                self.scan_rx = None;
            }
        }
    }

    /// Applies a freshly completed scan's device list. Only `self.devices`
    /// changes: `shown_devices` and `device_overrides` are keyed by device
    /// name and are left exactly as the user set them, whether or not the
    /// device set changed since the previous scan.
    fn apply_scan_result(&mut self, devices: Vec<DeviceInfo>) {
        self.devices = devices;
        self.scanning = false;
        self.scan_rx = None;
    }

    /// Renders a "Rescan" button, disabled and relabeled while a scan is
    /// already in flight.
    fn render_rescan_button(&mut self, ui: &mut egui::Ui) {
        let egui_ctx = ui.ctx().clone();
        ui.add_enabled_ui(!self.scanning, |ui| {
            let label = if self.scanning {
                "Scanning…"
            } else {
                "Rescan"
            };
            if ui.button(label).clicked() {
                self.spawn_scan(egui_ctx.clone());
            }
        });
    }

    /// Shared empty-state message for both device sections below, pointing at
    /// the Rescan button instead of telling the user to reopen the window.
    fn render_device_empty_state(ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("No devices found. Connect a device, then press Rescan.").weak(),
        );
    }

    /// Renders the per-device checkboxes that drive `shown_devices`. Each checkbox
    /// toggles whether that device gets a tray icon (PerDevice) or appears in the
    /// menu (PrimaryOnly). Unchecking a duplicate (e.g. the BT copy of a mouse that
    /// is also seen over USB) removes that one icon.
    fn render_device_picker(&mut self, ui: &mut egui::Ui) {
        if self.devices.is_empty() {
            Self::render_device_empty_state(ui);
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
            Self::render_device_empty_state(ui);
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
        self.poll_scan();

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
                ui.horizontal(|ui| {
                    ui.strong("Devices in tray");
                    self.render_rescan_button(ui);
                });
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
        ui.horizontal(|ui| {
            ui.strong("Per-device overrides");
            self.render_rescan_button(ui);
        });
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
        if self.systemd_service_enabled {
            ui.add_enabled_ui(false, |ui| {
                ui.checkbox(&mut self.autostart_enabled, "Start with session");
            });
            ui.label(
                egui::RichText::new(
                    "Managed by the systemd user service. Disable it with: \
                     systemctl --user disable --now rigbat.service",
                )
                .weak(),
            );
        } else if ui
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

/// Opens the settings window. Blocks until the user closes it.
///
/// Keeps a tokio runtime alive for the life of the window (unlike the old
/// discover-once-and-drop approach) so devices that connect after the window
/// opens still show up: a scan is spawned on it at startup and again on
/// every "Rescan" click, never entered blockingly from `ui()`.
pub fn run() -> anyhow::Result<()> {
    use anyhow::Context as _;

    let config = config::load();
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building the tokio runtime for device discovery")?,
    );
    let discovery_ctx = Arc::new(crate::discovery::Context::new());
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

    fn settings_app_with(config: Config) -> SettingsApp {
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

    #[test]
    fn apply_scan_result_preserves_shown_devices_and_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: Some(10),
            },
        );
        let mut app = settings_app_with(Config {
            shown_devices: vec!["mouse".to_string()],
            device_overrides: overrides.clone(),
            ..Config::default()
        });
        app.apply_scan_result(vec![device("mouse"), device("keyboard")]);
        assert_eq!(app.config.shown_devices, vec!["mouse".to_string()]);
        assert_eq!(app.config.device_overrides, overrides);
        assert!(!app.scanning);
        assert!(app.scan_rx.is_none());
    }

    #[test]
    fn apply_scan_result_empty_means_all_survives_device_set_change() {
        // shown_devices == [] is the canonical "show all"; it must not flip
        // to a concrete list just because the discovered set changed.
        let mut app = settings_app_with(Config::default());
        app.apply_scan_result(vec![device("mouse")]);
        assert!(app.config.shown_devices.is_empty());
        app.apply_scan_result(vec![device("mouse"), device("keyboard")]);
        assert!(app.config.shown_devices.is_empty());
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
        app.apply_scan_result(vec![device("mouse")]);
        assert_eq!(app.config.device_overrides, overrides);
        assert!(!app.devices.iter().any(|d| d.name == "headset"));
    }
}
