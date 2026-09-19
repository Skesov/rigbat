use crate::config::DisplayMode;
use crate::domain::{DeviceKind, PrimaryStatus};
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

pub trait IconRenderer: Send + Sync {
    fn render(
        &self,
        status: PrimaryStatus,
        kind: Option<DeviceKind>,
        theme: &Theme,
        mode: DisplayMode,
        stale: bool,
    ) -> Vec<ksni::Icon>;
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
            // 8.18:1 against a #1e1e1e panel — well clear of the 3:1 WCAG
            // 2.1 SC 1.4.11 floor for graphical objects.
            charging: [163, 190, 140, 255],
            offline: [216, 222, 233, 180],
        }
    }

    /// Light theme: dark foreground (Nord polar night).
    pub fn light() -> Self {
        Self {
            normal: [59, 66, 82, 255],
            low: [191, 97, 106, 255],
            // The Nord green (#a3be8c) used on dark only reaches 1.79:1
            // against a #f0f0f0 panel, well under the 3:1 WCAG 2.1
            // SC 1.4.11 floor for graphical objects. This darker shade
            // reaches 6.19:1, and still clears the floor at 3.23:1 once the
            // fill is dimmed by STALE_ALPHA — which is the case that has to
            // pass, since in IconOnly mode the fill level *is* the reading.
            // Do not swap it for the dark-theme value.
            charging: [69, 96, 53, 255],
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

/// Alpha multiplier applied to the fill bar of a stale-but-known reading.
/// 0.38 (the Material/Apple convention) is for *disabled controls*, which
/// WCAG 2.1 SC 1.4.11 exempts from contrast — a stale battery reading is
/// still information the user must read, not a control, so that convention
/// does not apply here. 0.70 keeps the outline/digit colours (never dimmed,
/// see below) reading clearly muted at 22 px while every meaning-bearing
/// mark stays at full contrast.
const STALE_ALPHA: f32 = 0.70;

impl IconRenderer for TinySkiaRenderer {
    fn render(
        &self,
        status: PrimaryStatus,
        kind: Option<DeviceKind>,
        theme: &Theme,
        mode: DisplayMode,
        stale: bool,
    ) -> Vec<ksni::Icon> {
        let (color_rgba, percent, is_offline) = match status {
            PrimaryStatus::Offline => (theme.offline, None, true),
            PrimaryStatus::Charging { percent } => (theme.charging, Some(percent), false),
            PrimaryStatus::Low { percent } => (theme.low, Some(percent), false),
            PrimaryStatus::Ok { percent } => (theme.normal, Some(percent), false),
        };

        // Offline always draws the offline battery regardless of mode.
        // `Offline` + `stale` cannot happen by construction (resolve_for
        // never sets stale alongside Offline), but if it did, this path
        // draws the plain offline battery rather than dimming an
        // already-empty icon.
        if is_offline {
            return self
                .sizes
                .iter()
                .filter_map(|&size| render_offline_battery(size, color_rgba, kind))
                .collect();
        }

        // A low battery that has gone stale is exactly the reading that must
        // not get quieter, so `Low` is never dimmed even when stale. Every
        // other status dims the fill bar only — the outline and the digits
        // carry the reading and stay at full colour/contrast.
        let dim_fill = stale && !matches!(status, PrimaryStatus::Low { .. });
        let fill_rgba = if dim_fill {
            let [r, g, b, a] = color_rgba;
            let scaled = (f32::from(a) * STALE_ALPHA).round().clamp(0.0, 255.0) as u8;
            [r, g, b, scaled]
        } else {
            color_rgba
        };

        let fill_ratio = percent.map_or(0.0, |p| f32::from(p) / 100.0);
        let pct = percent.unwrap_or(0);

        self.sizes
            .iter()
            .filter_map(|&size| {
                render_mode(size, color_rgba, fill_rgba, fill_ratio, pct, mode, kind)
            })
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

/// Renders one icon for the given mode and size. `fill_rgba` is the colour
/// used for the fill bar (may be dimmed for a stale reading); `color_rgba`
/// is the full colour used for the outline, nub, digits and glyph, which are
/// never dimmed. Returns `None` only if `Pixmap::new` fails (does not happen
/// for sizes ≤ 64).
fn render_mode(
    size: u32,
    color_rgba: [u8; 4],
    fill_rgba: [u8; 4],
    fill_ratio: f32,
    percent: u8,
    mode: DisplayMode,
    kind: Option<DeviceKind>,
) -> Option<ksni::Icon> {
    let [r, g, b, a] = color_rgba;
    let color = Color::from_rgba8(r, g, b, a);
    let [fr, fg, fb, fa] = fill_rgba;
    let fill_color = Color::from_rgba8(fr, fg, fb, fa);
    let w = wide_width(size);

    match mode {
        DisplayMode::IconOnly => {
            let mut pixmap = Pixmap::new(w, size)?;
            draw_battery(&mut pixmap, fill_ratio, fill_color, color, false);
            maybe_draw_kind_glyph(&mut pixmap, kind, size, color);
            Some(pixmap_to_icon(pixmap))
        }
        DisplayMode::PercentOnly => {
            let mut pixmap = Pixmap::new(w, size)?;
            draw_percent_centered(&mut pixmap, percent, color);
            maybe_draw_kind_glyph(&mut pixmap, kind, size, color);
            Some(pixmap_to_icon(pixmap))
        }
        DisplayMode::PercentInIcon => {
            let mut pixmap = Pixmap::new(w, size)?;
            // Draw battery outline only (no fill bar — number takes priority).
            draw_battery_outline_only(&mut pixmap, color);
            draw_percent_in_battery(&mut pixmap, percent, color);
            maybe_draw_kind_glyph(&mut pixmap, kind, size, color);
            Some(pixmap_to_icon(pixmap))
        }
    }
}

/// Renders an offline (crossed) battery icon, matching the online width.
/// Offline is never dimmed, so fill and mark share one colour.
fn render_offline_battery(
    n: u32,
    color_rgba: [u8; 4],
    kind: Option<DeviceKind>,
) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(wide_width(n), n)?;
    let [r, g, b, a] = color_rgba;
    let color = Color::from_rgba8(r, g, b, a);
    draw_battery(&mut pixmap, 0.0, color, color, true);
    maybe_draw_kind_glyph(&mut pixmap, kind, n, color);
    Some(pixmap_to_icon(pixmap))
}

// ---------------------------------------------------------------------------
// Battery drawing helpers
// ---------------------------------------------------------------------------

/// Draws the full battery (outline + fill + nub + optional cross line)
/// into `pixmap`. The battery occupies the full pixmap area. `fill_color`
/// paints the fill bar only; `mark_color` paints the outline, nub and cross
/// line — the meaning-bearing marks, which are never dimmed.
fn draw_battery(
    pixmap: &mut Pixmap,
    fill_ratio: f32,
    fill_color: Color,
    mark_color: Color,
    is_offline: bool,
) {
    let g = battery_geometry(pixmap.width() as f32, pixmap.height() as f32);
    draw_battery_fill(pixmap, &g, fill_ratio, fill_color);
    draw_battery_outline(pixmap, &g, mark_color);
    draw_battery_nub(pixmap, &g, mark_color);
    if is_offline {
        draw_cross_line(pixmap, &g, mark_color);
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
// Device-kind corner glyph
// ---------------------------------------------------------------------------

/// Calls `draw_kind_glyph` only for drawable kinds; no-op for `None`/`Other`.
///
/// The glyph occupies the bottom-right corner so it does not collide with the
/// centered digit block in PercentOnly/PercentInIcon modes.
fn maybe_draw_kind_glyph(pixmap: &mut Pixmap, kind: Option<DeviceKind>, size: u32, color: Color) {
    let k = match kind {
        Some(DeviceKind::Mouse) => DeviceKind::Mouse,
        Some(DeviceKind::Keyboard) => DeviceKind::Keyboard,
        Some(DeviceKind::Headset) => DeviceKind::Headset,
        Some(DeviceKind::Controller) => DeviceKind::Controller,
        _ => return, // None or Other → no glyph
    };

    let glyph = ((size as f32 / 3.0).round() as u32).max(6);
    let margin = ((size as f32 / 22.0).round() as u32).max(1);

    // At 22px the glyph (7px) fits in the corner without touching the centered
    // digit block (which occupies the top and centre rows), so no skip needed.
    let x0 = size.saturating_sub(glyph + margin);
    let y0 = size.saturating_sub(glyph + margin);

    draw_kind_glyph(pixmap, k, x0, y0, glyph, color);
}

/// Draws a single-color device-kind silhouette in the box at `(x0, y0, glyph×glyph)`.
///
/// Step 1: punch a 1px transparent ring around the box so the glyph reads
/// against a full-charge battery fill without a background square.
/// Step 2: draw the silhouette in `color` using filled `Rect`s, `anti_alias = false`.
fn draw_kind_glyph(
    pixmap: &mut Pixmap,
    kind: DeviceKind,
    x0: u32,
    y0: u32,
    glyph: u32,
    color: Color,
) {
    // --- knockout ring (1px transparent border around the glyph box) ---
    let pw = pixmap.width();
    let ph = pixmap.height();
    let ko_x = x0.saturating_sub(1);
    let ko_y = y0.saturating_sub(1);
    let ko_w = (glyph + 2).min(pw.saturating_sub(ko_x));
    let ko_h = (glyph + 2).min(ph.saturating_sub(ko_y));
    if ko_w > 0
        && ko_h > 0
        && let Some(rect) = Rect::from_xywh(ko_x as f32, ko_y as f32, ko_w as f32, ko_h as f32)
    {
        let mut paint = Paint::default();
        paint.set_color(Color::TRANSPARENT);
        paint.anti_alias = false;
        // BlendMode::Source clears pixels regardless of what is already there.
        paint.blend_mode = tiny_skia::BlendMode::Source;
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }

    // --- silhouette ---
    match kind {
        DeviceKind::Mouse => draw_glyph_mouse(pixmap, x0, y0, glyph, color),
        DeviceKind::Keyboard => draw_glyph_keyboard(pixmap, x0, y0, glyph, color),
        DeviceKind::Headset => draw_glyph_headset(pixmap, x0, y0, glyph, color),
        DeviceKind::Controller => draw_glyph_controller(pixmap, x0, y0, glyph, color),
        DeviceKind::Other => {}
    }
}

/// Mouse: tall body (taller than wide) with a 1px top-centre notch.
///
/// Layout (origin = glyph box top-left):
///   body   : x = ⌊g/4⌋ .. x+⌊g/2⌋, y = ⌊g/6⌋ .. g
///   notch  : 1px gap at top-centre of body (body_x + body_w/2, body_y)
fn draw_glyph_mouse(pixmap: &mut Pixmap, x0: u32, y0: u32, g: u32, color: Color) {
    let paint = solid_paint(color);
    let bx = x0 + g / 4;
    let by = y0 + g / 6;
    let bw = g / 2;
    let bh = g - g / 6;
    if bw == 0 || bh == 0 {
        return;
    }
    // Full body
    if let Some(rect) = Rect::from_xywh(bx as f32, by as f32, bw as f32, bh as f32) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
    // Top-centre notch: punch a 1px transparent cell at the top middle of the body.
    let notch_x = bx + bw / 2;
    if let Some(rect) = Rect::from_xywh(notch_x as f32, by as f32, 1.0, 1.0) {
        let mut tp = Paint::default();
        tp.set_color(Color::TRANSPARENT);
        tp.anti_alias = false;
        tp.blend_mode = tiny_skia::BlendMode::Source;
        pixmap.fill_rect(rect, &tp, Transform::identity(), None);
    }
}

/// Keyboard: wide flat rect (wider than tall) with 1px key-bump dots on the top edge.
///
/// Layout:
///   body : x = 0 .. g, y = ⌊g/3⌋ .. ⌊2g/3⌋  (flat band in vertical centre)
///   bumps: three 1px dots evenly spaced across the body, 1px above the body top
fn draw_glyph_keyboard(pixmap: &mut Pixmap, x0: u32, y0: u32, g: u32, color: Color) {
    let paint = solid_paint(color);
    let bx = x0;
    let by = y0 + g / 3;
    let bw = g;
    let bh = g / 3;
    if bw == 0 || bh == 0 {
        return;
    }
    // Flat body
    if let Some(rect) = Rect::from_xywh(bx as f32, by as f32, bw as f32, bh as f32) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
    // Key bumps: 3 dots 1px above body, evenly spaced
    if by > y0 {
        let dot_y = by - 1;
        let spacing = bw / 4; // positions at 1/4, 2/4, 3/4 of bw
        for i in 1..=3_u32 {
            let dot_x = bx + spacing * i;
            if let Some(rect) = Rect::from_xywh(dot_x as f32, dot_y as f32, 1.0, 1.0) {
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
        }
    }
}

/// Headset: 1px-thick top arc (two corner pixels) + two short vertical earcup stems.
///
/// Layout:
///   top bar : y = y0,          x = x0+1 .. x0+g-2  (headband row)
///   stems   : x = x0, x0+g-1, y = y0+1 .. y0+⌊g/2⌋  (left and right earcup)
fn draw_glyph_headset(pixmap: &mut Pixmap, x0: u32, y0: u32, g: u32, color: Color) {
    let paint = solid_paint(color);
    // Headband: top row from x+1 to x+g-2 (leave corner pixels transparent)
    let bar_x = x0 + 1;
    let bar_w = g.saturating_sub(2);
    if bar_w > 0
        && let Some(rect) = Rect::from_xywh(bar_x as f32, y0 as f32, bar_w as f32, 1.0)
    {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
    // Left earcup stem
    let stem_h = g / 2;
    if stem_h > 0 {
        if let Some(rect) = Rect::from_xywh(x0 as f32, (y0 + 1) as f32, 1.0, stem_h as f32) {
            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
        // Right earcup stem
        let rx = x0 + g - 1;
        if let Some(rect) = Rect::from_xywh(rx as f32, (y0 + 1) as f32, 1.0, stem_h as f32) {
            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }
}

/// Controller: wide central rect with a 1px bump protruding from each side.
///
/// Layout:
///   body  : x = 1 .. g-1, y = ⌊g/4⌋ .. ⌊3g/4⌋  (wide band)
///   bumps : 1px×2px block on left (x=0) and right (x=g) at vertical centre
fn draw_glyph_controller(pixmap: &mut Pixmap, x0: u32, y0: u32, g: u32, color: Color) {
    let paint = solid_paint(color);
    let bx = x0 + 1;
    let by = y0 + g / 4;
    let bw = g.saturating_sub(2);
    let bh = g / 2;
    if bw == 0 || bh == 0 {
        return;
    }
    // Wide body
    if let Some(rect) = Rect::from_xywh(bx as f32, by as f32, bw as f32, bh as f32) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
    // Side bumps at vertical centre
    let mid_y = y0 + g / 2;
    let bump_h = 2_u32.min(g / 4).max(1);
    // Left bump
    if let Some(rect) = Rect::from_xywh(x0 as f32, mid_y as f32, 1.0, bump_h as f32) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
    // Right bump
    let right_x = x0 + g;
    if right_x < pixmap.width()
        && let Some(rect) = Rect::from_xywh(right_x as f32, mid_y as f32, 1.0, bump_h as f32)
    {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
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
        use crate::domain::DeviceKind;
        use tiny_skia::{IntSize, Pixmap};
        let renderer = TinySkiaRenderer { sizes: vec![128] };

        // Helper: convert ARGB (network byte order) back to premultiplied RGBA and save.
        let save = |icon: &ksni::Icon, path: &str| {
            let mut rgba = Vec::with_capacity(icon.data.len());
            for px in icon.data.chunks_exact(4) {
                rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
            }
            let size = IntSize::from_wh(icon.width as u32, icon.height as u32).unwrap();
            let pm = Pixmap::from_vec(rgba, size).unwrap();
            pm.save_png(path).unwrap();
        };

        // Without glyph — existing baseline.
        let modes = [
            ("icon_only", DisplayMode::IconOnly),
            ("percent_only", DisplayMode::PercentOnly),
            ("percent_in_icon", DisplayMode::PercentInIcon),
        ];
        for (name, mode) in modes {
            let icons = renderer.render(
                PrimaryStatus::Ok { percent: 90 },
                None,
                &Theme::dark(),
                mode,
                false,
            );
            save(&icons[0], &format!("/tmp/rigbat_{name}.png"));
        }

        // With device-kind glyphs for visual review.
        let kinds = [
            ("mouse", Some(DeviceKind::Mouse)),
            ("keyboard", Some(DeviceKind::Keyboard)),
            ("headset", Some(DeviceKind::Headset)),
            ("controller", Some(DeviceKind::Controller)),
        ];
        for (kname, kind) in kinds {
            let icons = renderer.render(
                PrimaryStatus::Ok { percent: 75 },
                kind,
                &Theme::dark(),
                DisplayMode::IconOnly,
                false,
            );
            save(&icons[0], &format!("/tmp/rigbat_glyph_{kname}.png"));
        }

        // Fresh/stale pairs, both themes: the point of comparison is whether a
        // remembered reading still reads as a reading. `Low` appears here
        // deliberately — it must come out identical in both columns.
        let stale_cases = [
            ("ok", PrimaryStatus::Ok { percent: 75 }),
            ("charging", PrimaryStatus::Charging { percent: 75 }),
            ("low", PrimaryStatus::Low { percent: 12 }),
        ];
        for (theme_name, theme) in [("dark", Theme::dark()), ("light", Theme::light())] {
            for (case, status) in stale_cases {
                for (suffix, stale) in [("fresh", false), ("stale", true)] {
                    let icons = renderer.render(
                        status,
                        Some(DeviceKind::Mouse),
                        &theme,
                        DisplayMode::IconOnly,
                        stale,
                    );
                    save(
                        &icons[0],
                        &format!("/tmp/rigbat_{theme_name}_{case}_{suffix}.png"),
                    );
                }
            }
        }
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

    // --- mode: PercentInIcon --------------------------------------------

    #[test]
    fn percent_in_icon_renders_correct_count() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            None,
            &Theme::dark(),
            DisplayMode::PercentInIcon,
            false,
        );
        assert_eq!(icons.len(), default_sizes().len());
    }

    #[test]
    fn percent_in_icon_data_len() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 50 },
            None,
            &Theme::dark(),
            DisplayMode::PercentInIcon,
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
        for mode in [
            DisplayMode::IconOnly,
            DisplayMode::PercentOnly,
            DisplayMode::PercentInIcon,
        ] {
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
        let dark_icons =
            renderer.render(status, None, &Theme::dark(), DisplayMode::IconOnly, false);
        let light_icons =
            renderer.render(status, None, &Theme::light(), DisplayMode::IconOnly, false);

        assert!(!dark_icons.is_empty());
        assert!(!light_icons.is_empty());
        assert_ne!(&dark_icons[0].data, &light_icons[0].data);
    }

    /// Mean alpha (icon data is ARGB, alpha is byte 0 of each pixel) across
    /// all pixels of the first icon.
    fn mean_alpha(icon: &ksni::Icon) -> f64 {
        let alphas: Vec<u8> = icon.data.chunks_exact(4).map(|px| px[0]).collect();
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
            .chunks_exact(4)
            .zip(stale_icon.data.chunks_exact(4))
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

    /// WCAG 2.1 relative luminance of one sRGB channel (0-255).
    fn linearize_channel(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG 2.1 relative luminance of an sRGB colour.
    fn relative_luminance(rgb: [u8; 3]) -> f64 {
        let [r, g, b] = rgb;
        0.2126 * linearize_channel(r)
            + 0.7152 * linearize_channel(g)
            + 0.0722 * linearize_channel(b)
    }

    /// WCAG 2.1 contrast ratio between two sRGB colours.
    fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
        let (l1, l2) = (relative_luminance(a), relative_luminance(b));
        let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn light_theme_charging_color_clears_wcag_graphical_object_contrast() {
        let theme = Theme::light();
        let [r, g, b, _] = theme.charging;
        let panel = [0xf0, 0xf0, 0xf0]; // typical light panel background
        let ratio = contrast_ratio([r, g, b], panel);

        // WCAG 2.1 SC 1.4.11 (Non-text Contrast) requires 3:1 for graphical
        // objects; a battery icon's charging colour is not an exempt
        // "inactive control", so it must clear this floor.
        assert!(
            ratio >= 3.0,
            "light theme charging colour contrast is {ratio:.2}:1, below the 3:1 WCAG floor"
        );

        // The dimmed fill is the case that actually has to pass: in IconOnly
        // mode there are no digits, so the fill level is the whole reading.
        // Composite the stale fill over the panel the way the renderer does.
        let dimmed = [
            (f64::from(r) * f64::from(STALE_ALPHA)
                + f64::from(panel[0]) * (1.0 - f64::from(STALE_ALPHA)))
            .round() as u8,
            (f64::from(g) * f64::from(STALE_ALPHA)
                + f64::from(panel[1]) * (1.0 - f64::from(STALE_ALPHA)))
            .round() as u8,
            (f64::from(b) * f64::from(STALE_ALPHA)
                + f64::from(panel[2]) * (1.0 - f64::from(STALE_ALPHA)))
            .round() as u8,
        ];
        let dimmed_ratio = contrast_ratio(dimmed, panel);
        assert!(
            dimmed_ratio >= 3.0,
            "light theme charging fill dimmed by STALE_ALPHA is {dimmed_ratio:.2}:1, \
             below the 3:1 WCAG floor"
        );
    }

    // --- corner glyph tests -----------------------------------------------

    /// Helper: extract RGBA bytes from ksni::Icon (ARGB network order → RGBA).
    fn icon_to_rgba(icon: &ksni::Icon) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(icon.data.len());
        for px in icon.data.chunks_exact(4) {
            rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
        }
        rgba
    }

    /// Returns true if any pixel in the bottom-right `glyph×glyph` box is non-transparent.
    fn corner_has_opaque(icon: &ksni::Icon, size: u32) -> bool {
        let glyph = ((size as f32 / 3.0).round() as u32).max(6);
        let margin = ((size as f32 / 22.0).round() as u32).max(1);
        let x0 = size.saturating_sub(glyph + margin) as i32;
        let y0 = size.saturating_sub(glyph + margin) as i32;
        let glyph = glyph as i32;
        let w = icon.width;
        let rgba = icon_to_rgba(icon);
        for dy in 0..glyph {
            for dx in 0..glyph {
                let px = x0 + dx;
                let py = y0 + dy;
                if px < 0 || py < 0 || px >= w || py >= icon.height {
                    continue;
                }
                let idx = ((py * w + px) * 4) as usize;
                if rgba[idx + 3] != 0 {
                    return true;
                }
            }
        }
        false
    }

    /// Returns true if any pixel OUTSIDE the corner glyph box + knockout ring
    /// (i.e., in the battery body region) is non-transparent.
    fn battery_body_has_opaque(icon: &ksni::Icon, size: u32) -> bool {
        let glyph = ((size as f32 / 3.0).round() as u32).max(6);
        let margin = ((size as f32 / 22.0).round() as u32).max(1);
        let ko_x0 = (size.saturating_sub(glyph + margin).saturating_sub(1)) as i32;
        let ko_y0 = (size.saturating_sub(glyph + margin).saturating_sub(1)) as i32;
        let ko_size = (glyph + 2) as i32;
        let w = icon.width;
        let rgba = icon_to_rgba(icon);
        for py in 0..icon.height {
            for px in 0..w {
                let in_corner =
                    px >= ko_x0 && px < ko_x0 + ko_size && py >= ko_y0 && py < ko_y0 + ko_size;
                if !in_corner {
                    let idx = ((py * w + px) * 4) as usize;
                    if rgba[idx + 3] != 0 {
                        return true;
                    }
                }
            }
        }
        false
    }

    #[test]
    fn glyph_mouse_has_corner_pixels_at_22px() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 80 },
            Some(crate::domain::DeviceKind::Mouse),
            &Theme::dark(),
            DisplayMode::IconOnly,
            false,
        );
        assert!(!icons.is_empty());
        assert!(
            corner_has_opaque(&icons[0], 22),
            "mouse glyph missing at 22px"
        );
    }

    #[test]
    fn glyph_keyboard_has_corner_pixels_at_22px() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 80 },
            Some(crate::domain::DeviceKind::Keyboard),
            &Theme::dark(),
            DisplayMode::IconOnly,
            false,
        );
        assert!(!icons.is_empty());
        assert!(
            corner_has_opaque(&icons[0], 22),
            "keyboard glyph missing at 22px"
        );
    }

    #[test]
    fn glyph_headset_has_corner_pixels_at_22px() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 80 },
            Some(crate::domain::DeviceKind::Headset),
            &Theme::dark(),
            DisplayMode::IconOnly,
            false,
        );
        assert!(!icons.is_empty());
        assert!(
            corner_has_opaque(&icons[0], 22),
            "headset glyph missing at 22px"
        );
    }

    #[test]
    fn glyph_controller_has_corner_pixels_at_22px() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 80 },
            Some(crate::domain::DeviceKind::Controller),
            &Theme::dark(),
            DisplayMode::IconOnly,
            false,
        );
        assert!(!icons.is_empty());
        assert!(
            corner_has_opaque(&icons[0], 22),
            "controller glyph missing at 22px"
        );
    }

    /// `None` and `Other` both skip glyph drawing — their outputs must be byte-identical.
    #[test]
    fn glyph_none_and_other_render_identically() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let status = PrimaryStatus::Ok { percent: 50 };
        let icons_none =
            renderer.render(status, None, &Theme::dark(), DisplayMode::IconOnly, false);
        let icons_other = renderer.render(
            status,
            Some(crate::domain::DeviceKind::Other),
            &Theme::dark(),
            DisplayMode::IconOnly,
            false,
        );
        assert!(!icons_none.is_empty());
        assert_eq!(icons_none.len(), icons_other.len());
        assert_eq!(
            icons_none[0].data, icons_other[0].data,
            "None and Other must produce identical pixmaps (no glyph drawn for either)"
        );
    }

    /// A drawable kind (Mouse) must change some pixels relative to `None`.
    #[test]
    fn glyph_some_kind_differs_from_none_in_corner() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let status = PrimaryStatus::Ok { percent: 50 };
        let icons_none =
            renderer.render(status, None, &Theme::dark(), DisplayMode::IconOnly, false);
        let icons_mouse = renderer.render(
            status,
            Some(crate::domain::DeviceKind::Mouse),
            &Theme::dark(),
            DisplayMode::IconOnly,
            false,
        );
        assert!(!icons_none.is_empty());
        assert_ne!(
            icons_none[0].data, icons_mouse[0].data,
            "Mouse glyph must change at least one pixel compared to None"
        );
    }

    #[test]
    fn glyph_does_not_erase_battery_body_at_22px() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 80 },
            Some(crate::domain::DeviceKind::Mouse),
            &Theme::dark(),
            DisplayMode::IconOnly,
            false,
        );
        assert!(!icons.is_empty());
        assert!(
            battery_body_has_opaque(&icons[0], 22),
            "battery body must remain non-transparent outside the corner box"
        );
    }

    // --- offline + corner glyph (render_offline_battery calls maybe_draw_kind_glyph) ---

    #[test]
    fn offline_glyph_has_corner_pixels_for_each_drawable_kind() {
        let kinds = [
            crate::domain::DeviceKind::Mouse,
            crate::domain::DeviceKind::Keyboard,
            crate::domain::DeviceKind::Headset,
            crate::domain::DeviceKind::Controller,
        ];
        for kind in kinds {
            let renderer = TinySkiaRenderer { sizes: vec![22] };
            let icons = renderer.render(
                PrimaryStatus::Offline,
                Some(kind),
                &Theme::dark(),
                DisplayMode::IconOnly,
                false,
            );
            assert!(!icons.is_empty());
            assert!(
                corner_has_opaque(&icons[0], 22),
                "offline {kind:?} glyph missing at 22px"
            );
        }
    }

    // --- glyph + digit coexistence in PercentOnly / PercentInIcon ---
    //
    // `battery_body_has_opaque` asserts opaque pixels outside the corner box —
    // for these two modes that region is where the centred digit block lives,
    // so it doubles as the "digit area still has content" check.

    #[test]
    fn percent_only_with_glyph_has_both_corner_and_digit_content() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 42 },
            Some(crate::domain::DeviceKind::Mouse),
            &Theme::dark(),
            DisplayMode::PercentOnly,
            false,
        );
        assert!(!icons.is_empty());
        assert!(
            corner_has_opaque(&icons[0], 22),
            "PercentOnly: glyph missing from corner"
        );
        assert!(
            battery_body_has_opaque(&icons[0], 22),
            "PercentOnly: digit block missing outside the corner box"
        );
    }

    #[test]
    fn percent_in_icon_with_glyph_has_both_corner_and_digit_content() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let icons = renderer.render(
            PrimaryStatus::Ok { percent: 42 },
            Some(crate::domain::DeviceKind::Mouse),
            &Theme::dark(),
            DisplayMode::PercentInIcon,
            false,
        );
        assert!(!icons.is_empty());
        assert!(
            corner_has_opaque(&icons[0], 22),
            "PercentInIcon: glyph missing from corner"
        );
        assert!(
            battery_body_has_opaque(&icons[0], 22),
            "PercentInIcon: digit block missing outside the corner box"
        );
    }

    // --- Fix 4 regression: draw_glyph_headset/draw_glyph_controller no longer
    // guard `g < 4`; the sole caller (maybe_draw_kind_glyph) always computes
    // glyph = max(6, ...), but confirm a canvas smaller than the glyph box
    // still cannot panic. ---

    // --- structural invariants (T16): assert what the icon draws, not just its size ---
    //
    // Premultiplied-alpha handling: `Pixmap` stores premultiplied RGBA and
    // `icon_to_rgba` does not un-premultiply. For the fully opaque theme
    // colours (alpha 255) that is a no-op, but `Theme::offline` (alpha 180)
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
            .chunks_exact(4)
            .filter(|px| px[3] != 0)
            .map(|px| (px[0], px[1], px[2], px[3]))
            .collect()
    }

    /// Number of opaque pixels in the icon.
    fn opaque_pixel_count(icon: &ksni::Icon) -> usize {
        icon_to_rgba(icon)
            .chunks_exact(4)
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
    fn percent_in_icon_digits_are_value_sensitive() {
        let renderer = TinySkiaRenderer { sizes: vec![22] };
        let theme = Theme::dark();
        let render = |percent: u8| {
            renderer
                .render(
                    PrimaryStatus::Ok { percent },
                    None,
                    &theme,
                    DisplayMode::PercentInIcon,
                    false,
                )
                .remove(0)
        };

        let a1 = render(42);
        let a2 = render(42);
        let b = render(87);
        assert_eq!(
            a1.data, a2.data,
            "PercentInIcon: rendering the same value twice must be identical"
        );
        assert_ne!(
            a1.data, b.data,
            "PercentInIcon: rendering a different value must change the buffer"
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
    fn headset_and_controller_glyphs_do_not_panic_on_1px_and_2px_canvas() {
        for size in [1, 2] {
            for kind in [
                crate::domain::DeviceKind::Headset,
                crate::domain::DeviceKind::Controller,
            ] {
                let renderer = TinySkiaRenderer { sizes: vec![size] };
                let icons = renderer.render(
                    PrimaryStatus::Ok { percent: 50 },
                    Some(kind),
                    &Theme::dark(),
                    DisplayMode::IconOnly,
                    false,
                );
                assert_eq!(icons.len(), 1, "size {size}: {kind:?} did not render");
            }
        }
    }
}
