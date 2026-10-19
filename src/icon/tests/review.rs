use super::*;

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
