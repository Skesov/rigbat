//! Session appearance for the settings window and the dashboard.

use eframe::egui;
use tokio::sync::watch;

use crate::appearance::{Appearance, ColorScheme};
use crate::domain::{DeviceKind, Palette, WindowTheme};
use crate::icon;
use crate::palette::{self, DIM, GRAPHIC_CONTRAST, Rgb, TEXT_CONTRAST};

/// A device row in either window, and the least height of a settings row.
pub const ROW_HEIGHT: f32 = 48.0;
pub const GLYPH_COLUMN: f32 = 30.0;
/// Three points per cell of the 7 × 7 kind glyph.
pub const GLYPH_SIZE: f32 = 21.0;

/// Accent goes into both styles so a scheme switch keeps it.
pub fn apply(ctx: &egui::Context, appearance: &Appearance) {
    ctx.set_theme(match appearance.scheme {
        ColorScheme::Dark => egui::ThemePreference::Dark,
        ColorScheme::Light => egui::ThemePreference::Light,
    });
    ctx.set_zoom_factor(appearance.text_scale);
    ctx.all_styles_mut(|style| {
        let defaults = if style.visuals.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        style.visuals.selection = match appearance.accent {
            Some([r, g, b]) => {
                let fill = egui::Color32::from_rgb(r, g, b);
                egui::style::Selection {
                    bg_fill: fill,
                    stroke: egui::Stroke::new(defaults.selection.stroke.width, readable_on(fill)),
                }
            }
            None => defaults.selection,
        };
    });
}

/// Applies the session's look under `theme` now, and again on every change of
/// either for as long as the window lives.
pub fn follow(
    rt: &tokio::runtime::Handle,
    ctx: egui::Context,
    mut appearance: watch::Receiver<Appearance>,
    mut theme: watch::Receiver<WindowTheme>,
) {
    apply(&ctx, &look(&mut appearance, &mut theme));
    rt.spawn(async move {
        // A theme fixed at launch closes its channel; the portal is still followed.
        let mut theme_open = true;
        loop {
            tokio::select! {
                changed = appearance.changed() => if changed.is_err() { return },
                changed = theme.changed(), if theme_open => theme_open = changed.is_ok(),
            }
            apply(&ctx, &look(&mut appearance, &mut theme));
            ctx.request_repaint();
        }
    });
}

fn look(
    appearance: &mut watch::Receiver<Appearance>,
    theme: &mut watch::Receiver<WindowTheme>,
) -> Appearance {
    let theme = *theme.borrow_and_update();
    appearance.borrow_and_update().with_theme(theme)
}

/// A window size in points, grown with the text so the layout still fits.
pub fn scaled([width, height]: [f32; 2], text_scale: f32) -> [f32; 2] {
    [width * text_scale, height * text_scale]
}

/// Black or white, whichever reads better on `fill`.
fn readable_on(fill: egui::Color32) -> egui::Color32 {
    if contrast_ratio(fill, egui::Color32::BLACK) >= contrast_ratio(fill, egui::Color32::WHITE) {
        egui::Color32::BLACK
    } else {
        egui::Color32::WHITE
    }
}

/// The kind's silhouette from `icon::kind_glyph`, the one the tray icon draws,
/// `GLYPH_SIZE` square around `centre`, its cell edges on whole pixels.
pub fn kind_glyph(
    centre: egui::Pos2,
    kind: DeviceKind,
    color: egui::Color32,
    pixels_per_point: f32,
) -> egui::Shape {
    let bitmap = icon::kind_glyph(kind);
    let origin = centre - egui::Vec2::splat(GLYPH_SIZE / 2.0);
    let cell = egui::vec2(
        GLYPH_SIZE / bitmap.cols as f32,
        GLYPH_SIZE / bitmap.row_count() as f32,
    );
    let snap = |p: egui::Pos2| {
        egui::pos2(
            (p.x * pixels_per_point).round() / pixels_per_point,
            (p.y * pixels_per_point).round() / pixels_per_point,
        )
    };
    let mut mesh = egui::Mesh::default();
    for (col, row) in bitmap.cells() {
        let min = origin + egui::vec2(col as f32 * cell.x, row as f32 * cell.y);
        mesh.add_colored_rect(egui::Rect::from_min_max(snap(min), snap(min + cell)), color);
    }
    egui::Shape::mesh(mesh)
}

/// Hints, notes and subtitles. `weak_text_color` misses WCAG 4.5:1 on a dark panel.
pub fn secondary_text(visuals: &egui::Visuals) -> egui::Color32 {
    visuals.text_color()
}

/// A charge value: in the low colour when low, strong when live, else secondary.
pub fn charge_value_text(
    visuals: &egui::Visuals,
    status: &StatusColors,
    text: String,
    low: bool,
    online: bool,
) -> egui::RichText {
    let text = egui::RichText::new(text);
    match (low, online) {
        (true, _) => text.strong().color(status.low),
        (false, true) => text.strong(),
        (false, false) => text.color(secondary_text(visuals)),
    }
}

/// The palette's status colours for a window, readable where the window paints them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatusColors {
    /// An ordinary reading's bar.
    pub normal: egui::Color32,
    pub charging: egui::Color32,
    /// Text and bar.
    pub low: egui::Color32,
    /// Text.
    pub warn: egui::Color32,
    pub track: egui::Color32,
}

/// Text roles reach 4.5:1 on every surface text is painted on; bars reach
/// 3:1 on the panel, dimmed included.
pub fn status_colors(visuals: &egui::Visuals, palette: Palette) -> StatusColors {
    let scheme = if visuals.dark_mode {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };
    let s = palette::swatches(palette, scheme);
    let panel = rgb(visuals.panel_fill);
    let text = |color| {
        text_surfaces(visuals)
            .into_iter()
            .fold(color, |c, surface| {
                palette::readable(c, rgb(surface), TEXT_CONTRAST, 1.0)
            })
    };
    let bar = |color| palette::readable(color, panel, GRAPHIC_CONTRAST, DIM);
    StatusColors {
        normal: opaque(bar(s.fg)),
        charging: opaque(bar(s.charging)),
        low: opaque(text(s.low)),
        warn: opaque(text(s.warn)),
        track: opaque(s.track),
    }
}

/// The panel, a settings group's fill and a button's fill.
pub fn text_surfaces(visuals: &egui::Visuals) -> [egui::Color32; 3] {
    [
        visuals.panel_fill,
        visuals.panel_fill.blend(visuals.faint_bg_color),
        visuals.widgets.inactive.weak_bg_fill,
    ]
}

fn rgb(c: egui::Color32) -> Rgb {
    [c.r(), c.g(), c.b()]
}

fn opaque([r, g, b]: Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

/// WCAG 2.1 contrast ratio between two opaque colors.
pub fn contrast_ratio(a: egui::Color32, b: egui::Color32) -> f32 {
    palette::contrast_ratio(rgb(a), rgb(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_pass(ctx: &egui::Context) {
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
    }

    #[test]
    fn apply_sets_theme_zoom_and_accent_in_both_styles() {
        let ctx = egui::Context::default();
        let accent = egui::Color32::from_rgb(0x40, 0xA0, 0x2B);
        apply(
            &ctx,
            &Appearance {
                scheme: ColorScheme::Light,
                accent: Some([0x40, 0xA0, 0x2B]),
                text_scale: 1.25,
            },
        );
        run_pass(&ctx);

        assert_eq!(ctx.theme(), egui::Theme::Light);
        assert!(!ctx.global_style().visuals.dark_mode);
        assert_eq!(ctx.zoom_factor(), 1.25);
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            let selection = ctx.style_of(theme).visuals.selection;
            assert_eq!(selection.bg_fill, accent, "{theme:?}");
            assert!(
                contrast_ratio(selection.stroke.color, accent) >= 4.5,
                "{theme:?}: selection text unreadable on the accent"
            );
        }
    }

    #[test]
    fn losing_the_accent_restores_the_default_selection() {
        let ctx = egui::Context::default();
        let mut appearance = Appearance {
            scheme: ColorScheme::Dark,
            accent: Some([200, 0, 0]),
            text_scale: 1.0,
        };
        apply(&ctx, &appearance);
        appearance.accent = None;
        apply(&ctx, &appearance);
        run_pass(&ctx);

        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.selection,
            egui::Visuals::dark().selection
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Light).visuals.selection,
            egui::Visuals::light().selection
        );
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn preference(ctx: &egui::Context) -> egui::ThemePreference {
        ctx.options(|o| o.theme_preference)
    }

    #[test]
    fn a_forced_theme_wins_over_the_portal_until_it_is_system_again() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let (portal, appearance) = watch::channel(Appearance::default());
        let (theme, theme_rx) = watch::channel(WindowTheme::Light);
        let ctx = egui::Context::default();

        follow(rt.handle(), ctx.clone(), appearance, theme_rx);
        run_pass(&ctx);
        assert_eq!(preference(&ctx), egui::ThemePreference::Light);
        assert!(!ctx.global_style().visuals.dark_mode);

        theme.send_replace(WindowTheme::System);
        until("System follows the dark portal", || {
            preference(&ctx) == egui::ThemePreference::Dark
        });
        theme.send_replace(WindowTheme::Dark);
        portal.send_modify(|a| {
            a.scheme = ColorScheme::Light;
            a.accent = Some([200, 0, 0]);
        });
        until("the accent still follows the portal", || {
            ctx.style_of(egui::Theme::Dark).visuals.selection.bg_fill
                == egui::Color32::from_rgb(200, 0, 0)
        });
        assert_eq!(preference(&ctx), egui::ThemePreference::Dark);
    }

    #[test]
    fn a_theme_fixed_at_launch_still_follows_the_portal() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let (portal, appearance) = watch::channel(Appearance::default());
        let ctx = egui::Context::default();

        follow(
            rt.handle(),
            ctx.clone(),
            appearance,
            watch::channel(WindowTheme::System).1,
        );
        assert_eq!(preference(&ctx), egui::ThemePreference::Dark);

        portal.send_modify(|a| a.scheme = ColorScheme::Light);
        until("the window follows the portal to light", || {
            preference(&ctx) == egui::ThemePreference::Light
        });
    }

    #[test]
    fn secondary_text_is_readable_in_both_themes() {
        for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
            let text = secondary_text(&visuals);
            assert_eq!(text.a(), 255, "secondary text must be opaque");
            let group = visuals.panel_fill.blend(visuals.faint_bg_color);
            for (surface, fill) in [("panel", visuals.panel_fill), ("group", group)] {
                let ratio = contrast_ratio(text, fill);
                assert!(
                    ratio >= 4.5,
                    "dark_mode={}: {ratio:.2}:1 on the {surface} is below 4.5:1",
                    visuals.dark_mode
                );
            }
        }
    }

    #[test]
    fn every_palette_status_colour_is_readable_where_the_window_paints_it() {
        for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
            for palette in Palette::ALL {
                let status = status_colors(&visuals, palette);
                let at = |role| format!("{palette:?} dark_mode={} {role}", visuals.dark_mode);
                for (role, color) in [("low", status.low), ("warn", status.warn)] {
                    for surface in text_surfaces(&visuals) {
                        let ratio = contrast_ratio(color, surface);
                        assert!(ratio >= TEXT_CONTRAST, "{}: {ratio:.2}:1", at(role));
                    }
                }
                for (role, color, opacity) in [
                    ("normal", status.normal, DIM),
                    ("charging", status.charging, DIM),
                    ("low", status.low, 1.0),
                ] {
                    let panel = rgb(visuals.panel_fill);
                    let painted = palette::over(rgb(color), panel, opacity);
                    let ratio = palette::contrast_ratio(painted, panel);
                    assert!(ratio >= GRAPHIC_CONTRAST, "{}: {ratio:.2}:1", at(role));
                }
            }
        }
    }

    #[test]
    fn the_window_glyph_is_the_tray_bitmap_on_whole_pixels() {
        for kind in crate::egui_test::KINDS {
            let bitmap = icon::kind_glyph(kind);
            for pixels_per_point in [1.0_f32, 1.25_f32, 2.0_f32] {
                let centre = egui::pos2(15.3_f32, 24.0_f32);
                let shape = kind_glyph(centre, kind, egui::Color32::WHITE, pixels_per_point);
                let egui::Shape::Mesh(mesh) = shape else {
                    unreachable!("{kind:?}: not a mesh: {shape:?}");
                };
                let cells: Vec<_> = bitmap.cells().collect();
                assert!(!cells.is_empty(), "{kind:?}");
                assert_eq!(mesh.vertices.len(), 4 * cells.len(), "{kind:?}");
                let square = egui::Rect::from_center_size(centre, egui::Vec2::splat(GLYPH_SIZE));
                for (quad, (col, row)) in mesh.vertices.chunks(4).zip(cells) {
                    for v in quad {
                        let px = v.pos * pixels_per_point;
                        assert_eq!(px, px.round(), "{kind:?} off the pixel grid");
                        assert!(
                            square.expand(1.0).contains(v.pos),
                            "{kind:?} leaves its square"
                        );
                    }
                    let cell = GLYPH_SIZE / 7.0;
                    let at = quad[0].pos - square.min;
                    assert!(
                        (at.x - col as f32 * cell).abs() <= 1.0
                            && (at.y - row as f32 * cell).abs() <= 1.0,
                        "{kind:?} cell ({col}, {row}) at {at:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn contrast_ratio_spans_one_to_twenty_one() {
        let ratio = contrast_ratio(egui::Color32::BLACK, egui::Color32::WHITE);
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
        assert!((contrast_ratio(egui::Color32::RED, egui::Color32::RED) - 1.0).abs() < 1e-6);
    }
}
