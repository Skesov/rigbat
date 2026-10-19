use super::*;

impl Theme {
    fn dark() -> Self {
        Self::new(Palette::default(), ColorScheme::Dark)
    }

    fn light() -> Self {
        Self::new(Palette::default(), ColorScheme::Light)
    }
}

/// The status mark sits under the digits with a clear gap; this asserts
/// no digit pixel is lost at any published size.
#[test]
fn the_status_mark_does_not_erase_a_digit() {
    let theme = Theme::dark();
    let renderer = TinySkiaRenderer {
        sizes: vec![22, 24, 32, 44, 48, 64],
    };
    let statuses = [
        (PrimaryStatus::Ok { percent: 88 }, 88, theme.normal),
        (PrimaryStatus::Charging { percent: 88 }, 88, theme.charging),
        (
            PrimaryStatus::Charging { percent: 100 },
            100,
            theme.charging,
        ),
        (PrimaryStatus::Low { percent: 8 }, 8, theme.low),
    ];

    for (status, percent, rgba) in statuses {
        let icons = renderer.render(
            status,
            Some(DeviceKind::Mouse),
            &theme,
            DisplayMode::PercentOnly,
            false,
        );
        for icon in icons {
            let (w, h) = (icon.width as u32, icon.height as u32);
            let mut digits = Pixmap::new(w, h).expect("digit mask pixmap");
            let region = digit_region(h, status_mark(status));
            draw_percent_in_region(&mut digits, region, percent, color(rgba), false);

            let lost = digits
                .pixels()
                .iter()
                .enumerate()
                .filter(|(i, pixel)| pixel.alpha() > 0 && icon.data[i * 4] == 0)
                .count();
            assert_eq!(lost, 0, "{status:?} at {w}x{h}: {lost} digit pixels erased");
        }
    }
}

// --- digits ------------------------------------------------------------

const WHITE: Color = Color::WHITE;

fn digits(size: u32, value: u8, mark: Option<Bitmap>, dotted: bool) -> (Pixmap, Region) {
    let mut pixmap = Pixmap::new(wide_width(size), size).expect("digit pixmap");
    let region = digit_region(size, mark);
    draw_percent_in_region(&mut pixmap, region, value, WHITE, dotted);
    (pixmap, region)
}

/// Painted pixels as `(x, y)`.
fn ink(pixmap: &Pixmap) -> Vec<(u32, u32)> {
    let w = pixmap.width();
    pixmap
        .pixels()
        .iter()
        .enumerate()
        .filter(|(_, px)| px.alpha() > 0)
        .map(|(i, _)| (i as u32 % w, i as u32 / w))
        .collect()
}

fn ink_height(pixmap: &Pixmap) -> u32 {
    let rows = ink(pixmap).into_iter().map(|(_, y)| y);
    let (top, bottom) = rows.fold((u32::MAX, 0), |(t, b), y| (t.min(y), b.max(y)));
    bottom + 1 - top
}

#[test]
fn every_value_paints_inside_the_digit_region_at_every_size() {
    let sizes = PUBLISHED_SIZES.into_iter().chain(16..=64);
    for size in sizes {
        for mark in [None, Some(BOLT)] {
            for dotted in [false, true] {
                for value in 0..=100 {
                    let at = format!("{value} at {size} px, mark {mark:?}, dotted {dotted}");
                    let (pixmap, region) = digits(size, value, mark, dotted);
                    let painted = ink(&pixmap);
                    assert!(!painted.is_empty(), "{at}: no ink");
                    let (x0, y0) = (region.x.floor() as u32, region.y.floor() as u32);
                    let x1 = (region.x + region.w).ceil() as u32;
                    let y1 = (region.y + region.h).ceil() as u32;
                    let outside = painted
                        .iter()
                        .filter(|&&(x, y)| x < x0 || x >= x1 || y < y0 || y >= y1)
                        .count();
                    assert_eq!(outside, 0, "{at}: {outside} px outside the region");
                }
            }
        }
    }
}

#[test]
fn a_hundred_is_as_tall_as_two_digits() {
    for size in [32, 64] {
        let hundred = ink_height(&digits(size, 100, None, false).0);
        let eighty_eight = ink_height(&digits(size, 88, None, false).0);
        assert!(
            hundred.abs_diff(eighty_eight) <= 1,
            "{size} px: 100 is {hundred} px tall, 88 is {eighty_eight}"
        );
        for h in [hundred, eighty_eight] {
            assert!(h as f32 >= 0.8 * size as f32, "{size} px: {h} px tall");
        }
    }
}

#[test]
fn ink_density_does_not_depend_on_the_digit() {
    let fifty_seven = ink(&digits(64, 57, None, false).0).len() as f32;
    let eighty_six = ink(&digits(64, 86, None, false).0).len() as f32;
    let ratio = fifty_seven / eighty_six;
    // The font's own 7 has 0.64 of an 8's ink, so 57/86 sits near 0.77; the 3×5 bitmap gave 0.72.
    assert!((0.75..=1.33).contains(&ratio), "57 / 86 = {ratio}");
}

#[test]
fn dotted_digits_paint_less_but_stay_visible() {
    for size in PUBLISHED_SIZES.into_iter().chain([16, 48, 64]) {
        for value in [7, 11, 57, 86, 100] {
            let solid = ink(&digits(size, value, None, false).0).len();
            let dotted = ink(&digits(size, value, None, true).0).len();
            assert!(dotted < solid, "{value} at {size} px: {dotted} vs {solid}");
            assert!(dotted > 0, "{value} at {size} px: dotted is empty");
        }
    }
}

/// PNGs of a few values, live and dotted, for visual review.
/// Run with: `RIGBAT_DIGITS_DIR=<dir> cargo test dump_digits -- --ignored`.
#[test]
#[ignore]
fn dump_digits() {
    let dir = std::env::var_os("RIGBAT_DIGITS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir).expect("output dir");
    for size in [22, 32, 44] {
        for value in [57, 86, 80, 95, 38, 100, 7, 11] {
            for (dotted, look) in [(false, "live"), (true, "dotted")] {
                let (pixmap, _) = digits(size, value, None, dotted);
                pixmap
                    .save_png(dir.join(format!("{value}-{size}px-{look}.png")))
                    .expect("saving the png");
            }
        }
    }
}

fn default_sizes() -> Vec<u32> {
    TinySkiaRenderer::default().sizes.clone()
}

fn icon_pixmap(icon: &ksni::Icon) -> Pixmap {
    let size =
        tiny_skia::IntSize::from_wh(icon.width as u32, icon.height as u32).expect("icon size");
    Pixmap::from_vec(icon_to_rgba(icon), size).expect("icon pixmap")
}

/// Every state × mode × palette × scheme on the nominal panel: each cell
/// holds the 22 px icon ×3, the 22 px icon and the 64 px icon.
fn contact_sheet(schemes: &[ColorScheme], kind: Option<DeviceKind>) -> Pixmap {
    const ZOOM: u32 = 3;
    const PAD: u32 = 6;
    let states = [
        (PrimaryStatus::Ok { percent: 75 }, false),
        (PrimaryStatus::Charging { percent: 40 }, false),
        (PrimaryStatus::Low { percent: 12 }, false),
        (PrimaryStatus::Ok { percent: 75 }, true),
        (PrimaryStatus::Charging { percent: 100 }, true),
        (PrimaryStatus::Low { percent: 12 }, true),
        (PrimaryStatus::Offline, false),
    ];
    let renderer = TinySkiaRenderer {
        sizes: vec![22, 64],
    };
    let cell_w = 22 * ZOOM + 22 + 64 + 4 * PAD;
    let cell_h = 64 + 2 * PAD;
    let rows = schemes.len() * Palette::ALL.len() * DisplayMode::ALL.len();
    let mut sheet = Pixmap::new(cell_w * states.len() as u32, cell_h * rows as u32).expect("sheet");
    let mut row = 0;
    for &scheme in schemes {
        let [r, g, b] = match scheme {
            ColorScheme::Dark => NOMINAL_PANEL_DARK,
            ColorScheme::Light => NOMINAL_PANEL_LIGHT,
        };
        for palette_choice in Palette::ALL {
            let theme = Theme::new(palette_choice, scheme);
            for mode in DisplayMode::ALL {
                let y = (row * cell_h) as f32;
                let band =
                    Rect::from_xywh(0.0, y, sheet.width() as f32, cell_h as f32).expect("band");
                sheet.fill_rect(
                    band,
                    &solid_paint(Color::from_rgba8(r, g, b, 255)),
                    Transform::identity(),
                    None,
                );
                for (col, &(status, stale)) in states.iter().enumerate() {
                    let icons = renderer.render(status, kind, &theme, mode, stale);
                    let (small, large) = (icon_pixmap(&icons[0]), icon_pixmap(&icons[1]));
                    let x = (col as u32 * cell_w + PAD) as f32;
                    let paint = tiny_skia::PixmapPaint::default();
                    let middle = y + (cell_h / 2 - 11) as f32;
                    sheet.draw_pixmap(
                        0,
                        0,
                        small.as_ref(),
                        &paint,
                        Transform::from_scale(ZOOM as f32, ZOOM as f32)
                            .post_translate(x, y + (cell_h / 2 - 33) as f32),
                        None,
                    );
                    sheet.draw_pixmap(
                        0,
                        0,
                        small.as_ref(),
                        &paint,
                        Transform::from_translate(x + (22 * ZOOM + PAD) as f32, middle),
                        None,
                    );
                    sheet.draw_pixmap(
                        0,
                        0,
                        large.as_ref(),
                        &paint,
                        Transform::from_translate(
                            x + (22 * ZOOM + 22 + 2 * PAD) as f32,
                            y + PAD as f32,
                        ),
                        None,
                    );
                }
                row += 1;
            }
        }
    }
    sheet
}

/// Writes contact sheets, and `rigbat-icons.txt` with each 22 px icon as
/// text, to the temp directory for visual review.
/// Run with: `cargo test dump_icons -- --ignored`.
#[test]
#[ignore]
fn dump_icons() {
    let dir = std::env::temp_dir();
    let sheets = [
        (
            "rigbat-icons.png",
            &[ColorScheme::Dark, ColorScheme::Light][..],
        ),
        ("rigbat-icons-dark.png", &[ColorScheme::Dark][..]),
        ("rigbat-icons-light.png", &[ColorScheme::Light][..]),
    ];
    for (name, schemes) in sheets {
        contact_sheet(schemes, Some(DeviceKind::Mouse))
            .save_png(dir.join(name))
            .expect("saving the sheet");
    }
    for kind in [
        DeviceKind::Keyboard,
        DeviceKind::Headset,
        DeviceKind::Controller,
        DeviceKind::Other,
    ] {
        contact_sheet(&[ColorScheme::Dark], Some(kind))
            .save_png(dir.join(format!("rigbat-icons-{}.png", kind.as_str())))
            .expect("saving the sheet");
    }
    std::fs::write(dir.join("rigbat-icons.txt"), ascii_sheet()).expect("saving the text");
}

// --- mode: IconOnly --------------------------------------------------

#[test]
fn icon_only_renders_correct_count() {
    let renderer = TinySkiaRenderer::default();
    let icons = renderer.render(
        PrimaryStatus::Ok { percent: 50 },
        None,
        &Theme::dark(),
        DisplayMode::IconOnly,
        false,
    );
    assert_eq!(icons.len(), default_sizes().len());
}

#[test]
fn icon_only_dimensions_and_data_len() {
    let renderer = TinySkiaRenderer::default();
    let sizes = renderer.sizes.clone();
    let icons = renderer.render(
        PrimaryStatus::Ok { percent: 50 },
        None,
        &Theme::dark(),
        DisplayMode::IconOnly,
        false,
    );
    for icon in &icons {
        // Height matches a requested size; width is slightly wider.
        assert!(sizes.contains(&(icon.height as u32)));
        assert_eq!(icon.width as u32, wide_width(icon.height as u32));
        assert!(icon.width >= icon.height);
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
    }
}

// --- mode: PercentOnly -----------------------------------------------

#[test]
fn percent_only_renders_correct_count() {
    let renderer = TinySkiaRenderer::default();
    let icons = renderer.render(
        PrimaryStatus::Ok { percent: 50 },
        None,
        &Theme::dark(),
        DisplayMode::PercentOnly,
        false,
    );
    assert_eq!(icons.len(), default_sizes().len());
}

#[test]
fn percent_only_data_len() {
    let renderer = TinySkiaRenderer::default();
    let icons = renderer.render(
        PrimaryStatus::Ok { percent: 50 },
        None,
        &Theme::dark(),
        DisplayMode::PercentOnly,
        false,
    );
    for icon in &icons {
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
    }
}

// --- offline falls back to battery in every mode ---------------------

#[test]
fn offline_valid_in_percent_only() {
    let renderer = TinySkiaRenderer::default();
    let icons = renderer.render(
        PrimaryStatus::Offline,
        None,
        &Theme::dark(),
        DisplayMode::PercentOnly,
        false,
    );
    assert_eq!(icons.len(), default_sizes().len());
    for icon in &icons {
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
    }
}

#[test]
fn offline_valid_in_all_modes() {
    let renderer = TinySkiaRenderer::default();
    for mode in DisplayMode::ALL {
        let icons = renderer.render(PrimaryStatus::Offline, None, &Theme::dark(), mode, false);
        assert_eq!(
            icons.len(),
            default_sizes().len(),
            "offline failed for mode {mode:?}"
        );
        for icon in &icons {
            assert_eq!(
                icon.data.len(),
                (icon.width * icon.height * 4) as usize,
                "data.len mismatch for mode {mode:?}"
            );
        }
    }
}

// --- legacy-compatible tests (themes, charging, low) -----------------

#[test]
fn offline_renders_valid_icons() {
    let renderer = TinySkiaRenderer::default();
    let icons = renderer.render(
        PrimaryStatus::Offline,
        None,
        &Theme::dark(),
        DisplayMode::IconOnly,
        false,
    );
    assert_eq!(icons.len(), default_sizes().len());
    for icon in &icons {
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
    }
}

#[test]
fn charging_renders_valid_icons() {
    let renderer = TinySkiaRenderer::default();
    let icons = renderer.render(
        PrimaryStatus::Charging { percent: 75 },
        None,
        &Theme::dark(),
        DisplayMode::IconOnly,
        false,
    );
    assert_eq!(icons.len(), default_sizes().len());
    for icon in &icons {
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
    }
}

#[test]
fn low_renders_valid_icons() {
    let renderer = TinySkiaRenderer::default();
    let icons = renderer.render(
        PrimaryStatus::Low { percent: 10 },
        None,
        &Theme::dark(),
        DisplayMode::IconOnly,
        false,
    );
    assert_eq!(icons.len(), default_sizes().len());
    for icon in &icons {
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
    }
}

#[test]
fn dark_and_light_themes_differ_by_normal() {
    assert_ne!(Theme::dark().normal, Theme::light().normal);
}

#[test]
fn render_dark_differs_from_light() {
    let renderer = TinySkiaRenderer::default();
    let status = PrimaryStatus::Ok { percent: 50 };
    let dark_icons = renderer.render(status, None, &Theme::dark(), DisplayMode::IconOnly, false);
    let light_icons = renderer.render(status, None, &Theme::light(), DisplayMode::IconOnly, false);

    assert!(!dark_icons.is_empty());
    assert!(!light_icons.is_empty());
    assert_ne!(&dark_icons[0].data, &light_icons[0].data);
}

/// Mean alpha (icon data is ARGB, alpha is byte 0 of each pixel) across
/// all pixels of the first icon.
fn mean_alpha(icon: &ksni::Icon) -> f64 {
    let alphas: Vec<u8> = icon
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[0])
        .collect();
    alphas.iter().map(|&a| f64::from(a)).sum::<f64>() / alphas.len() as f64
}

#[test]
fn stale_reading_renders_more_transparent_than_fresh() {
    let renderer = TinySkiaRenderer::default();
    let status = PrimaryStatus::Ok { percent: 50 };
    let theme = Theme::dark();

    let fresh = renderer.render(status, None, &theme, DisplayMode::IconOnly, false);
    let stale = renderer.render(status, None, &theme, DisplayMode::IconOnly, true);

    assert_ne!(&fresh[0].data, &stale[0].data);
    assert!(
        mean_alpha(&stale[0]) < mean_alpha(&fresh[0]),
        "stale icon must be more transparent than the fresh one"
    );
}

#[test]
fn stale_offline_renders_same_as_non_stale_offline() {
    let renderer = TinySkiaRenderer::default();
    let theme = Theme::dark();

    let plain = renderer.render(
        PrimaryStatus::Offline,
        None,
        &theme,
        DisplayMode::IconOnly,
        false,
    );
    let stale_offline = renderer.render(
        PrimaryStatus::Offline,
        None,
        &theme,
        DisplayMode::IconOnly,
        true,
    );

    assert_eq!(&plain[0].data, &stale_offline[0].data);
}

#[test]
fn low_status_never_dims_when_stale() {
    let renderer = TinySkiaRenderer::default();
    let theme = Theme::dark();
    let status = PrimaryStatus::Low { percent: 10 };

    let fresh = renderer.render(status, None, &theme, DisplayMode::IconOnly, false);
    let stale = renderer.render(status, None, &theme, DisplayMode::IconOnly, true);

    assert_eq!(mean_alpha(&fresh[0]), mean_alpha(&stale[0]));
    assert_eq!(&fresh[0].data, &stale[0].data);
}

#[test]
fn stale_dimming_leaves_outline_and_digits_unchanged_only_fill_dims() {
    let renderer = TinySkiaRenderer::default();
    let theme = Theme::dark();
    let status = PrimaryStatus::Ok { percent: 60 };

    let fresh = renderer.render(status, None, &theme, DisplayMode::IconOnly, false);
    let stale = renderer.render(status, None, &theme, DisplayMode::IconOnly, true);
    let fresh_icon = &fresh[0];
    let stale_icon = &stale[0];

    // The outline/nub stroke is opaque and drawn last, on top of the
    // (possibly dimmed) fill, so SourceOver compositing makes its pixels
    // bit-identical between fresh and stale regardless of fill alpha.
    // Pixels strictly inside the fill area carry no such stroke, so
    // dimming must show up there instead.
    let mut mark_pixel_unchanged = false;
    let mut fill_pixel_dimmed = false;
    for (f, s) in fresh_icon
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(stale_icon.data.as_chunks::<4>().0.iter())
    {
        if f[0] == 255 && f == s {
            mark_pixel_unchanged = true;
        }
        if f[0] == 255 && s[0] < f[0] {
            fill_pixel_dimmed = true;
        }
    }
    assert!(
        mark_pixel_unchanged,
        "expected at least one fully-opaque pixel (outline/nub) to be identical"
    );
    assert!(
        fill_pixel_dimmed,
        "expected at least one fully-opaque fresh pixel (fill) to be more transparent when stale"
    );
}

/// WCAG 2.1 SC 1.4.11 asks 3:1 of a graphical object. The dimmed fill is
/// the case that has to pass: in IconOnly mode it is the whole reading.
#[test]
fn every_palette_icon_colour_clears_3_to_1_on_the_nominal_panel_dimmed_included() {
    for palette_choice in Palette::ALL {
        for (scheme, panel) in [
            (ColorScheme::Dark, NOMINAL_PANEL_DARK),
            (ColorScheme::Light, NOMINAL_PANEL_LIGHT),
        ] {
            let theme = Theme::new(palette_choice, scheme);
            for (role, [r, g, b, a], dims) in [
                ("normal", theme.normal, true),
                ("charging", theme.charging, true),
                ("low", theme.low, false),
                ("offline", theme.offline, false),
            ] {
                let opacity = f32::from(a) / 255.0 * if dims { DIM } else { 1.0 };
                let painted = palette::over([r, g, b], panel, opacity);
                let ratio = palette::contrast_ratio(painted, panel);
                assert!(
                    ratio >= GRAPHIC_CONTRAST,
                    "{palette_choice:?} {scheme:?} {role}: {ratio:.2}:1"
                );
            }
        }
    }
}

#[test]
fn the_palette_reaches_the_icon_colours() {
    let catppuccin = Theme::new(Palette::Catppuccin, ColorScheme::Dark);
    let nord = Theme::new(Palette::Nord, ColorScheme::Dark);
    assert_eq!(catppuccin.low, [0xf3, 0x8b, 0xa8, 255]);
    assert_eq!(nord.low, [0xbf, 0x61, 0x6a, 255]);
    assert_eq!(catppuccin.offline[3], 179);
}

// --- pixel helpers ------------------------------------------------------

/// Helper: extract RGBA bytes from ksni::Icon (ARGB network order → RGBA).
fn icon_to_rgba(icon: &ksni::Icon) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(icon.data.len());
    for px in icon.data.as_chunks::<4>().0.iter() {
        rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
    }
    rgba
}

// --- structural invariants (T16): assert what the icon draws, not just its size ---
//
// Premultiplied-alpha handling: `Pixmap` stores premultiplied RGBA and
// `icon_to_rgba` does not un-premultiply. For the fully opaque theme
// colours (alpha 255) that is a no-op, but `Theme::offline` (alpha 179)
// is stored premultiplied. Rather than un-premultiply the rendered
// buffer (which would need to replicate tiny-skia's internal rounding
// to avoid off-by-one false negatives), these tests premultiply the
// *expected* theme colour with the same tiny-skia API the renderer
// itself uses (`Color::premultiply().to_color_u8()`) and compare against
// that. This is an equality check against the library's own conversion,
// not a hardcoded byte value.

/// Distinct opaque `(r,g,b,a)` colours present in the icon, in the same
/// (premultiplied) byte representation the pixmap stores.
fn opaque_colors(icon: &ksni::Icon) -> std::collections::HashSet<(u8, u8, u8, u8)> {
    icon_to_rgba(icon)
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px[3] != 0)
        .map(|px| (px[0], px[1], px[2], px[3]))
        .collect()
}

/// Number of opaque pixels in the icon.
fn opaque_pixel_count(icon: &ksni::Icon) -> usize {
    icon_to_rgba(icon)
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px[3] != 0)
        .count()
}

/// A straight-alpha theme colour, converted to the premultiplied bytes it
/// is stored as once drawn — via the same tiny-skia conversion the
/// renderer's `solid_paint` goes through.
fn theme_color_as_drawn(c: [u8; 4]) -> (u8, u8, u8, u8) {
    let p = Color::from_rgba8(c[0], c[1], c[2], c[3])
        .premultiply()
        .to_color_u8();
    (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn ok_status_uses_normal_and_not_low_or_charging() {
    let renderer = TinySkiaRenderer { sizes: vec![22] };
    let theme = Theme::dark();
    let icons = renderer.render(
        PrimaryStatus::Ok { percent: 80 },
        None,
        &theme,
        DisplayMode::IconOnly,
        false,
    );
    let colors = opaque_colors(&icons[0]);
    assert!(
        colors.contains(&theme_color_as_drawn(theme.normal)),
        "Ok must render using theme.normal"
    );
    assert!(
        !colors.contains(&theme_color_as_drawn(theme.low)),
        "Ok must not contain theme.low"
    );
    assert!(
        !colors.contains(&theme_color_as_drawn(theme.charging)),
        "Ok must not contain theme.charging"
    );
}

#[test]
fn low_status_uses_low_and_not_normal() {
    let renderer = TinySkiaRenderer { sizes: vec![22] };
    let theme = Theme::dark();
    let icons = renderer.render(
        PrimaryStatus::Low { percent: 10 },
        None,
        &theme,
        DisplayMode::IconOnly,
        false,
    );
    let colors = opaque_colors(&icons[0]);
    assert!(
        colors.contains(&theme_color_as_drawn(theme.low)),
        "Low must render using theme.low"
    );
    assert!(
        !colors.contains(&theme_color_as_drawn(theme.normal)),
        "Low must not contain theme.normal"
    );
}

#[test]
fn charging_status_uses_charging_color() {
    let renderer = TinySkiaRenderer { sizes: vec![22] };
    let theme = Theme::dark();
    let icons = renderer.render(
        PrimaryStatus::Charging { percent: 40 },
        None,
        &theme,
        DisplayMode::IconOnly,
        false,
    );
    let colors = opaque_colors(&icons[0]);
    assert!(
        colors.contains(&theme_color_as_drawn(theme.charging)),
        "Charging must render using theme.charging"
    );
}

#[test]
fn offline_status_uses_offline_color() {
    let renderer = TinySkiaRenderer { sizes: vec![22] };
    let theme = Theme::dark();
    let icons = renderer.render(
        PrimaryStatus::Offline,
        None,
        &theme,
        DisplayMode::IconOnly,
        false,
    );
    let colors = opaque_colors(&icons[0]);
    assert!(
        colors.contains(&theme_color_as_drawn(theme.offline)),
        "Offline must render using theme.offline"
    );
}

#[test]
fn fill_pixel_count_increases_with_percent() {
    let renderer = TinySkiaRenderer { sizes: vec![22] };
    let theme = Theme::dark();
    let count_for = |percent: u8| {
        let icons = renderer.render(
            PrimaryStatus::Ok { percent },
            None,
            &theme,
            DisplayMode::IconOnly,
            false,
        );
        opaque_pixel_count(&icons[0])
    };

    let c0 = count_for(0);
    let c50 = count_for(50);
    let c100 = count_for(100);
    assert!(
        c0 < c50,
        "0% must have fewer opaque pixels than 50% ({c0} vs {c50})"
    );
    assert!(
        c50 < c100,
        "50% must have fewer opaque pixels than 100% ({c50} vs {c100})"
    );
}

#[test]
fn percent_only_digits_are_value_sensitive() {
    let renderer = TinySkiaRenderer { sizes: vec![22] };
    let theme = Theme::dark();
    let render = |percent: u8| {
        renderer
            .render(
                PrimaryStatus::Ok { percent },
                None,
                &theme,
                DisplayMode::PercentOnly,
                false,
            )
            .remove(0)
    };

    let a1 = render(42);
    let a2 = render(42);
    let b = render(87);
    assert_eq!(
        a1.data, a2.data,
        "PercentOnly: rendering the same value twice must be identical"
    );
    assert_ne!(
        a1.data, b.data,
        "PercentOnly: rendering a different value must change the buffer"
    );
}

#[test]
fn offline_is_visually_distinct_from_empty_battery() {
    let renderer = TinySkiaRenderer { sizes: vec![22] };
    let theme = Theme::dark();
    let offline = renderer.render(
        PrimaryStatus::Offline,
        None,
        &theme,
        DisplayMode::IconOnly,
        false,
    );
    let empty = renderer.render(
        PrimaryStatus::Ok { percent: 0 },
        None,
        &theme,
        DisplayMode::IconOnly,
        false,
    );
    assert_ne!(
        offline[0].data, empty[0].data,
        "offline (crossed battery) must differ from an empty online battery"
    );
}

#[test]
fn ok_status_theme_reaches_the_pixels() {
    let renderer = TinySkiaRenderer { sizes: vec![22] };
    let status = PrimaryStatus::Ok { percent: 50 };
    let dark = renderer.render(status, None, &Theme::dark(), DisplayMode::IconOnly, false);
    let light = renderer.render(status, None, &Theme::light(), DisplayMode::IconOnly, false);
    assert_ne!(
        dark[0].data, light[0].data,
        "Ok uses theme.normal, which differs between dark and light themes"
    );
}

#[test]
fn no_mode_panics_on_a_1px_or_2px_canvas() {
    let renderer = TinySkiaRenderer { sizes: vec![1, 2] };
    for mode in DisplayMode::ALL {
        for kind in KINDS {
            for status in STATUSES {
                let icons = renderer.render(status, kind, &Theme::dark(), mode, false);
                assert_eq!(icons.len(), 2, "{mode:?} {kind:?} {status:?}");
            }
        }
    }
}

// --- status without colour ----------------------------------------------

const PUBLISHED_SIZES: [u32; 5] = [22, 24, 32, 44, 64];

/// Every role in one colour: what remains is shape.
fn monochrome() -> Theme {
    let c = [0xff, 0xff, 0xff, 0xff];
    Theme {
        normal: c,
        low: c,
        charging: c,
        offline: c,
    }
}

fn masks(status: PrimaryStatus, mode: DisplayMode, stale: bool) -> Vec<Vec<bool>> {
    TinySkiaRenderer {
        sizes: PUBLISHED_SIZES.to_vec(),
    }
    .render(status, Some(DeviceKind::Mouse), &monochrome(), mode, stale)
    .iter()
    .map(|icon| {
        icon.data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[0] > 0)
            .collect()
    })
    .collect()
}

#[test]
fn charging_differs_from_ok_in_shape_and_low_in_colour_alone() {
    let renderer = TinySkiaRenderer {
        sizes: PUBLISHED_SIZES.to_vec(),
    };
    let mouse = Some(DeviceKind::Mouse);
    let theme = Theme::dark();
    for mode in DisplayMode::ALL {
        for percent in [8, 15, 88, 100] {
            let ok = masks(PrimaryStatus::Ok { percent }, mode, false);
            let charging = masks(PrimaryStatus::Charging { percent }, mode, false);
            let low = masks(PrimaryStatus::Low { percent }, mode, false);
            let ok_colour =
                renderer.render(PrimaryStatus::Ok { percent }, mouse, &theme, mode, false);
            let low_colour =
                renderer.render(PrimaryStatus::Low { percent }, mouse, &theme, mode, false);
            for (i, size) in PUBLISHED_SIZES.iter().enumerate() {
                let at = format!("{mode:?} {percent}% at {size} px");
                assert_ne!(ok[i], charging[i], "{at}: charging looks like ok");
                assert_eq!(ok[i], low[i], "{at}: low differs from ok in shape");
                assert_ne!(
                    ok_colour[i].data, low_colour[i].data,
                    "{at}: low looks like ok in colour"
                );
            }
        }
    }
}

#[test]
fn a_low_reading_keeps_the_full_size_digits() {
    let mode = DisplayMode::PercentOnly;
    for percent in [5, 8, 15, 19] {
        let ok = masks(PrimaryStatus::Ok { percent }, mode, false);
        let low = masks(PrimaryStatus::Low { percent }, mode, false);
        for (i, size) in PUBLISHED_SIZES.iter().enumerate() {
            assert_eq!(ok[i], low[i], "{percent}% at {size} px");
        }
    }
}

#[test]
fn a_retained_reading_differs_in_shape_in_every_mode() {
    for mode in DisplayMode::ALL {
        for status in [
            PrimaryStatus::Ok { percent: 88 },
            PrimaryStatus::Charging { percent: 40 },
            PrimaryStatus::Ok { percent: 100 },
        ] {
            let fresh = masks(status, mode, false);
            let stale = masks(status, mode, true);
            for (i, size) in PUBLISHED_SIZES.iter().enumerate() {
                assert_ne!(fresh[i], stale[i], "{mode:?} {status:?} at {size} px");
            }
        }
    }
}

fn alpha_at(icon: &ksni::Icon, x: i32, y: i32) -> u8 {
    if x < 0 || y < 0 || x >= icon.width || y >= icon.height {
        return 0;
    }
    icon.data[((y * icon.width + x) * 4) as usize]
}

/// Nothing drawn after the mark covers any of its pixels, and the mark
/// stays on the canvas.
#[test]
fn the_status_mark_is_drawn_whole() {
    for mode in DisplayMode::ALL {
        for status in [
            PrimaryStatus::Charging { percent: 100 },
            PrimaryStatus::Charging { percent: 40 },
        ] {
            let mark = status_mark(status).expect("a mark");
            let layout = Layout::new(mode, Some(DeviceKind::Mouse));
            for size in PUBLISHED_SIZES {
                let at = format!("{mode:?} {status:?} at {size} px");
                let area = mark_area(size, layout, mark);
                assert!(
                    area.x >= 0
                        && area.y >= 0
                        && area.right() <= size as i32
                        && area.bottom() <= size as i32,
                    "{at}: {area:?} leaves the canvas"
                );
                let icon = TinySkiaRenderer { sizes: vec![size] }
                    .render(status, Some(DeviceKind::Mouse), &Theme::dark(), mode, false)
                    .remove(0);
                for cell in cell_areas(mark, area) {
                    for y in cell.y..cell.bottom() {
                        for x in cell.x..cell.x + cell.w {
                            assert_eq!(alpha_at(&icon, x, y), 255, "{at}: ({x}, {y}) lost");
                        }
                    }
                }
            }
        }
    }
}

const SILHOUETTE_KINDS: [DeviceKind; 4] = [
    DeviceKind::Mouse,
    DeviceKind::Keyboard,
    DeviceKind::Headset,
    DeviceKind::Controller,
];

const KINDS: [Option<DeviceKind>; 6] = [
    None,
    Some(DeviceKind::Mouse),
    Some(DeviceKind::Keyboard),
    Some(DeviceKind::Headset),
    Some(DeviceKind::Controller),
    Some(DeviceKind::Other),
];

const STATUSES: [PrimaryStatus; 4] = [
    PrimaryStatus::Ok { percent: 60 },
    PrimaryStatus::Charging { percent: 60 },
    PrimaryStatus::Low { percent: 8 },
    PrimaryStatus::Offline,
];

fn icon_22(
    status: PrimaryStatus,
    kind: Option<DeviceKind>,
    mode: DisplayMode,
    stale: bool,
) -> ksni::Icon {
    TinySkiaRenderer { sizes: vec![22] }
        .render(status, kind, &monochrome(), mode, stale)
        .remove(0)
}

fn pixels(icons: Vec<ksni::Icon>) -> Vec<Vec<u8>> {
    icons.into_iter().map(|icon| icon.data).collect()
}

#[test]
fn every_kind_has_its_own_device_and_battery_icon() {
    for status in STATUSES {
        let icons = SILHOUETTE_KINDS
            .map(|kind| icon_22(status, Some(kind), DisplayMode::DeviceAndBattery, false));
        for (i, a) in icons.iter().enumerate() {
            for (b, kind) in icons.iter().zip(SILHOUETTE_KINDS).skip(i + 1) {
                assert_ne!(
                    a.data, b.data,
                    "{status:?}: {:?} looks like {kind:?}",
                    SILHOUETTE_KINDS[i]
                );
            }
        }
    }
}

/// `IconOnly` and `PercentOnly` never show the kind, and
/// `DeviceAndBattery` without a silhouette is `IconOnly`.
#[test]
fn only_device_and_battery_draws_the_kind() {
    let renderer = TinySkiaRenderer::default();
    let theme = Theme::dark();
    for status in STATUSES {
        for stale in [false, true] {
            for mode in [DisplayMode::IconOnly, DisplayMode::PercentOnly] {
                let plain = pixels(renderer.render(status, None, &theme, mode, stale));
                for kind in KINDS {
                    let icons = pixels(renderer.render(status, kind, &theme, mode, stale));
                    assert!(icons == plain, "{mode:?} {status:?} {kind:?}");
                }
            }
            let battery =
                pixels(renderer.render(status, None, &theme, DisplayMode::IconOnly, stale));
            for kind in [None, Some(DeviceKind::Other)] {
                let icons = pixels(renderer.render(
                    status,
                    kind,
                    &theme,
                    DisplayMode::DeviceAndBattery,
                    stale,
                ));
                assert!(icons == battery, "{status:?} {kind:?}");
            }
        }
    }
}

/// At 22 px the silhouette is its bitmap, one pixel per cell, in rows
/// 0–13; rows 14–15 stay clear and the bar takes rows 16–21.
#[test]
fn the_silhouette_and_the_bar_keep_to_their_rows() {
    for kind in SILHOUETTE_KINDS {
        for status in [PrimaryStatus::Ok { percent: 60 }, PrimaryStatus::Offline] {
            let icon = icon_22(status, Some(kind), DisplayMode::DeviceAndBattery, false);
            let bitmap = silhouette(kind).expect("a silhouette");
            let area = silhouette_area(22);
            assert_eq!(
                area,
                Area {
                    x: 4,
                    y: 0,
                    w: 14,
                    h: 14
                }
            );
            for y in 0..14 {
                for x in 0..22 {
                    let expected = (area.x..area.right()).contains(&x)
                        && bitmap.is_set((x - area.x) as u32, y as u32);
                    let lit = alpha_at(&icon, x, y) > 0;
                    assert_eq!(lit, expected, "{kind:?} {status:?} ({x}, {y})");
                }
            }
            for y in 14..16 {
                assert!(
                    (0..22).all(|x| alpha_at(&icon, x, y) == 0),
                    "{kind:?} row {y}"
                );
            }
            for y in 16..22 {
                assert!(
                    (0..22).any(|x| alpha_at(&icon, x, y) > 0),
                    "{kind:?} row {y}"
                );
            }
        }
    }
}

/// The fill spans columns 2–18 at 100 %: one pixel clear of the outline.
#[test]
fn the_device_bar_fill_grows_with_percent() {
    let lit = |percent| {
        let icon = icon_22(
            PrimaryStatus::Ok { percent },
            Some(DeviceKind::Mouse),
            DisplayMode::DeviceAndBattery,
            false,
        );
        (0..22).filter(|&x| alpha_at(&icon, x, 18) > 0).count()
    };
    let counts = [0, 30, 60, 100].map(lit);
    assert!(counts.windows(2).all(|w| w[0] < w[1]), "{counts:?}");
    assert_eq!(counts[3] - counts[0], 17, "{counts:?}");
}

#[test]
fn no_mode_panics_at_any_published_size() {
    let renderer = TinySkiaRenderer::default();
    for mode in DisplayMode::ALL {
        for kind in KINDS {
            for status in STATUSES {
                for stale in [false, true] {
                    let icons = renderer.render(status, kind, &Theme::dark(), mode, stale);
                    assert_eq!(
                        icons.len(),
                        renderer.sizes.len(),
                        "{mode:?} {kind:?} {status:?}"
                    );
                }
            }
        }
    }
}

/// Lit pixels as `#`, one line per row.
fn ascii(icon: &ksni::Icon) -> String {
    (0..icon.height)
        .map(|y| {
            (0..icon.width)
                .map(|x| if alpha_at(icon, x, y) > 0 { '#' } else { '.' })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every mode × kind at 22 px, then the mouse's other states in `DeviceAndBattery`.
fn ascii_sheet() -> String {
    let mut cases = Vec::new();
    for mode in DisplayMode::ALL {
        for kind in KINDS.into_iter().skip(1) {
            cases.push((mode, kind, PrimaryStatus::Ok { percent: 60 }, false));
        }
    }
    for (status, stale) in [
        (PrimaryStatus::Charging { percent: 60 }, false),
        (PrimaryStatus::Low { percent: 60 }, false),
        (PrimaryStatus::Ok { percent: 60 }, true),
        (PrimaryStatus::Offline, false),
    ] {
        let mouse = Some(DeviceKind::Mouse);
        cases.push((DisplayMode::DeviceAndBattery, mouse, status, stale));
    }
    cases
        .into_iter()
        .map(|(mode, kind, status, stale)| {
            let icon = icon_22(status, kind, mode, stale);
            let retained = if stale { " retained" } else { "" };
            format!("{mode:?} {kind:?} {status:?}{retained}\n{}\n", ascii(&icon))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct CountingRenderer(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl IconRenderer for CountingRenderer {
    fn render(
        &self,
        status: PrimaryStatus,
        kind: Option<DeviceKind>,
        theme: &Theme,
        mode: DisplayMode,
        stale: bool,
    ) -> Vec<ksni::Icon> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        TinySkiaRenderer::default().render(status, kind, theme, mode, stale)
    }
}

#[test]
fn the_cache_renders_each_key_once_while_it_is_kept() {
    let renders = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut cache = IconCache::new(Box::new(CountingRenderer(renders.clone())));
    let key = |percent| IconKey {
        status: PrimaryStatus::Ok { percent },
        kind: Some(DeviceKind::Mouse),
        theme: Theme::dark(),
        mode: DisplayMode::DeviceAndBattery,
        stale: false,
    };
    let count = || renders.load(std::sync::atomic::Ordering::Relaxed);

    let pixels = |icons: Vec<ksni::Icon>| icons.into_iter().map(|i| i.data).collect::<Vec<_>>();
    let first = pixels(cache.icons(&key(50)));
    assert_eq!(
        pixels(cache.icons(&key(50))),
        first,
        "a hit returns what was rendered"
    );
    assert_eq!(count(), 1);

    cache.icons(&key(49));
    assert_eq!(count(), 2, "a new key renders");

    cache.retain(|k| *k == key(49));
    cache.icons(&key(49));
    assert_eq!(count(), 2, "a kept key stays cached");
    cache.icons(&key(50));
    assert_eq!(count(), 3, "a dropped key renders again");
}
