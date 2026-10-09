//! Session appearance for the settings window and the dashboard.

use eframe::egui;
use tokio::sync::watch;

use crate::appearance::{Appearance, ColorScheme};
use crate::domain::{DeviceKind, Palette, WindowTheme};
use crate::palette::{self, DIM, Rgb, Targets};

/// A device row in either window, and the least height of a settings row.
pub const ROW_HEIGHT: f32 = 48.0;
pub const GLYPH_COLUMN: f32 = 30.0;
pub const GLYPH_SIZE: f32 = 20.0;
/// A running action shows progress only once it has run this long: a
/// spinner or "Refreshing…" that flashes for a moment distracts more than it
/// informs (GNOME HIG, spinners).
pub const PROGRESS_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// How long an action running since `since` waits before showing progress;
/// zero once it shows.
pub fn progress_wait(since: std::time::Instant, now: std::time::Instant) -> std::time::Duration {
    PROGRESS_DELAY.saturating_sub(now.saturating_duration_since(since))
}

/// WCAG 2.2 SC 2.5.8: the least width and height of anything a pointer operates.
pub const MIN_TARGET: f32 = 24.0;

/// The session's contrast and motion preferences, as `apply` last set them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Preferences {
    high_contrast: bool,
    reduced_motion: bool,
}

fn preferences_id() -> egui::Id {
    egui::Id::new("rigbat-session-preferences")
}

fn preferences(ctx: &egui::Context) -> Preferences {
    ctx.data(|d| d.get_temp(preferences_id()))
        .unwrap_or_default()
}

/// The contrast the window's status colours must reach.
pub fn targets(ctx: &egui::Context) -> Targets {
    Targets::for_contrast(preferences(ctx).high_contrast)
}

/// The session asks for no motion: no animation, no spinner.
pub fn reduced_motion(ctx: &egui::Context) -> bool {
    preferences(ctx).reduced_motion
}

/// Makes `id`'s AccessKit node a polite live region reading `text`: a screen
/// reader speaks it when it appears and whenever it changes, without moving
/// focus (WCAG 4.1.3). AccessKit's AT-SPI adapter sends the announcement.
pub fn live_region(ctx: &egui::Context, id: egui::Id, text: &str) {
    use egui::accesskit::{Live, Role};
    ctx.accesskit_node_builder(id, |node| {
        if node.role() == Role::Unknown {
            node.set_role(Role::Label);
        }
        node.set_value(text);
        node.set_live(Live::Polite);
    });
}

/// Accent goes into both styles so a scheme switch keeps it.
pub fn apply(ctx: &egui::Context, appearance: &Appearance) {
    ctx.set_theme(match appearance.scheme {
        ColorScheme::Dark => egui::ThemePreference::Dark,
        ColorScheme::Light => egui::ThemePreference::Light,
    });
    ctx.set_zoom_factor(appearance.text_scale);
    ctx.data_mut(|d| {
        d.insert_temp(
            preferences_id(),
            Preferences {
                high_contrast: appearance.high_contrast,
                reduced_motion: appearance.reduced_motion,
            },
        );
    });
    let animation_time = if appearance.reduced_motion {
        0.0
    } else {
        egui::Style::default().animation_time
    };
    ctx.all_styles_mut(|style| {
        style.spacing.interact_size.y = MIN_TARGET;
        style.animation_time = animation_time;
        let defaults = if style.visuals.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        style.visuals = defaults.clone();
        if appearance.high_contrast {
            raise_contrast(&mut style.visuals);
        }
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

/// Every text and hairline in the scheme's strongest text colour, so body
/// text clears 7:1 and strokes read as clearly as text.
fn raise_contrast(visuals: &mut egui::Visuals) {
    let strong = visuals.strong_text_color();
    visuals.override_text_color = Some(strong);
    visuals.weak_text_color = Some(strong);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.fg_stroke.color = strong;
    }
    visuals.widgets.noninteractive.bg_stroke.color = strong;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, strong);
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

/// GNOME's smallest supported screen width; no window needs more at any text scale.
pub const SMALLEST_SCREEN_WIDTH: f32 = 1024.0;

/// A window size in points, grown with the text but never wider than
/// `SMALLEST_SCREEN_WIDTH`: past it the layout adapts instead.
pub fn scaled([width, height]: [f32; 2], text_scale: f32) -> [f32; 2] {
    [
        (width * text_scale).min(SMALLEST_SCREEN_WIDTH),
        height * text_scale,
    ]
}

#[cfg(test)]
/// How wide `scaled(size, text_scale)` is in the window's own, zoomed points.
pub fn zoomed_width(size: [f32; 2], text_scale: f32) -> f32 {
    scaled(size, text_scale)[0] / text_scale
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

/// Text roles reach `targets.text` on every surface text is painted on; bars
/// reach `targets.graphic` on the panel, dimmed included.
pub fn status_colors(visuals: &egui::Visuals, palette: Palette, targets: Targets) -> StatusColors {
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
                palette::readable(c, rgb(surface), targets.text, 1.0)
            })
    };
    let bar = |color| palette::readable(color, panel, targets.graphic, DIM);
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

    /// The windows draw catalogue text in egui's bundled fonts; a character outside them
    /// would paint as "◻".
    #[test]
    fn every_catalogue_character_is_in_the_bundled_fonts() {
        let ctx = egui::Context::default();
        crate::egui_test::empty_pass(&ctx);
        let font = egui::TextStyle::Body.resolve(&ctx.global_style());
        let catalogues = [
            include_str!("../../i18n/en/rigbat.ftl"),
            include_str!("../../i18n/ru/rigbat.ftl"),
        ];
        let text: String = catalogues
            .iter()
            .flat_map(|source| source.lines())
            .filter(|line| !line.trim_start().starts_with('#'))
            .chain(["\u{a0}"])
            .collect();
        ctx.fonts_mut(|fonts| {
            for c in text.chars() {
                assert!(
                    crate::egui_test::has_glyph(fonts, &font, c),
                    "{c:?} (U+{:04X})",
                    u32::from(c)
                );
            }
            assert!(!crate::egui_test::has_glyph(fonts, &font, '中'));
        });
    }

    /// egui breaks a line at U+202F, the space GNOME prescribes between a number and its
    /// unit, so the catalogues join them with U+00A0, the one space it never breaks at.
    #[test]
    fn only_u00a0_keeps_a_number_and_its_unit_on_one_row() {
        let ctx = egui::Context::default();
        crate::egui_test::empty_pass(&ctx);
        let font = egui::TextStyle::Body.resolve(&ctx.global_style());
        let rows = |fonts: &mut egui::epaint::text::FontsView<'_>, text: &str, width: f32| {
            let galley = fonts.layout(text.to_owned(), font.clone(), egui::Color32::WHITE, width);
            galley.rows.iter().map(|row| row.text()).collect::<Vec<_>>()
        };
        ctx.fonts_mut(|fonts| {
            let width = fonts
                .layout_no_wrap("10\u{a0}min".to_owned(), font.clone(), egui::Color32::WHITE)
                .size()
                .x;
            assert_eq!(rows(fonts, "x 10\u{a0}min", width), ["x ", "10\u{a0}min"]);
            assert_eq!(
                rows(fonts, "x 10\u{202f}min", width),
                ["x 10\u{202f}", "min"]
            );
        });
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
                ..Appearance::default()
            },
        );
        crate::egui_test::empty_pass(&ctx);

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
            ..Appearance::default()
        };
        apply(&ctx, &appearance);
        appearance.accent = None;
        apply(&ctx, &appearance);
        crate::egui_test::empty_pass(&ctx);

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
        let (portal, appearance) = watch::channel(Appearance {
            scheme: ColorScheme::Dark,
            ..Appearance::default()
        });
        let (theme, theme_rx) = watch::channel(WindowTheme::Light);
        let ctx = egui::Context::default();

        follow(rt.handle(), ctx.clone(), appearance, theme_rx);
        crate::egui_test::empty_pass(&ctx);
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
        let (portal, appearance) = watch::channel(Appearance {
            scheme: ColorScheme::Dark,
            ..Appearance::default()
        });
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

    /// Raised contrast included: WCAG AAA 7:1 text, 4.5:1 bars.
    #[test]
    fn every_palette_status_colour_is_readable_where_the_window_paints_it() {
        for (high_contrast, targets) in [(false, Targets::NORMAL), (true, Targets::HIGH)] {
            for mut visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
                if high_contrast {
                    raise_contrast(&mut visuals);
                }
                for palette in Palette::ALL {
                    let status = status_colors(&visuals, palette, targets);
                    let at = |role| {
                        format!(
                            "{palette:?} dark_mode={} high={high_contrast} {role}",
                            visuals.dark_mode
                        )
                    };
                    for (role, color) in [("low", status.low), ("warn", status.warn)] {
                        for surface in text_surfaces(&visuals) {
                            let ratio = contrast_ratio(color, surface);
                            assert!(ratio >= targets.text, "{}: {ratio:.2}:1", at(role));
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
                        assert!(ratio >= targets.graphic, "{}: {ratio:.2}:1", at(role));
                    }
                }
            }
        }
    }

    #[test]
    fn high_contrast_raises_text_and_hairlines_to_7_to_1() {
        let ctx = egui::Context::default();
        for scheme in [ColorScheme::Dark, ColorScheme::Light] {
            apply(
                &ctx,
                &Appearance {
                    scheme,
                    high_contrast: true,
                    ..Appearance::default()
                },
            );
            crate::egui_test::empty_pass(&ctx);
            let visuals = ctx.global_style().visuals.clone();
            for surface in text_surfaces(&visuals) {
                for (role, color) in [
                    ("text", visuals.text_color()),
                    ("secondary", secondary_text(&visuals)),
                    ("weak", visuals.weak_text_color()),
                    ("hairline", visuals.widgets.noninteractive.bg_stroke.color),
                ] {
                    let ratio = contrast_ratio(color, surface);
                    assert!(ratio >= 7.0, "{scheme:?} {role}: {ratio:.2}:1");
                }
            }
            assert_eq!(targets(&ctx), Targets::HIGH);
        }
        apply(&ctx, &Appearance::default());
        crate::egui_test::empty_pass(&ctx);
        assert_eq!(targets(&ctx), Targets::NORMAL);
        assert_eq!(
            ctx.global_style().visuals.text_color(),
            egui::Visuals::light().text_color()
        );
    }

    #[test]
    fn reduced_motion_turns_animation_off() {
        let ctx = egui::Context::default();
        apply(
            &ctx,
            &Appearance {
                reduced_motion: true,
                ..Appearance::default()
            },
        );
        crate::egui_test::empty_pass(&ctx);
        assert!(reduced_motion(&ctx));
        assert_eq!(ctx.global_style().animation_time, 0.0);
        let id = egui::Id::new("switch");
        ctx.animate_bool_responsive(id, false);
        assert_eq!(ctx.animate_bool_responsive(id, true), 1.0);

        apply(&ctx, &Appearance::default());
        crate::egui_test::empty_pass(&ctx);
        assert!(!reduced_motion(&ctx));
        assert!(ctx.global_style().animation_time > 0.0);
    }

    #[test]
    fn progress_shows_only_after_the_delay() {
        let since = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        assert_eq!(progress_wait(since, since), PROGRESS_DELAY);
        assert_eq!(progress_wait(since, since + ms(100)), ms(200));
        assert_eq!(
            progress_wait(since, since + PROGRESS_DELAY),
            std::time::Duration::ZERO
        );
        assert_eq!(
            progress_wait(since, since + ms(900)),
            std::time::Duration::ZERO
        );
    }

    #[test]
    fn no_window_grows_past_the_smallest_screen() {
        assert_eq!(scaled([672.0, 360.0], 1.0), [672.0, 360.0]);
        assert_eq!(scaled([672.0, 360.0], 2.0), [SMALLEST_SCREEN_WIDTH, 720.0]);
        assert_eq!(zoomed_width([672.0, 360.0], 2.0), 512.0);
        assert_eq!(scaled([380.0, 104.0], 2.0)[0], 760.0);
    }

    #[test]
    fn contrast_ratio_spans_one_to_twenty_one() {
        let ratio = contrast_ratio(egui::Color32::BLACK, egui::Color32::WHITE);
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
        assert!((contrast_ratio(egui::Color32::RED, egui::Color32::RED) - 1.0).abs() < 1e-6);
    }
}
