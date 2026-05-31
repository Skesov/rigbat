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

    match mode {
        DisplayMode::IconOnly => {
            let mut pixmap = Pixmap::new(size, size)?;
            draw_battery(&mut pixmap, fill_ratio, color, false);
            Some(pixmap_to_icon(pixmap))
        }
        DisplayMode::PercentOnly => {
            let mut pixmap = Pixmap::new(size, size)?;
            draw_percent_centered(&mut pixmap, percent, color);
            Some(pixmap_to_icon(pixmap))
        }
        DisplayMode::PercentInIcon => {
            let mut pixmap = Pixmap::new(size, size)?;
            // Draw battery outline only (no fill bar — number takes priority).
            draw_battery_outline_only(&mut pixmap, color);
            draw_percent_in_battery(&mut pixmap, percent, color);
            Some(pixmap_to_icon(pixmap))
        }
        DisplayMode::PercentBesideIcon => {
            // Canvas is 2× wide: battery on the left half, number on the right.
            let width = size * 2;
            let mut pixmap = Pixmap::new(width, size)?;
            draw_battery_in_region(&mut pixmap, 0.0, 0.0, size as f32, fill_ratio, color, false);
            draw_percent_in_region(
                &mut pixmap,
                size as f32,
                0.0,
                size as f32,
                size as f32,
                percent,
                color,
            );
            Some(pixmap_to_icon(pixmap))
        }
    }
}

/// Renders an offline (crossed) battery icon of size `n×n`.
fn render_offline_battery(n: u32, color_rgba: [u8; 4]) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(n, n)?;
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
    let g = battery_geometry(pixmap.width() as f32);
    draw_battery_fill(pixmap, &g, fill_ratio, color);
    draw_battery_outline(pixmap, &g, color);
    draw_battery_nub(pixmap, &g, color);
    if is_offline {
        draw_cross_line(pixmap, &g, color);
    }
}

/// Draws only the battery outline + nub (no fill bar). Used for `PercentInIcon`.
fn draw_battery_outline_only(pixmap: &mut Pixmap, color: Color) {
    let g = battery_geometry(pixmap.width() as f32);
    draw_battery_outline(pixmap, &g, color);
    draw_battery_nub(pixmap, &g, color);
}

/// Draws a battery (outline + fill + nub + optional cross) into an arbitrary
/// square region starting at `(rx, ry)` with side `side` inside `pixmap`.
/// Used for `PercentBesideIcon`.
fn draw_battery_in_region(
    pixmap: &mut Pixmap,
    rx: f32,
    ry: f32,
    side: f32,
    fill_ratio: f32,
    color: Color,
    is_offline: bool,
) {
    let mut g = battery_geometry(side);
    g.bx += rx;
    g.by += ry;
    draw_battery_fill(pixmap, &g, fill_ratio, color);
    draw_battery_outline(pixmap, &g, color);
    draw_battery_nub(pixmap, &g, color);
    if is_offline {
        draw_cross_line(pixmap, &g, color);
    }
}

/// Geometry of the battery body within a square canvas of side `s`.
struct BatteryGeom {
    bx: f32,
    by: f32,
    bw: f32,
    bh: f32,
    sw: f32,
    nub_w: f32,
    nub_h: f32,
}

/// Computes battery geometry proportional to a square of side `s`.
fn battery_geometry(s: f32) -> BatteryGeom {
    BatteryGeom {
        bx: s * 0.05,
        by: s * 0.31,
        bw: s * 0.80,
        bh: s * 0.38,
        sw: (s * 0.06).max(1.0),
        nub_w: s * 0.06,
        nub_h: s * 0.16,
    }
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
        let mut paint = Paint::default();
        paint.set_color(color);
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

fn draw_battery_outline(pixmap: &mut Pixmap, g: &BatteryGeom, color: Color) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;

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
        let mut paint = Paint::default();
        paint.set_color(color);
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
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;

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

/// Draws the decimal digits of `value` (0–100) starting at pixel `(x, y)`.
/// Each font cell is `cell` pixels wide and tall. Digits are 3 cells wide,
/// 5 cells tall, separated by a 1-cell gap.
fn draw_number(pixmap: &mut Pixmap, value: u8, x: f32, y: f32, cell: f32, color: Color) {
    if cell < 1.0 {
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

    let digit_stride = 3.0 * cell + cell; // 3 columns + 1-cell gap
    let mut paint = Paint::default();
    paint.set_color(color);

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
fn draw_percent_centered(pixmap: &mut Pixmap, value: u8, color: Color) {
    let w = pixmap.width() as f32;
    let h = pixmap.height() as f32;

    let digit_count = if value >= 100 {
        3
    } else if value >= 10 {
        2
    } else {
        1
    };

    // cell chosen so 5 rows fit in 80 % of the canvas height.
    let cell = (h * 0.80 / 5.0).floor().max(1.0);
    let total_w = digit_count as f32 * 3.0 * cell + (digit_count - 1) as f32 * cell;
    let total_h = 5.0 * cell;

    let x = ((w - total_w) / 2.0).max(0.0);
    let y = ((h - total_h) / 2.0).max(0.0);
    draw_number(pixmap, value, x, y, cell, color);
}

/// Draws `value` inside the battery body (`PercentInIcon` mode).
fn draw_percent_in_battery(pixmap: &mut Pixmap, value: u8, color: Color) {
    let s = pixmap.width() as f32;
    // Battery inner area (inset by stroke width).
    let sw = (s * 0.06).max(1.0);
    let inner_x = s * 0.05 + sw;
    let inner_y = s * 0.31 + sw;
    let inner_w = s * 0.80 - sw * 2.0;
    let inner_h = s * 0.38 - sw * 2.0;

    draw_percent_in_region(pixmap, inner_x, inner_y, inner_w, inner_h, value, color);
}

/// Draws `value` centered inside an arbitrary region `(rx, ry, rw, rh)`.
fn draw_percent_in_region(
    pixmap: &mut Pixmap,
    rx: f32,
    ry: f32,
    rw: f32,
    rh: f32,
    value: u8,
    color: Color,
) {
    let digit_count = if value >= 100 {
        3
    } else if value >= 10 {
        2
    } else {
        1
    };

    // Fit 5 rows (height) and `digit_count*3 + gaps` columns (width).
    let cell_from_h = (rh / 5.0).floor().max(1.0);
    let total_w_at_cell =
        digit_count as f32 * 3.0 * cell_from_h + (digit_count - 1) as f32 * cell_from_h;
    let cell = if total_w_at_cell <= rw {
        cell_from_h
    } else {
        // Scale down so width fits.
        let cols = digit_count as f32 * 3.0 + (digit_count - 1) as f32;
        (rw / cols).floor().max(1.0)
    };

    let total_w = digit_count as f32 * 3.0 * cell + (digit_count - 1) as f32 * cell;
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
            assert_eq!(icon.width, icon.height, "must be square");
            assert!(sizes.contains(&(icon.width as u32)));
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

    // --- mode: PercentBesideIcon ----------------------------------------

    #[test]
    fn percent_beside_icon_renders_correct_count() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            &Theme::dark(),
            DisplayMode::PercentBesideIcon,
        );
        assert_eq!(icons.len(), default_sizes().len());
    }

    #[test]
    fn percent_beside_icon_width_is_double_height() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            &Theme::dark(),
            DisplayMode::PercentBesideIcon,
        );
        for icon in &icons {
            assert_eq!(
                icon.width,
                icon.height * 2,
                "PercentBesideIcon: width must be 2×height, got {}×{}",
                icon.width,
                icon.height
            );
        }
    }

    #[test]
    fn percent_beside_icon_data_len() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            &Theme::dark(),
            DisplayMode::PercentBesideIcon,
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
            DisplayMode::PercentBesideIcon,
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
