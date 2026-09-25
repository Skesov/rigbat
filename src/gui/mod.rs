//! Session appearance for the settings window and the dashboard.

use eframe::egui;
use tokio::sync::watch;

use crate::appearance::{Appearance, ColorScheme};
use crate::domain::DeviceKind;
use crate::icon::Theme;

/// A device row in either window, and the least height of a settings row.
pub const ROW_HEIGHT: f32 = 48.0;
pub const GLYPH_COLUMN: f32 = 30.0;
pub const GLYPH_SIZE: f32 = 20.0;

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

/// Re-applies every appearance change for as long as the window lives.
pub fn follow(
    rt: &tokio::runtime::Handle,
    ctx: egui::Context,
    mut appearance: watch::Receiver<Appearance>,
) {
    rt.spawn(async move {
        while appearance.changed().await.is_ok() {
            let current = *appearance.borrow_and_update();
            apply(&ctx, &current);
            ctx.request_repaint();
        }
    });
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

pub fn kind_glyph(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Mouse => "\u{1F5B1}",
        DeviceKind::Keyboard => "\u{2328}",
        DeviceKind::Headset => "\u{1F3A7}",
        DeviceKind::Controller => "\u{1F3AE}",
        DeviceKind::Other => "\u{1F50B}",
    }
}

/// Hints, notes and subtitles. `weak_text_color` misses WCAG 4.5:1 on a dark panel.
pub fn secondary_text(visuals: &egui::Visuals) -> egui::Color32 {
    visuals.text_color()
}

/// A charge value: in the low colour when low, strong when live, else secondary.
pub fn charge_value_text(
    visuals: &egui::Visuals,
    text: String,
    low: bool,
    online: bool,
) -> egui::RichText {
    let text = egui::RichText::new(text);
    match (low, online) {
        (true, _) => text.strong().color(color(theme(visuals).low)),
        (false, true) => text.strong(),
        (false, false) => text.color(secondary_text(visuals)),
    }
}

/// The status colours of the tray icon, for this window's scheme.
pub fn theme(visuals: &egui::Visuals) -> Theme {
    if visuals.dark_mode {
        Theme::dark()
    } else {
        Theme::light()
    }
}

pub fn color([r, g, b, a]: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// WCAG 2.1 contrast ratio between two opaque colors.
pub fn contrast_ratio(a: egui::Color32, b: egui::Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn relative_luminance(c: egui::Color32) -> f32 {
    let linear = |v: u8| {
        let s = f32::from(v) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(c.r()) + 0.7152 * linear(c.g()) + 0.0722 * linear(c.b())
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
    fn contrast_ratio_spans_one_to_twenty_one() {
        let ratio = contrast_ratio(egui::Color32::BLACK, egui::Color32::WHITE);
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
        assert!((contrast_ratio(egui::Color32::RED, egui::Color32::RED) - 1.0).abs() < 1e-6);
    }
}
