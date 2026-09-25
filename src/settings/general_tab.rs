use eframe::egui;

use super::{LOW_THRESHOLD_RANGE, SettingsApp, widgets};
use crate::autostart;
use crate::domain::{DisplayMode, PrimaryStatus, TrayMode};
use crate::i18n::{self, Lang, fl, loader};
use crate::icon::{IconRenderer, Theme, TinySkiaRenderer};

/// The General tab's poll-interval choices, seconds.
const POLL_INTERVAL_PRESETS: [u64; 7] = [30, 60, 120, 300, 900, 1800, 3600];

/// Edge of an icon-style preview, points.
const STYLE_PREVIEW_SIZE: f32 = 32.0;

/// The largest icon `TinySkiaRenderer` is drawn for.
const STYLE_PREVIEW_MAX_PIXELS: u32 = 64;

/// The one reading every icon-style preview shows.
const STYLE_PREVIEW_STATUS: PrimaryStatus = PrimaryStatus::Ok { percent: 72 };

/// The low-battery threshold's slider and value box, points.
const THRESHOLD_CONTROL_WIDTH: f32 = 240.0;
const THRESHOLD_SLIDER_WIDTH: f32 = 180.0;

/// The icon-style tiles' pictures, rendered once per theme and pixel density.
pub(super) struct StylePreviews {
    dark: bool,
    pixels: u32,
    textures: Vec<(DisplayMode, egui::TextureHandle)>,
}

impl SettingsApp {
    /// A centred column of preference groups, scrolling as a whole.
    pub(super) fn render_general_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width().min(widgets::CONTENT_MAX_WIDTH);
                let margin = (ui.available_width() - width) / 2.0 - ui.spacing().item_spacing.x;
                ui.horizontal(|ui| {
                    ui.add_space(margin.max(0.0));
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        ui.add_space(8.0);
                        self.render_tray_group(ui);
                        self.render_battery_group(ui);
                        self.render_system_group(ui);
                        let l = loader(self.config.lang());
                        widgets::footer(
                            ui,
                            &format!("rigbat {} ·", env!("CARGO_PKG_VERSION")),
                            &fl!(l, "about-project-page"),
                            env!("CARGO_PKG_REPOSITORY"),
                        );
                        ui.add_space(8.0);
                    });
                });
            });
    }

    fn render_tray_group(&mut self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let l = loader(lang);
        widgets::group(ui, &fl!(l, "group-tray"), None, |rows| {
            rows.block(&fl!(l, "tray-icon-style"), |ui| self.render_style_tiles(ui));

            // Bound to a local copy, not to `self.config`: `persist` adopts
            // the saved config only when the write succeeds, so a failed save
            // leaves `self.config` as it was and the next frame redraws the
            // real value.
            let mut per_device = self.config.tray_mode == TrayMode::PerDevice;
            let hint = if per_device {
                fl!(l, "tray-per-device-hint")
            } else {
                aggregate_icon_hint(self.config.primary_device.as_deref(), lang)
            };
            // The pin is set from a device's row, so a pin naming a device the
            // Devices tab has no row for — one retired before the inventory
            // existed, or deleted since — would be unreachable without this.
            // Clearing is the only action that needs no row, which is why it
            // is the only one that lives here.
            let pinned = !per_device && self.config.primary_device.is_some();
            let mut clear_pin = false;
            let title = fl!(l, "tray-per-device");
            let toggled = rows.row(
                &title,
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(widgets::secondary(ui, &hint));
                        if pinned {
                            clear_pin = ui.small_button(fl!(l, "button-clear")).clicked();
                        }
                    });
                },
                |ui| {
                    widgets::switch(ui, switch_id("tray-per-device"), &mut per_device, &title)
                        .changed()
                },
            );
            if toggled {
                let tray_mode = if per_device {
                    TrayMode::PerDevice
                } else {
                    TrayMode::PrimaryOnly
                };
                self.persist(move |target| target.tray_mode = tray_mode);
            }
            if clear_pin {
                self.persist(|target| target.primary_device = None);
            }
        });
    }

    /// One tile per `DisplayMode`, each showing the same made-up reading.
    fn render_style_tiles(&mut self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let current = self.config.display_mode;
        let previews = self.style_previews(ui.ctx(), ui.visuals().dark_mode);
        let width = widgets::tile_width(ui.available_width(), previews.len());
        let mut chosen = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = widgets::TILE_GAP;
            for (mode, texture) in &previews {
                let caption = mode.label(lang);
                let selected = *mode == current;
                if widgets::tile(ui, width, selected, texture, STYLE_PREVIEW_SIZE, &caption)
                    .clicked()
                {
                    chosen = Some(*mode);
                }
            }
        });
        if let Some(mode) = chosen.filter(|&mode| mode != current) {
            // The tray watches the file and re-renders.
            self.persist(move |target| target.display_mode = mode);
        }
    }

    fn style_previews(
        &mut self,
        ctx: &egui::Context,
        dark: bool,
    ) -> Vec<(DisplayMode, egui::TextureHandle)> {
        let pixels = ((STYLE_PREVIEW_SIZE * ctx.pixels_per_point()).round() as u32)
            .min(STYLE_PREVIEW_MAX_PIXELS);
        let fresh = self
            .style_previews
            .as_ref()
            .is_some_and(|p| p.dark == dark && p.pixels == pixels);
        if !fresh {
            self.style_previews = Some(StylePreviews {
                dark,
                pixels,
                textures: render_style_previews(ctx, dark, pixels),
            });
        }
        self.style_previews
            .as_ref()
            .map(|p| p.textures.clone())
            .unwrap_or_default()
    }

    fn render_battery_group(&mut self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let l = loader(lang);
        widgets::group(
            ui,
            &fl!(l, "group-battery"),
            Some(&fl!(l, "defaults-hint")),
            |rows| {
                let mut threshold = self.config.low_threshold;
                let resp = rows.row(&fl!(l, "default-low-threshold"), widgets::none, |ui| {
                    widgets::trailing(ui, THRESHOLD_CONTROL_WIDTH, |ui| {
                        ui.spacing_mut().slider_width = THRESHOLD_SLIDER_WIDTH;
                        ui.add(egui::Slider::new(&mut threshold, LOW_THRESHOLD_RANGE).suffix("%"))
                    })
                });
                if resp.drag_stopped() || resp.lost_focus() {
                    self.persist(move |target| target.low_threshold = threshold);
                }

                let current = self.config.poll_interval_secs;
                let mut interval = current;
                let hint = fl!(l, "poll-interval-hint");
                let changed = rows.row(
                    &fl!(l, "default-poll-interval"),
                    widgets::subtitle(&hint),
                    |ui| {
                        let mut changed = false;
                        egui::ComboBox::from_id_salt("poll-interval")
                            .selected_text(interval_label(current, lang))
                            .show_ui(ui, |ui| {
                                for secs in interval_choices(current) {
                                    changed |= ui
                                        .selectable_value(
                                            &mut interval,
                                            secs,
                                            interval_label(secs, lang),
                                        )
                                        .changed();
                                }
                            });
                        changed
                    },
                );
                if changed && interval != current {
                    self.persist(move |target| target.poll_interval_secs = interval);
                }

                let mut notifications_enabled = self.config.notifications_enabled;
                let title = fl!(l, "notifications-enabled");
                let toggled = rows.row(&title, widgets::none, |ui| {
                    widgets::switch(
                        ui,
                        switch_id("notifications-enabled"),
                        &mut notifications_enabled,
                        &title,
                    )
                    .changed()
                });
                if toggled {
                    self.persist(move |target| {
                        target.notifications_enabled = notifications_enabled;
                    });
                }
            },
        );
    }

    fn render_system_group(&mut self, ui: &mut egui::Ui) {
        let l = loader(self.config.lang());
        widgets::group(ui, &fl!(l, "group-system"), None, |rows| {
            self.render_autostart_row(rows);
            self.render_language_row(rows);
        });
    }

    /// With `rigbat.service` enabled, a second launch path would start a
    /// second tray, so the switch only reports that the service starts it.
    fn render_autostart_row(&mut self, rows: &mut widgets::Rows<'_>) {
        let l = loader(self.config.lang());
        let title = fl!(l, "autostart-enabled");
        let id = switch_id("autostart-enabled");
        if self.systemd_service_enabled {
            let how_to_disable = fl!(l, "autostart-systemd-disable");
            let managed = fl!(l, "autostart-managed-by-systemd");
            rows.row(
                &title,
                |ui| {
                    ui.label(widgets::secondary(ui, &managed))
                        .on_hover_text(&how_to_disable);
                },
                |ui| {
                    ui.add_enabled_ui(false, |ui| {
                        widgets::switch(ui, id, &mut true, &title)
                            .on_disabled_hover_text(&how_to_disable);
                    });
                },
            );
            return;
        }
        let toggled = rows.row(&title, widgets::none, |ui| {
            widgets::switch(ui, id, &mut self.autostart_enabled, &title).changed()
        });
        if toggled && let Err(e) = autostart::set_enabled(self.autostart_enabled) {
            tracing::warn!("failed to update autostart: {e}");
            // Revert the switch so it reflects the real filesystem state.
            self.autostart_enabled = !self.autostart_enabled;
        }
    }

    fn render_language_row(&mut self, rows: &mut widgets::Rows<'_>) {
        let l = loader(self.config.lang());
        let mut choice = self.config.language.as_deref().and_then(Lang::from_tag);
        let system = format!(
            "{} ({})",
            fl!(l, "language-system"),
            i18n::system().native_name()
        );
        let selected = choice.map_or_else(|| system.clone(), |lang| lang.native_name().to_owned());
        let changed = rows.row(&fl!(l, "section-language"), widgets::none, |ui| {
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
            changed
        });
        if changed {
            self.persist(move |target| {
                target.language = choice.map(|lang| lang.tag().to_owned());
            });
        }
    }
}

/// Global so a test can find the switch it clicks.
fn switch_id(key: &str) -> egui::Id {
    egui::Id::new(("settings-switch", key))
}

/// Tray icons for `STYLE_PREVIEW_STATUS` in every `DisplayMode`, in the
/// `dark` or light theme, without a device-type corner glyph.
fn render_style_previews(
    ctx: &egui::Context,
    dark: bool,
    pixels: u32,
) -> Vec<(DisplayMode, egui::TextureHandle)> {
    let renderer = TinySkiaRenderer {
        sizes: vec![pixels],
    };
    let theme = if dark { Theme::dark() } else { Theme::light() };
    DisplayMode::ALL
        .into_iter()
        .filter_map(|mode| {
            let icon = renderer
                .render(STYLE_PREVIEW_STATUS, None, &theme, mode, false)
                .into_iter()
                .next()?;
            let size = [
                usize::try_from(icon.width).ok()?,
                usize::try_from(icon.height).ok()?,
            ];
            let rgba: Vec<u8> = icon
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[a, r, g, b]| [r, g, b, a])
                .collect();
            let image = egui::ColorImage::from_rgba_premultiplied(size, &rgba);
            let name = format!("style-preview-{mode:?}");
            Some((
                mode,
                ctx.load_texture(name, image, egui::TextureOptions::LINEAR),
            ))
        })
        .collect()
}

/// A poll interval as the General tab names it: whole hours, whole minutes,
/// else seconds.
fn interval_label(secs: u64, lang: Lang) -> String {
    let l = loader(lang);
    let (hours, minutes) = (secs / 3600, secs / 60);
    if hours > 0 && secs.is_multiple_of(3600) {
        fl!(l, "interval-hours", count = hours)
    } else if minutes > 0 && secs.is_multiple_of(60) {
        fl!(l, "interval-minutes", count = minutes)
    } else {
        fl!(l, "interval-seconds", count = secs)
    }
}

/// The presets, plus `current` when it is not one of them, so a value set
/// by hand in `config.json` stays selectable instead of being replaced.
fn interval_choices(current: u64) -> Vec<u64> {
    let mut choices = POLL_INTERVAL_PRESETS.to_vec();
    if !choices.contains(&current) {
        choices.push(current);
        choices.sort_unstable();
    }
    choices
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{self, Config};
    use crate::egui_test::{
        assert_no_overlap, click_at, fully_painted_text_at, painted_text_at, run_frame,
    };
    use crate::settings::WINDOW_MIN_SIZE;
    use crate::settings::tests::{scratch_config_path, settings_app_with};

    /// The narrowest the window gets, tall enough that the General tab's
    /// column does not scroll: its rows are checked, not the scroll area.
    const GENERAL_TAB_TEST_SIZE: [f32; 2] = [WINDOW_MIN_SIZE[0], 1000.0];

    #[test]
    fn general_tab_offers_every_language() {
        let mut app = settings_app_with(Config {
            language: Some("ru".to_owned()),
            ..Config::default()
        });

        let painted = painted_text_at(GENERAL_TAB_TEST_SIZE, |ui| app.render_general_tab(ui));

        assert!(
            painted.iter().any(|t| t == "Язык / Language"),
            "{painted:?}"
        );
        assert!(painted.iter().any(|t| t == "Русский"), "{painted:?}");
    }

    /// Every title and value on the tab, in both tray modes and with the
    /// autostart switch both free and managed by systemd, painted whole on one
    /// line and clear of every other string.
    #[test]
    fn general_tab_text_is_whole_on_one_line_in_every_language() {
        for lang in Lang::ALL {
            for per_device in [false, true] {
                let mut app = settings_app_with(Config {
                    language: Some(lang.tag().to_owned()),
                    tray_mode: if per_device {
                        TrayMode::PerDevice
                    } else {
                        TrayMode::PrimaryOnly
                    },
                    primary_device: Some("SteelSeries Aerox 5 Wireless".to_owned()),
                    ..Config::default()
                });
                app.systemd_service_enabled = per_device;

                let painted = fully_painted_text_at(GENERAL_TAB_TEST_SIZE, |ui| {
                    app.render_tab_bar(ui);
                    app.render_general_tab(ui);
                });

                let l = loader(lang);
                let mut expected: Vec<String> = [
                    "tab-general",
                    "tab-devices",
                    "group-tray",
                    "tray-icon-style",
                    "tray-per-device",
                    "group-battery",
                    "default-low-threshold",
                    "default-poll-interval",
                    "notifications-enabled",
                    "group-system",
                    "autostart-enabled",
                    "section-language",
                    "about-project-page",
                ]
                .into_iter()
                .map(|id| l.get(id))
                .collect();
                expected.extend(DisplayMode::ALL.map(|mode| mode.label(lang)));
                expected.extend(["20", "%"].map(str::to_owned));
                expected.push(interval_label(Config::default().poll_interval_secs, lang));
                expected.push(lang.native_name().to_owned());
                expected.push(format!("rigbat {} ·", env!("CARGO_PKG_VERSION")));
                expected.push(if per_device {
                    l.get("autostart-managed-by-systemd")
                } else {
                    l.get("button-clear")
                });
                for text in &expected {
                    let lines = painted.iter().find(|p| &p.text == text).map(|p| p.lines);
                    assert_eq!(
                        lines,
                        Some(1),
                        "{lang:?}: {text:?} is cut off, missing or wrapped: {painted:?}"
                    );
                }
                assert_no_overlap(&painted);
            }
        }
    }

    #[test]
    fn general_tab_column_is_centred_and_capped() {
        let mut app = settings_app_with(Config::default());

        let painted = fully_painted_text_at(GENERAL_TAB_TEST_SIZE, |ui| app.render_general_tab(ui));

        let title = painted
            .iter()
            .find(|p| p.text == "Low battery threshold")
            .expect("row title painted");
        let value = painted
            .iter()
            .find(|p| p.text == "%")
            .expect("slider value painted");
        let margin = (GENERAL_TAB_TEST_SIZE[0] - widgets::CONTENT_MAX_WIDTH) / 2.0;
        assert!(title.rect.left() > margin, "{:?}", title.rect);
        assert!(
            value.rect.right() < GENERAL_TAB_TEST_SIZE[0] - margin,
            "{:?}",
            value.rect
        );
    }

    /// A fresh settings window over a scratch config file, so a click saves
    /// somewhere other than `~/.config/rigbat/config.json`.
    fn app_saving_to(test_name: &str, config: Config) -> (SettingsApp, PathBuf) {
        let path = scratch_config_path(test_name);
        config::save_to(&path, &config).unwrap();
        let mut app = settings_app_with(config::load_from(&path));
        app.config_path = Some(path.clone());
        (app, path)
    }

    #[test]
    fn clicking_the_per_device_switch_saves_the_tray_mode() {
        let (mut app, path) = app_saving_to("per-device-switch", Config::default());
        let ctx = egui::Context::default();
        let size = GENERAL_TAB_TEST_SIZE;
        run_frame(&ctx, size, Vec::new(), |ui| app.render_general_tab(ui));
        let switch = ctx
            .read_response(switch_id("tray-per-device"))
            .expect("the switch was laid out");

        click_at(&ctx, size, switch.rect.center(), |ui| {
            app.render_general_tab(ui)
        });

        assert_eq!(config::load_from(&path).tray_mode, TrayMode::PerDevice);
        assert_eq!(app.config.tray_mode, TrayMode::PerDevice);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Focus and Space are enough to flip a switch, and the switch reports
    /// itself as a labelled checkbox to accessibility tools.
    #[test]
    fn a_switch_toggles_from_the_keyboard_and_carries_its_label() {
        let (mut app, path) = app_saving_to("notifications-switch", Config::default());
        let ctx = egui::Context::default();
        let size = GENERAL_TAB_TEST_SIZE;
        run_frame(&ctx, size, Vec::new(), |ui| app.render_general_tab(ui));
        ctx.memory_mut(|m| m.request_focus(switch_id("notifications-enabled")));
        run_frame(&ctx, size, Vec::new(), |ui| app.render_general_tab(ui));

        let space = egui::Event::Key {
            key: egui::Key::Space,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        let output = run_frame(&ctx, size, vec![space], |ui| app.render_general_tab(ui));

        assert!(!config::load_from(&path).notifications_enabled);
        let announced = output.platform_output.events.iter().any(|event| {
            let info = event.widget_info();
            info.typ == egui::WidgetType::Checkbox
                && info.label.as_deref() == Some("Low battery notifications")
                && info.selected == Some(false)
        });
        assert!(announced, "{:?}", output.platform_output.events);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn interval_labels_use_whole_units() {
        let en: Vec<_> = [30, 45, 60, 90, 120, 900, 3600, 7200]
            .map(|secs| interval_label(secs, Lang::En))
            .into();
        assert_eq!(
            en,
            [
                "30 s", "45 s", "1 min", "90 s", "2 min", "15 min", "1 h", "2 h"
            ]
        );
        assert_eq!(interval_label(300, Lang::Ru), "5 мин");
        assert_eq!(interval_label(3600, Lang::Ru), "1 ч");
    }

    #[test]
    fn interval_choices_are_the_presets_in_seconds() {
        assert_eq!(
            interval_choices(60),
            [30, 60, 120, 300, 900, 1800, 3600].to_vec()
        );
    }

    /// A value set by hand in `config.json` is offered as its own entry and
    /// shown as the current one, not replaced by the nearest preset.
    #[test]
    fn an_interval_that_is_no_preset_survives() {
        assert_eq!(
            interval_choices(45),
            [30, 45, 60, 120, 300, 900, 1800, 3600].to_vec()
        );

        let mut app = settings_app_with(Config {
            poll_interval_secs: 45,
            ..Config::default()
        });
        let painted = fully_painted_text_at(GENERAL_TAB_TEST_SIZE, |ui| app.render_general_tab(ui));

        assert!(painted.iter().any(|p| p.text == "45 s"), "{painted:?}");
        assert_eq!(app.config.poll_interval_secs, 45);
    }

    #[test]
    fn style_previews_cover_every_display_mode() {
        let ctx = egui::Context::default();

        let previews = render_style_previews(&ctx, true, STYLE_PREVIEW_MAX_PIXELS);

        let modes: Vec<_> = previews.iter().map(|(mode, _)| *mode).collect();
        assert_eq!(modes, DisplayMode::ALL.to_vec());
        for (mode, texture) in &previews {
            assert_eq!(texture.size(), [64, 64], "{mode:?}");
        }
    }

    #[test]
    fn aggregate_icon_hint_names_the_pinned_device() {
        assert!(aggregate_icon_hint(Some("MX Anywhere 3"), Lang::En).contains("MX Anywhere 3"));
    }

    #[test]
    fn aggregate_icon_hint_describes_the_automatic_choice() {
        assert!(aggregate_icon_hint(None, Lang::En).contains("lowest charge"));
    }
}
