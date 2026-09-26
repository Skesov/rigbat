use eframe::egui;

use super::{SettingsApp, widgets};
use crate::appearance::ColorScheme;
use crate::domain::{DisplayMode, Palette, PrimaryStatus, WindowTheme};
use crate::i18n::{fl, loader};
use crate::icon::{IconRenderer, Theme, TinySkiaRenderer};
use crate::palette::{self, Rgb, Swatches};

/// Edge of an icon-style preview, points.
const STYLE_PREVIEW_SIZE: f32 = 32.0;

/// The largest icon `TinySkiaRenderer` is drawn for.
const STYLE_PREVIEW_MAX_PIXELS: u32 = 64;

/// Edge of a palette's swatch picture, points and image pixels.
const SWATCH_PREVIEW_SIZE: f32 = 32.0;
const SWATCH_PIXELS: usize = 16;
/// A 2 × 2 grid of cells, `SWATCH_GAP` of background around and between them.
const SWATCH_CELL: usize = 5;
const SWATCH_GAP: usize = 2;

/// The one reading every icon-style preview shows.
const STYLE_PREVIEW_STATUS: PrimaryStatus = PrimaryStatus::Ok { percent: 72 };

/// The icon-style tiles' pictures, rendered once per scheme, palette and pixel density.
pub(super) struct StylePreviews {
    dark: bool,
    palette: Palette,
    pixels: u32,
    textures: Vec<(DisplayMode, egui::TextureHandle)>,
}

/// The palette tiles' pictures, rendered once per scheme.
pub(super) struct PaletteSwatches {
    dark: bool,
    textures: Vec<(Palette, egui::TextureHandle)>,
}

impl SettingsApp {
    pub(super) fn render_appearance_tab(&mut self, ui: &mut egui::Ui) {
        widgets::page(ui, "appearance-tab", |ui| {
            self.render_windows_group(ui);
            self.render_colours_group(ui);
            self.render_tray_icon_group(ui);
        });
    }

    fn render_windows_group(&mut self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let l = loader(lang);
        let current = self.config.theme;
        widgets::group(ui, &fl!(l, "group-windows"), None, |rows| {
            let hint = fl!(l, "window-theme-hint");
            let chosen = rows.row(&fl!(l, "window-theme"), widgets::subtitle(&hint), |ui| {
                let mut theme = current;
                egui::ComboBox::from_id_salt("window-theme")
                    .selected_text(current.label(lang))
                    .show_ui(ui, |ui| {
                        for option in WindowTheme::ALL {
                            ui.selectable_value(&mut theme, option, option.label(lang));
                        }
                    });
                (theme != current).then_some(theme)
            });
            if let Some(theme) = chosen {
                self.persist(move |target| target.theme = theme);
            }
        });
    }

    fn render_colours_group(&mut self, ui: &mut egui::Ui) {
        let l = loader(self.config.lang());
        let hint = fl!(l, "appearance-palette-hint");
        widgets::group(ui, &fl!(l, "group-colours"), Some(&hint), |rows| {
            rows.block(&fl!(l, "appearance-palette"), |ui| {
                self.render_palette_tiles(ui);
            });
        });
    }

    fn render_tray_icon_group(&mut self, ui: &mut egui::Ui) {
        let l = loader(self.config.lang());
        widgets::group(ui, &fl!(l, "group-tray-icon"), None, |rows| {
            rows.block(&fl!(l, "tray-icon-style"), |ui| self.render_style_tiles(ui));
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
        let palette = self.config.palette;
        let fresh = self
            .style_previews
            .as_ref()
            .is_some_and(|p| p.dark == dark && p.palette == palette && p.pixels == pixels);
        if !fresh {
            self.style_previews = Some(StylePreviews {
                dark,
                palette,
                pixels,
                textures: render_style_previews(ctx, scheme(dark), palette, pixels),
            });
        }
        self.style_previews
            .as_ref()
            .map(|p| p.textures.clone())
            .unwrap_or_default()
    }

    /// One tile per `Palette`, each showing its colours on its own background.
    fn render_palette_tiles(&mut self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let current = self.config.palette;
        let swatches = self.palette_swatches(ui.ctx(), ui.visuals().dark_mode);
        let width = widgets::tile_width(ui.available_width(), swatches.len());
        let mut chosen = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = widgets::TILE_GAP;
            for (palette, texture) in &swatches {
                let caption = palette.label(lang);
                let selected = *palette == current;
                if widgets::tile(ui, width, selected, texture, SWATCH_PREVIEW_SIZE, &caption)
                    .clicked()
                {
                    chosen = Some(*palette);
                }
            }
        });
        if let Some(palette) = chosen.filter(|&palette| palette != current) {
            self.persist(move |target| target.palette = palette);
        }
    }

    fn palette_swatches(
        &mut self,
        ctx: &egui::Context,
        dark: bool,
    ) -> Vec<(Palette, egui::TextureHandle)> {
        if self
            .palette_swatches
            .as_ref()
            .is_none_or(|p| p.dark != dark)
        {
            self.palette_swatches = Some(PaletteSwatches {
                dark,
                textures: render_palette_swatches(ctx, scheme(dark)),
            });
        }
        self.palette_swatches
            .as_ref()
            .map(|p| p.textures.clone())
            .unwrap_or_default()
    }
}

fn scheme(dark: bool) -> ColorScheme {
    if dark {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    }
}

/// Tray icons for `STYLE_PREVIEW_STATUS` in every `DisplayMode`, in the
/// palette's colours, without a device-type corner glyph.
fn render_style_previews(
    ctx: &egui::Context,
    scheme: ColorScheme,
    palette: Palette,
    pixels: u32,
) -> Vec<(DisplayMode, egui::TextureHandle)> {
    let renderer = TinySkiaRenderer {
        sizes: vec![pixels],
    };
    let theme = Theme::new(palette, scheme);
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

/// Every palette's swatch picture for `scheme`.
fn render_palette_swatches(
    ctx: &egui::Context,
    scheme: ColorScheme,
) -> Vec<(Palette, egui::TextureHandle)> {
    Palette::ALL
        .into_iter()
        .map(|palette| {
            let image = swatch_image(&palette::swatches(palette, scheme));
            let name = format!("palette-swatch-{palette:?}");
            let texture = ctx.load_texture(name, image, egui::TextureOptions::NEAREST);
            (palette, texture)
        })
        .collect()
}

/// The foreground, charging, warning and low colours as a 2 × 2 grid on the background.
fn swatch_image(s: &Swatches) -> egui::ColorImage {
    let color = |[r, g, b]: Rgb| egui::Color32::from_rgb(r, g, b);
    let cells = [s.fg, s.charging, s.warn, s.low];
    let cell = |v: usize| {
        (0..2).find(|i| {
            let start = SWATCH_GAP + i * (SWATCH_CELL + SWATCH_GAP);
            (start..start + SWATCH_CELL).contains(&v)
        })
    };
    let pixels = (0..SWATCH_PIXELS * SWATCH_PIXELS)
        .map(
            |i| match (cell(i / SWATCH_PIXELS), cell(i % SWATCH_PIXELS)) {
                (Some(row), Some(col)) => {
                    cells.get(row * 2 + col).copied().map_or(color(s.bg), color)
                }
                _ => color(s.bg),
            },
        )
        .collect();
    egui::ColorImage::new([SWATCH_PIXELS, SWATCH_PIXELS], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::Appearance;
    use crate::config::{self, Config};
    use crate::egui_test::{
        assert_no_overlap, click_at, fully_painted_text_at, painted, run_frame,
    };
    use crate::gui;
    use crate::i18n::Lang;
    use crate::settings::WINDOW_MIN_SIZE;
    use crate::settings::tests::{app_saving_to, settings_app_with};

    /// The narrowest the window gets, tall enough that the column does not scroll.
    const TEST_SIZE: [f32; 2] = [WINDOW_MIN_SIZE[0], 1000.0];

    #[test]
    fn appearance_tab_text_is_whole_on_one_line_in_every_language() {
        for lang in Lang::ALL {
            for theme in WindowTheme::ALL {
                let mut app = settings_app_with(Config {
                    language: Some(lang.tag().to_owned()),
                    theme,
                    ..Config::default()
                });

                let painted = fully_painted_text_at(TEST_SIZE, |ui| {
                    app.render_tab_bar(ui);
                    app.render_appearance_tab(ui);
                });

                let l = loader(lang);
                let mut expected: Vec<String> = [
                    "tab-general",
                    "tab-appearance",
                    "tab-devices",
                    "group-windows",
                    "window-theme",
                    "group-colours",
                    "appearance-palette",
                    "group-tray-icon",
                    "tray-icon-style",
                ]
                .into_iter()
                .map(|id| l.get(id))
                .collect();
                expected.push(theme.label(lang));
                expected.extend(Palette::ALL.map(|palette| palette.label(lang)));
                expected.extend(DisplayMode::ALL.map(|mode| mode.label(lang)));
                for text in &expected {
                    let lines = painted.iter().find(|p| &p.text == text).map(|p| p.lines);
                    assert_eq!(
                        lines,
                        Some(1),
                        "{lang:?}: {text:?} is cut off, missing or wrapped: {painted:?}"
                    );
                }
                for hint in ["window-theme-hint", "appearance-palette-hint"].map(|id| l.get(id)) {
                    assert!(
                        painted.iter().any(|p| p.text == hint),
                        "{lang:?}: {hint:?} is cut off or missing: {painted:?}"
                    );
                }
                assert_no_overlap(&painted);
            }
        }
    }

    #[test]
    fn choosing_a_theme_in_the_combo_saves_it() {
        let (mut app, path) = app_saving_to("theme-combo", Config::default());
        let ctx = egui::Context::default();
        let size = TEST_SIZE;
        run_frame(&ctx, size, Vec::new(), |ui| app.render_appearance_tab(ui));
        let combo = fully_painted_text_at(size, |ui| app.render_appearance_tab(ui))
            .into_iter()
            .find(|p| p.text == "System")
            .expect("the combo shows the current theme");

        click_at(&ctx, size, combo.rect.center(), |ui| {
            app.render_appearance_tab(ui)
        });
        let open = run_frame(&ctx, size, Vec::new(), |ui| app.render_appearance_tab(ui));
        let light = painted(&open)
            .into_iter()
            .find(|p| p.text == "Light")
            .expect("the open combo lists Light");
        click_at(&ctx, size, light.rect.center(), |ui| {
            app.render_appearance_tab(ui)
        });

        assert_eq!(config::load_from(&path).theme, WindowTheme::Light);
        assert_eq!(app.config.theme, WindowTheme::Light);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// The window's look follows the saved theme; the portal still says dark.
    #[test]
    fn choosing_light_restyles_the_window_over_a_dark_portal() {
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        let (_portal, appearance) = tokio::sync::watch::channel(Appearance {
            scheme: ColorScheme::Dark,
            ..Appearance::default()
        });
        let mut app = settings_app_with(Config::default());
        let window = egui::Context::default();
        gui::follow(
            rt.handle(),
            window.clone(),
            appearance,
            app.theme.subscribe(),
        );
        let dark = || window.options(|o| o.theme_preference) == egui::ThemePreference::Dark;
        assert!(dark());

        app.config.theme = WindowTheme::Light;
        app.follow_theme();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while dark() {
            assert!(
                std::time::Instant::now() < deadline,
                "the window never turned light"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = window.run_ui(egui::RawInput::default(), |_| {});
        assert!(!window.global_style().visuals.dark_mode);
    }

    #[test]
    fn clicking_a_palette_tile_saves_the_palette_and_restyles_the_previews() {
        let (mut app, path) = app_saving_to("palette-tile", Config::default());
        let size = TEST_SIZE;
        let nord = fully_painted_text_at(size, |ui| app.render_appearance_tab(ui))
            .into_iter()
            .find(|p| p.text == "Nord")
            .expect("the Nord tile is painted");
        let ctx = egui::Context::default();
        run_frame(&ctx, size, Vec::new(), |ui| app.render_appearance_tab(ui));

        click_at(&ctx, size, nord.rect.center(), |ui| {
            app.render_appearance_tab(ui)
        });

        assert_eq!(config::load_from(&path).palette, Palette::Nord);
        assert_eq!(app.config.palette, Palette::Nord);
        run_frame(&ctx, size, Vec::new(), |ui| app.render_appearance_tab(ui));
        assert_eq!(
            app.style_previews.as_ref().map(|p| p.palette),
            Some(Palette::Nord),
            "the icon-style tiles are drawn in the new palette"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_swatch_shows_four_colours_on_the_palette_background() {
        let s = palette::swatches(Palette::Catppuccin, ColorScheme::Dark);
        let image = swatch_image(&s);
        let at = |x: usize, y: usize| image.pixels[y * SWATCH_PIXELS + x];
        let color = |[r, g, b]: Rgb| egui::Color32::from_rgb(r, g, b);
        let (near, far) = (SWATCH_GAP, SWATCH_PIXELS - SWATCH_GAP - 1);
        assert_eq!(at(0, 0), color(s.bg));
        assert_eq!(at(SWATCH_PIXELS / 2, SWATCH_PIXELS / 2), color(s.bg));
        assert_eq!(at(near, near), color(s.fg));
        assert_eq!(at(far, near), color(s.charging));
        assert_eq!(at(near, far), color(s.warn));
        assert_eq!(at(far, far), color(s.low));
    }

    #[test]
    fn style_previews_cover_every_display_mode() {
        let ctx = egui::Context::default();

        let previews = render_style_previews(
            &ctx,
            ColorScheme::Dark,
            Palette::default(),
            STYLE_PREVIEW_MAX_PIXELS,
        );

        let modes: Vec<_> = previews.iter().map(|(mode, _)| *mode).collect();
        assert_eq!(modes, DisplayMode::ALL.to_vec());
        for (mode, texture) in &previews {
            assert_eq!(texture.size(), [64, 64], "{mode:?}");
        }
    }
}
