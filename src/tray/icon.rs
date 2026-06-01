// This module is part of the tray icon infrastructure used in T4b/T4c.
// Types are public API but not yet wired into main, hence dead_code for now.
#![allow(dead_code)]

use crate::config::DisplayMode;
use crate::domain::PrimaryStatus;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

pub trait IconRenderer: Send + Sync {
    fn render(&self, status: PrimaryStatus, theme: &Theme, mode: DisplayMode) -> Vec<ksni::Icon>;
}

/// Theme colors in RGBA format (straight alpha).
pub struct Theme {
    pub normal: [u8; 4],
    pub low: [u8; 4],
    pub charging: [u8; 4],
    pub offline: [u8; 4],
}

impl Theme {
    /// Dark theme: light foreground (Nord off-white).
    pub fn dark() -> Self {
        Self {
            normal: [216, 222, 233, 255],
            low: [191, 97, 106, 255],
            charging: [163, 190, 140, 255],
            offline: [216, 222, 233, 180],
        }
    }

    /// Light theme: dark foreground (Nord polar night).
    pub fn light() -> Self {
        Self {
            normal: [59, 66, 82, 255],
            low: [191, 97, 106, 255],
            charging: [163, 190, 140, 255],
            offline: [59, 66, 82, 180],
        }
    }
}

pub struct TinySkiaRenderer {
    pub sizes: Vec<u32>,
}

impl Default for TinySkiaRenderer {
    fn default() -> Self {
        Self {
            sizes: vec![22, 24, 32, 44, 64],
        }
    }
}

impl IconRenderer for TinySkiaRenderer {
    fn render(&self, status: PrimaryStatus, theme: &Theme, mode: DisplayMode) -> Vec<ksni::Icon> {
        let (color_rgba, percent, is_offline) = match status {
            PrimaryStatus::Offline => (theme.offline, None, true),
            PrimaryStatus::Charging { percent } => (theme.charging, Some(percent), false),
            PrimaryStatus::Low { percent } => (theme.low, Some(percent), false),
            PrimaryStatus::Ok { percent } => (theme.normal, Some(percent), false),
        };

        // Offline always draws the offline battery regardless of mode.
        if is_offline {
            return self
                .sizes
                .iter()
                .filter_map(|&size| render_offline_battery(size, color_rgba))
                .collect();
        }

        let fill_ratio = percent.map_or(0.0, |p| f32::from(p) / 100.0);
        let pct = percent.unwrap_or(0);

        self.sizes
            .iter()
            .filter_map(|&size| render_mode(size, color_rgba, fill_ratio, pct, mode))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Per-mode dispatch
// ---------------------------------------------------------------------------

/// SNI hosts (e.g. COSMIC) fit the icon into a roughly square slot, so a wider
/// pixmap does not gain width — it only shrinks. Keep the canvas square and fill it.
const WIDE_ASPECT: f32 = 1.0;

/// Canvas width for a given icon height.
fn wide_width(height: u32) -> u32 {
    ((height as f32) * WIDE_ASPECT).round() as u32
}

/// Renders one icon for the given mode and size. Returns `None` only if
/// `Pixmap::new` fails (does not happen for sizes ≤ 64).
fn render_mode(
    size: u32,
    color_rgba: [u8; 4],
    fill_ratio: f32,
    percent: u8,
    mode: DisplayMode,
) -> Option<ksni::Icon> {
    let [r, g, b, a] = color_rgba;
    let color = Color::from_rgba8(r, g, b, a);
    let w = wide_width(size);

    match mode {
        DisplayMode::IconOnly => {
            let mut pixmap = Pixmap::new(w, size)?;
            draw_battery(&mut pixmap, fill_ratio, color, false);
            Some(pixmap_to_icon(pixmap))
        }
        DisplayMode::PercentOnly => {
            let mut pixmap = Pixmap::new(w, size)?;
            draw_percent_centered(&mut pixmap, percent, color);
            Some(pixmap_to_icon(pixmap))
        }
        DisplayMode::PercentInIcon => {
            let mut pixmap = Pixmap::new(w, size)?;
            // Draw battery outline only (no fill bar — number takes priority).
            draw_battery_outline_only(&mut pixmap, color);
            draw_percent_in_battery(&mut pixmap, percent, color);
            Some(pixmap_to_icon(pixmap))
        }
    }
}

/// Renders an offline (crossed) battery icon, matching the online width.
fn render_offline_battery(n: u32, color_rgba: [u8; 4]) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(wide_width(n), n)?;
    let [r, g, b, a] = color_rgba;
    let color = Color::from_rgba8(r, g, b, a);
    draw_battery(&mut pixmap, 0.0, color, true);
    Some(pixmap_to_icon(pixmap))
}

// ---------------------------------------------------------------------------
// Battery drawing helpers
// ---------------------------------------------------------------------------

/// Draws the full battery (outline + fill + nub + optional cross line)
/// into `pixmap`. The battery occupies the full pixmap area.
fn draw_battery(pixmap: &mut Pixmap, fill_ratio: f32, color: Color, is_offline: bool) {
    let g = battery_geometry(pixmap.width() as f32, pixmap.height() as f32);
    draw_battery_fill(pixmap, &g, fill_ratio, color);
    draw_battery_outline(pixmap, &g, color);
    draw_battery_nub(pixmap, &g, color);
    if is_offline {
        draw_cross_line(pixmap, &g, color);
    }
}

/// Draws only the battery outline + nub (no fill bar). Used for `PercentInIcon`.
fn draw_battery_outline_only(pixmap: &mut Pixmap, color: Color) {
    let g = battery_geometry(pixmap.width() as f32, pixmap.height() as f32);
    draw_battery_outline(pixmap, &g, color);
    draw_battery_nub(pixmap, &g, color);
}

/// Geometry of the battery body within a `w`×`h` canvas.
struct BatteryGeom {
    bx: f32,
    by: f32,
    bw: f32,
    bh: f32,
    sw: f32,
    nub_w: f32,
    nub_h: f32,
}

/// Computes battery geometry centered in a `w`×`h` canvas. The body fills most
/// of the canvas so the icon matches the visual weight of neighbouring tray
/// icons instead of looking small.
fn battery_geometry(w: f32, h: f32) -> BatteryGeom {
    let bw = w * 0.76;
    let bh = h * 0.80;
    let nub_w = (w * 0.12).max(2.0);
    let nub_h = h * 0.40;
    let sw = (h * 0.08).max(1.5);
    let total_w = bw + nub_w;
    BatteryGeom {
        bx: ((w - total_w) / 2.0).max(0.0),
        by: ((h - bh) / 2.0).max(0.0),
        bw,
        bh,
        sw,
        nub_w,
        nub_h,
    }
}

/// A solid-color paint with anti-aliasing disabled. AA is unnecessary for the
/// axis-aligned rectangles we draw and tiny-skia's AA path can panic on very
/// thin rects at small icon sizes, which would crash the tray.
fn solid_paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = false;
    paint
}

fn draw_battery_fill(pixmap: &mut Pixmap, g: &BatteryGeom, fill_ratio: f32, color: Color) {
    if fill_ratio <= 0.0 {
        return;
    }
    let half = g.sw / 2.0;
    let inner_x = g.bx + half;
    let inner_y = g.by + half;
    let inner_h = g.bh - g.sw;
    let inner_w = (g.bw - g.sw) * fill_ratio;
    if inner_w <= 0.0 || inner_h <= 0.0 {
        return;
    }
    if let Some(rect) = Rect::from_xywh(inner_x, inner_y, inner_w, inner_h) {
        let paint = solid_paint(color);
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

fn draw_battery_outline(pixmap: &mut Pixmap, g: &BatteryGeom, color: Color) {
    let paint = solid_paint(color);

    let stroke = Stroke {
        width: g.sw,
        line_cap: LineCap::Square,
        line_join: LineJoin::Miter,
        ..Stroke::default()
    };

    let mut pb = PathBuilder::new();
    pb.move_to(g.bx, g.by);
    pb.line_to(g.bx + g.bw, g.by);
    pb.line_to(g.bx + g.bw, g.by + g.bh);
    pb.line_to(g.bx, g.by + g.bh);
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn draw_battery_nub(pixmap: &mut Pixmap, g: &BatteryGeom, color: Color) {
    let nub_x = g.bx + g.bw;
    let nub_y = g.by + (g.bh - g.nub_h) / 2.0;

    if let Some(rect) = Rect::from_xywh(nub_x, nub_y, g.nub_w, g.nub_h) {
        let path = PathBuilder::from_rect(rect);
        let paint = solid_paint(color);
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn draw_cross_line(pixmap: &mut Pixmap, g: &BatteryGeom, color: Color) {
    let paint = solid_paint(color);

    let stroke = Stroke {
        width: g.sw,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };

    let mut pb = PathBuilder::new();
    pb.move_to(g.bx, g.by);
    pb.line_to(g.bx + g.bw, g.by + g.bh);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

// ---------------------------------------------------------------------------
// Percent / digit drawing
// ---------------------------------------------------------------------------

/// 3×5 bitmap for digits 0–9 (rows top→bottom, columns left→right).
/// Each entry is a 5-element array of row bitmasks (3 bits wide, MSB = leftmost column).
const DIGITS: [[u8; 5]; 10] = [
    // 0: full rectangle, hollow centre
    [0b111, 0b101, 0b101, 0b101, 0b111],
    // 1: right column only
    [0b010, 0b110, 0b010, 0b010, 0b111],
    // 2: top, middle, bottom with turns
    [0b111, 0b001, 0b111, 0b100, 0b111],
    // 3: top, middle, bottom aligned right
    [0b111, 0b001, 0b111, 0b001, 0b111],
    // 4: sides top, join middle, right column bottom
    [0b101, 0b101, 0b111, 0b001, 0b001],
    // 5: top-left, middle, bottom-right
    [0b111, 0b100, 0b111, 0b001, 0b111],
    // 6: top-left, middle, full bottom
    [0b111, 0b100, 0b111, 0b101, 0b111],
    // 7: top, right column
    [0b111, 0b001, 0b001, 0b001, 0b001],
    // 8: full rectangle with middle
    [0b111, 0b101, 0b111, 0b101, 0b111],
    // 9: full top, middle, bottom-right
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// Gap between adjacent digits, as a fraction of one font cell.
const DIGIT_GAP: f32 = 0.4;

/// Total width of `n` digits at the given `cell` size (columns + gaps).
fn digits_width(n: usize, cell: f32) -> f32 {
    n as f32 * 3.0 * cell + (n.saturating_sub(1)) as f32 * DIGIT_GAP * cell
}

/// Number of decimal digits used to render `value` (0–100).
fn digit_count(value: u8) -> usize {
    if value >= 100 {
        3
    } else if value >= 10 {
        2
    } else {
        1
    }
}

/// Draws the decimal digits of `value` (0–100) starting at pixel `(x, y)`.
/// Each font cell is `cell` pixels wide and tall. Digits are 3 cells wide,
/// 5 cells tall, separated by `DIGIT_GAP` of a cell.
fn draw_number(pixmap: &mut Pixmap, value: u8, x: f32, y: f32, cell: f32, color: Color) {
    if cell <= 0.0 {
        return;
    }

    // Build the list of digit indices for `value` (left to right).
    let digits: Vec<usize> = if value >= 100 {
        vec![1, 0, 0]
    } else if value >= 10 {
        vec![(value / 10) as usize, (value % 10) as usize]
    } else {
        vec![value as usize]
    };

    let digit_stride = 3.0 * cell + DIGIT_GAP * cell; // 3 columns + gap
    let paint = solid_paint(color);

    for (i, &d) in digits.iter().enumerate() {
        let ox = x + i as f32 * digit_stride;
        for (row, &mask) in DIGITS[d].iter().enumerate() {
            let oy = y + row as f32 * cell;
            for col in 0..3_u8 {
                // MSB of the 3-bit mask is the leftmost column.
                if mask & (0b100 >> col) != 0 {
                    let px = ox + col as f32 * cell;
                    if let Some(rect) = Rect::from_xywh(px, oy, cell, cell) {
                        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                    }
                }
            }
        }
    }
}

/// Draws `value` centered in the full pixmap canvas (`PercentOnly` mode).
/// Digits fill most of the canvas for readability at small panel sizes.
fn draw_percent_centered(pixmap: &mut Pixmap, value: u8, color: Color) {
    let w = pixmap.width() as f32;
    let h = pixmap.height() as f32;
    draw_percent_in_region(pixmap, w * 0.05, h * 0.05, w * 0.90, h * 0.90, value, color);
}

/// Draws `value` inside the battery body (`PercentInIcon` mode).
fn draw_percent_in_battery(pixmap: &mut Pixmap, value: u8, color: Color) {
    let g = battery_geometry(pixmap.width() as f32, pixmap.height() as f32);
    let inner_x = g.bx + g.sw;
    let inner_y = g.by + g.sw;
    let inner_w = g.bw - g.sw * 2.0;
    let inner_h = g.bh - g.sw * 2.0;
    draw_percent_in_region(pixmap, inner_x, inner_y, inner_w, inner_h, value, color);
}

/// Draws `value` centered inside an arbitrary region `(rx, ry, rw, rh)`,
/// sized as large as fits in both width and height.
fn draw_percent_in_region(
    pixmap: &mut Pixmap,
    rx: f32,
    ry: f32,
    rw: f32,
    rh: f32,
    value: u8,
    color: Color,
) {
    let n = digit_count(value);

    // Largest cell that fits 5 rows in height and the digit block in width.
    let cell_from_h = rh / 5.0;
    let cols = n as f32 * 3.0 + (n.saturating_sub(1)) as f32 * DIGIT_GAP;
    let cell_from_w = rw / cols;
    let cell = cell_from_h.min(cell_from_w).max(0.0);

    let total_w = digits_width(n, cell);
    let total_h = 5.0 * cell;

    let x = rx + ((rw - total_w) / 2.0).max(0.0);
    let y = ry + ((rh - total_h) / 2.0).max(0.0);
    draw_number(pixmap, value, x, y, cell, color);
}

// ---------------------------------------------------------------------------
// Pixmap → ksni::Icon conversion
// ---------------------------------------------------------------------------

/// Converts tiny-skia premultiplied RGBA to ARGB32 network byte order (A,R,G,B).
fn pixmap_to_icon(pixmap: Pixmap) -> ksni::Icon {
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let rgba_data = pixmap.data();
    let mut argb_data = Vec::with_capacity(rgba_data.len());
    for chunk in rgba_data.chunks_exact(4) {
        argb_data.push(chunk[3]); // A
        argb_data.push(chunk[0]); // R
        argb_data.push(chunk[1]); // G
        argb_data.push(chunk[2]); // B
    }
    ksni::Icon {
        width: w,
        height: h,
        data: argb_data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DisplayMode;

    fn default_sizes() -> Vec<u32> {
        TinySkiaRenderer::default().sizes.clone()
    }

    /// Debug helper: dump every mode to /tmp as PNG for visual inspection.
    /// Run with: `cargo test dump_icons -- --ignored`.
    #[test]
    #[ignore]
    fn dump_icons() {
        use tiny_skia::{IntSize, Pixmap};
        let renderer = TinySkiaRenderer { sizes: vec![128] };
        let cases = [
            ("icon_only", DisplayMode::IconOnly),
            ("percent_only", DisplayMode::PercentOnly),
            ("percent_in_icon", DisplayMode::PercentInIcon),
        ];
        for (name, mode) in cases {
            let icons = renderer.render(PrimaryStatus::Ok { percent: 90 }, &Theme::dark(), mode);
            let icon = &icons[0];
            // Convert ARGB (network byte order) back to premultiplied RGBA.
            let mut rgba = Vec::with_capacity(icon.data.len());
            for px in icon.data.chunks_exact(4) {
                rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
            }
            let size = IntSize::from_wh(icon.width as u32, icon.height as u32).unwrap();
            let pm = Pixmap::from_vec(rgba, size).unwrap();
            pm.save_png(format!("/tmp/rigbat_{name}.png")).unwrap();
        }
    }

    // --- mode: IconOnly --------------------------------------------------

    #[test]
    fn icon_only_renders_correct_count() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            &Theme::dark(),
            DisplayMode::IconOnly,
        );
        assert_eq!(icons.len(), default_sizes().len());
    }

    #[test]
    fn icon_only_dimensions_and_data_len() {
        let renderer = TinySkiaRenderer::default();
        let sizes = renderer.sizes.clone();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            &Theme::dark(),
            DisplayMode::IconOnly,
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
            &Theme::dark(),
            DisplayMode::PercentOnly,
        );
        assert_eq!(icons.len(), default_sizes().len());
    }

    #[test]
    fn percent_only_data_len() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            &Theme::dark(),
            DisplayMode::PercentOnly,
        );
        for icon in &icons {
            assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        }
    }

    // --- mode: PercentInIcon --------------------------------------------

    #[test]
    fn percent_in_icon_renders_correct_count() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            &Theme::dark(),
            DisplayMode::PercentInIcon,
        );
        assert_eq!(icons.len(), default_sizes().len());
    }

    #[test]
    fn percent_in_icon_data_len() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            &Theme::dark(),
            DisplayMode::PercentInIcon,
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
            &Theme::dark(),
            DisplayMode::PercentOnly,
        );
        assert_eq!(icons.len(), default_sizes().len());
        for icon in &icons {
            assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        }
    }

    #[test]
    fn offline_valid_in_all_modes() {
        let renderer = TinySkiaRenderer::default();
        for mode in [
            DisplayMode::IconOnly,
            DisplayMode::PercentOnly,
            DisplayMode::PercentInIcon,
        ] {
            let icons = renderer.render(PrimaryStatus::Offline, &Theme::dark(), mode);
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

    // --- digit table -----------------------------------------------------

    #[test]
    fn digit_table_has_ten_entries() {
        assert_eq!(DIGITS.len(), 10);
    }

    #[test]
    fn each_digit_has_five_rows() {
        for (i, digit) in DIGITS.iter().enumerate() {
            assert_eq!(digit.len(), 5, "digit {i} must have 5 rows");
        }
    }

    #[test]
    fn draw_number_produces_non_transparent_pixels() {
        let size = 32_u32;
        let mut pixmap = Pixmap::new(size, size).unwrap();
        let color = Color::from_rgba8(255, 255, 255, 255);
        draw_number(&mut pixmap, 5, 2.0, 2.0, 3.0, color);

        // At least one pixel must be non-transparent after drawing.
        let has_opaque = pixmap.data().chunks_exact(4).any(|px| px[3] != 0);
        assert!(has_opaque, "draw_number produced no visible pixels");
    }

    #[test]
    fn draw_number_100_fits_three_digits() {
        let size = 64_u32;
        let mut pixmap = Pixmap::new(size, size).unwrap();
        let color = Color::from_rgba8(255, 255, 255, 255);
        // Cell of 4 px → three digits use 3*(3*4 + 4) - 4 = 44 px wide, fits in 64.
        draw_number(&mut pixmap, 100, 0.0, 0.0, 4.0, color);
        let has_opaque = pixmap.data().chunks_exact(4).any(|px| px[3] != 0);
        assert!(has_opaque);
    }

    // --- legacy-compatible tests (themes, charging, low) -----------------

    #[test]
    fn offline_renders_valid_icons() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Offline,
            &Theme::dark(),
            DisplayMode::IconOnly,
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
            &Theme::dark(),
            DisplayMode::IconOnly,
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
            &Theme::dark(),
            DisplayMode::IconOnly,
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
        let dark_icons = renderer.render(status, &Theme::dark(), DisplayMode::IconOnly);
        let light_icons = renderer.render(status, &Theme::light(), DisplayMode::IconOnly);

        assert!(!dark_icons.is_empty());
        assert!(!light_icons.is_empty());
        assert_ne!(&dark_icons[0].data, &light_icons[0].data);
    }
}
