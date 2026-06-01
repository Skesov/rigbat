use eframe::egui;

use crate::config::{self, Config, DisplayMode};

struct SettingsApp {
    config: Config,
}

impl eframe::App for SettingsApp {
    /// Called each frame; `ui` is the root central panel provided by eframe 0.34.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.heading("rigbat settings");
        ui.add_space(8.0);
        ui.label("Show in tray as:");
        for mode in DisplayMode::ALL {
            if ui
                .radio_value(&mut self.config.display_mode, mode, mode.label())
                .changed()
            {
                // Persist immediately; the tray watches the file and re-renders.
                if let Err(e) = config::save(&self.config) {
                    eprintln!("rigbat settings: failed to save config: {e}");
                }
            }
        }
    }
}

/// Opens the settings window. Blocks until the user closes it.
pub fn run() -> anyhow::Result<()> {
    let config = config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([360.0, 240.0])
            .with_title("rigbat settings")
            .with_app_id("rigbat"),
        ..Default::default()
    };
    eframe::run_native(
        "rigbat settings",
        options,
        Box::new(|_cc| Ok(Box::new(SettingsApp { config }))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
