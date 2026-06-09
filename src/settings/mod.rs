use std::time::{Duration, Instant};

use eframe::egui;

use crate::config::{self, Config, DisplayMode};

/// Seconds the "Changes saved." status line remains visible after a save.
const SAVED_VISIBLE_SECS: u64 = 2;

struct SettingsApp {
    config: Config,
    /// Set to `Some(Instant::now())` on every successful save; cleared implicitly
    /// by comparing elapsed time on each frame.
    saved_at: Option<Instant>,
}

impl SettingsApp {
    /// Renders one section header: bold label followed by a small gap.
    fn section_header(ui: &mut egui::Ui, title: &str) {
        ui.strong(title);
        ui.add_space(4.0);
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
                if let Err(e) = config::save(&self.config) {
                    eprintln!("rigbat settings: failed to save config: {e}");
                } else {
                    self.saved_at = Some(Instant::now());
                    // Ensure the frame repaints after 2 s so the status text clears.
                    ui.ctx()
                        .request_repaint_after(Duration::from_secs(SAVED_VISIBLE_SECS));
                }
            }
        }

        // ── Visible devices (placeholder — D2 makes it functional) ────────────
        ui.add_space(16.0);
        Self::section_header(ui, "Visible devices");
        ui.label(egui::RichText::new("Device selection arrives with multiple tray icons.").weak());

        // ── Notifications (disabled placeholder) ─────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, "Notifications");
        // Throwaway local — intentionally not stored in Config (coming soon).
        let mut _notify_placeholder = false;
        ui.add_enabled(
            false,
            egui::Checkbox::new(&mut _notify_placeholder, "Low battery notifications"),
        );
        ui.label(egui::RichText::new("(coming soon)").weak());

        // ── Startup (disabled placeholder) ────────────────────────────────────
        ui.add_space(16.0);
        Self::section_header(ui, "Startup");
        // Throwaway local — intentionally not stored in Config (coming soon).
        let mut _startup_placeholder = false;
        ui.add_enabled(
            false,
            egui::Checkbox::new(&mut _startup_placeholder, "Start with session"),
        );
        ui.label(egui::RichText::new("(coming soon)").weak());

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

/// Opens the settings window. Blocks until the user closes it.
pub fn run() -> anyhow::Result<()> {
    let config = config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 400.0])
            .with_min_inner_size([360.0, 240.0])
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
                saved_at: None,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
