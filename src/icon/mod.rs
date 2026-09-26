use crate::appearance::ColorScheme;
use crate::domain::{DeviceKind, DisplayMode, Palette, PrimaryStatus};
use crate::palette::{self, DIM, GRAPHIC_CONTRAST, Rgb};
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect, Stroke, StrokeDash,
    Transform,
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

/// Everything a rendered icon depends on: equal keys render equal pixmaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconKey {
    pub status: PrimaryStatus,
    pub kind: Option<DeviceKind>,
    pub theme: Theme,
    pub mode: DisplayMode,
    pub stale: bool,
}

/// Renders each distinct `IconKey` once and keeps the result while the key is in use.
pub struct IconCache {
    renderer: Box<dyn IconRenderer>,
    icons: Vec<(IconKey, Vec<ksni::Icon>)>,
}

impl IconCache {
    pub fn new(renderer: Box<dyn IconRenderer>) -> Self {
        Self {
            renderer,
            icons: Vec::new(),
        }
    }

    pub fn icons(&mut self, key: &IconKey) -> Vec<ksni::Icon> {
        if let Some((_, icons)) = self.icons.iter().find(|(k, _)| k == key) {
            return icons.clone();
        }
        let icons = self
            .renderer
            .render(key.status, key.kind, &key.theme, key.mode, key.stale);
        self.icons.push((*key, icons.clone()));
        icons
    }

    /// Forgets every key `keep` rejects, so the cache holds only what is shown.
    pub fn retain(&mut self, keep: impl Fn(&IconKey) -> bool) {
        self.icons.retain(|(key, _)| keep(key));
    }
}

/// Theme colors in RGBA format (straight alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub normal: [u8; 4],
    pub low: [u8; 4],
    pub charging: [u8; 4],
    pub offline: [u8; 4],
}

/// Stand-ins for the host's panel, which rigbat cannot see, when measuring contrast.
const NOMINAL_PANEL_DARK: Rgb = [0x1e, 0x1e, 0x1e];
const NOMINAL_PANEL_LIGHT: Rgb = [0xf0, 0xf0, 0xf0];

impl Theme {
    /// The palette's colours, each a graphical object of at least 3:1 on the
    /// nominal panel; a colour that may be dimmed passes dimmed.
    pub fn new(palette: Palette, scheme: ColorScheme) -> Self {
        let s = palette::swatches(palette, scheme);
        let panel = match scheme {
            ColorScheme::Dark => NOMINAL_PANEL_DARK,
            ColorScheme::Light => NOMINAL_PANEL_LIGHT,
        };
        let graphic = |color, opacity| {
            let [r, g, b] = palette::readable(color, panel, GRAPHIC_CONTRAST, opacity);
            [r, g, b, 255]
        };
        let normal = graphic(s.fg, DIM);
        Self {
            normal,
            low: graphic(s.low, 1.0),
            charging: graphic(s.charging, DIM),
            offline: dimmed(normal),
        }
    }
}

fn dimmed([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    [
        r,
        g,
        b,
        (f32::from(a) * DIM).round().clamp(0.0, 255.0) as u8,
    ]
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
    fn render(
        &self,
        status: PrimaryStatus,
        kind: Option<DeviceKind>,
        theme: &Theme,
        mode: DisplayMode,
        stale: bool,
    ) -> Vec<ksni::Icon> {
        // Offline draws the crossed battery in every mode; `resolve_for` never
        // pairs it with `stale`.
        let (rgba, percent) = match status {
            PrimaryStatus::Offline => {
                return self
                    .sizes
                    .iter()
                    .filter_map(|&size| render_offline_battery(size, theme.offline, kind))
                    .collect();
            }
            PrimaryStatus::Charging { percent } => (theme.charging, percent),
            PrimaryStatus::Low { percent } => (theme.low, percent),
            PrimaryStatus::Ok { percent } => (theme.normal, percent),
        };
        let retained = stale && !matches!(status, PrimaryStatus::Low { .. });
        let look = Look {
            color: color(rgba),
            fill: color(if retained { dimmed(rgba) } else { rgba }),
            percent,
            kind,
            mark: status_mark(status),
            retained,
        };
        self.sizes
            .iter()
            .filter_map(|&size| render_mode(size, mode, &look))
            .collect()
    }
}

/// One reading, whatever the size. A low reading is never `retained`: it
/// must not get quieter.
struct Look {
    /// Outline, nub, digits, status mark and kind glyph — never dimmed.
    color: Color,
    /// The fill bar, dimmed by `DIM` when retained.
    fill: Color,
    percent: u8,
    kind: Option<DeviceKind>,
    mark: Option<Bitmap>,
    /// Not live: a dashed outline, or dotted digits where there is no outline.
    retained: bool,
}

fn color([r, g, b, a]: [u8; 4]) -> Color {
    Color::from_rgba8(r, g, b, a)
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

/// Returns `None` only if `Pixmap::new` fails (does not happen for sizes ≤ 64).
fn render_mode(size: u32, mode: DisplayMode, look: &Look) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(wide_width(size), size)?;
    let g = battery_geometry(pixmap.width() as f32, pixmap.height() as f32);
    let glyph = has_kind_glyph(look.kind);
    match mode {
        DisplayMode::IconOnly => {
            draw_battery_fill(&mut pixmap, &g, f32::from(look.percent) / 100.0, look.fill);
            draw_battery_outline(&mut pixmap, &g, look.color, look.retained);
            draw_battery_nub(&mut pixmap, &g, look.color);
            if let Some(mark) = look.mark {
                draw_mark(
                    &mut pixmap,
                    mark,
                    mark_area(size, mode, mark, glyph),
                    look.color,
                );
            }
            maybe_draw_kind_glyph(&mut pixmap, look.kind, size, look.color);
        }
        // The glyph goes on before the digits in both percent modes. Its first
        // step punches a transparent ring around itself, and the canvas is
        // square (`WIDE_ASPECT`), so at 22 px the centred digit block reaches
        // into the same bottom-right corner — drawing the glyph last erased
        // part of the last digit (21 px at 22 px, 163 px at 64 px, measured).
        // Digits carry the reading and the glyph only identifies the device,
        // so where they collide the digits win.
        DisplayMode::PercentOnly | DisplayMode::PercentInIcon => {
            let outline = mode == DisplayMode::PercentInIcon;
            if outline {
                draw_battery_outline(&mut pixmap, &g, look.color, look.retained);
                draw_battery_nub(&mut pixmap, &g, look.color);
            }
            maybe_draw_kind_glyph(&mut pixmap, look.kind, size, look.color);
            if let Some(mark) = look.mark {
                draw_mark(
                    &mut pixmap,
                    mark,
                    mark_area(size, mode, mark, glyph),
                    look.color,
                );
            }
            let dotted = look.retained && !outline;
            let region = digit_region(size, mode, look.mark);
            draw_percent_in_region(&mut pixmap, region, look.percent, look.color, dotted);
        }
    }
    Some(pixmap_to_icon(pixmap))
}

/// The crossed battery, in the offline colour, which is already dimmed.
fn render_offline_battery(n: u32, rgba: [u8; 4], kind: Option<DeviceKind>) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(wide_width(n), n)?;
    let c = color(rgba);
    let g = battery_geometry(pixmap.width() as f32, pixmap.height() as f32);
    draw_battery_outline(&mut pixmap, &g, c, false);
    draw_battery_nub(&mut pixmap, &g, c);
    draw_cross_line(&mut pixmap, &g, c);
    maybe_draw_kind_glyph(&mut pixmap, kind, n, c);
    Some(pixmap_to_icon(pixmap))
}

// ---------------------------------------------------------------------------
// Battery drawing helpers
// ---------------------------------------------------------------------------

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

fn draw_battery_outline(pixmap: &mut Pixmap, g: &BatteryGeom, color: Color, dashed: bool) {
    let paint = solid_paint(color);
    let stroke = Stroke {
        width: g.sw,
        line_cap: if dashed {
            LineCap::Butt
        } else {
            LineCap::Square
        },
        line_join: LineJoin::Miter,
        dash: dashed
            .then(|| StrokeDash::new(vec![g.sw * 2.0, g.sw * 1.25], 0.0))
            .flatten(),
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
// Bitmaps: kind glyphs and status marks
// ---------------------------------------------------------------------------

/// A one-colour pixel drawing: bit `cols - 1 - c` of `rows[r]` lights cell `(c, r)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bitmap {
    pub cols: u32,
    pub rows: &'static [u8],
}

impl Bitmap {
    pub fn row_count(self) -> u32 {
        self.rows.len() as u32
    }

    pub fn is_set(self, col: u32, row: u32) -> bool {
        col < self.cols
            && self
                .rows
                .get(row as usize)
                .is_some_and(|bits| bits & (1 << (self.cols - 1 - col)) != 0)
    }

    /// Lit cells as `(col, row)`.
    pub fn cells(self) -> impl Iterator<Item = (u32, u32)> {
        (0..self.row_count())
            .flat_map(move |row| (0..self.cols).map(move |col| (col, row)))
            .filter(move |&(col, row)| self.is_set(col, row))
    }
}

/// The device-kind silhouette, drawn by the tray icon's corner and by both
/// windows. `Other` is a battery; the tray icon, itself a battery, skips it.
pub fn kind_glyph(kind: DeviceKind) -> Bitmap {
    let rows: &'static [u8] = match kind {
        DeviceKind::Mouse => &[
            0b0110110, 0b0110110, 0b0111110, 0b0111110, 0b0111110, 0b0111110, 0b0011100,
        ],
        DeviceKind::Keyboard => &[
            0b0000000, 0b1111111, 0b1010101, 0b1111111, 0b1100011, 0b1111111, 0b0000000,
        ],
        DeviceKind::Headset => &[
            0b0011100, 0b0100010, 0b1000001, 0b1000001, 0b1100011, 0b1100011, 0b1100011,
        ],
        DeviceKind::Controller => &[
            0b0000000, 0b0111110, 0b1111111, 0b1011101, 0b1111111, 0b1100011, 0b1000001,
        ],
        DeviceKind::Other => &[
            0b0000000, 0b1111110, 0b1110011, 0b1110011, 0b1110011, 0b1111110, 0b0000000,
        ],
    };
    Bitmap { cols: 7, rows }
}

const BOLT: Bitmap = Bitmap {
    cols: 5,
    rows: &[
        0b00011, 0b00110, 0b01100, 0b11111, 0b00110, 0b01100, 0b11000,
    ],
};

const WARNING: Bitmap = Bitmap {
    cols: 7,
    rows: &[
        0b0001000, 0b0011100, 0b0010100, 0b0110110, 0b0111110, 0b1110111, 0b1111111,
    ],
};

/// The shape that tells charging and low apart from an ordinary reading
/// without colour — the icon's `CHARGING_SIGN` and `LOW_SIGN`.
fn status_mark(status: PrimaryStatus) -> Option<Bitmap> {
    match status {
        PrimaryStatus::Charging { .. } => Some(BOLT),
        PrimaryStatus::Low { .. } => Some(WARNING),
        PrimaryStatus::Ok { .. } | PrimaryStatus::Offline => None,
    }
}

/// A whole-pixel box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Area {
    fn bottom(self) -> i32 {
        self.y + self.h
    }

    fn grown(self, by: i32) -> Self {
        Self {
            x: self.x - by,
            y: self.y - by,
            w: self.w + 2 * by,
            h: self.h + 2 * by,
        }
    }

    fn rect(self) -> Option<Rect> {
        Rect::from_xywh(self.x as f32, self.y as f32, self.w as f32, self.h as f32)
    }
}

/// Each lit cell of `bitmap` stretched over `area`; cell edges round to whole
/// pixels, so cells differ by at most one pixel and never leave a seam.
fn cell_areas(bitmap: Bitmap, area: Area) -> impl Iterator<Item = Area> {
    let edge = |origin: i32, span: i32, n: u32, i: u32| {
        origin + (span as f32 * i as f32 / n as f32).round() as i32
    };
    bitmap.cells().map(move |(col, row)| {
        let (x0, x1) = (
            edge(area.x, area.w, bitmap.cols, col),
            edge(area.x, area.w, bitmap.cols, col + 1),
        );
        let (y0, y1) = (
            edge(area.y, area.h, bitmap.row_count(), row),
            edge(area.y, area.h, bitmap.row_count(), row + 1),
        );
        Area {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    })
}

fn draw_bitmap(pixmap: &mut Pixmap, bitmap: Bitmap, area: Area, color: Color) {
    let paint = solid_paint(color);
    for rect in cell_areas(bitmap, area).filter_map(Area::rect) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

/// Clears `area` to transparent, whatever was drawn there.
fn knock_out(pixmap: &mut Pixmap, area: Area) {
    if let Some(rect) = area.rect() {
        let mut paint = solid_paint(Color::TRANSPARENT);
        paint.blend_mode = tiny_skia::BlendMode::Source;
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

fn has_kind_glyph(kind: Option<DeviceKind>) -> bool {
    kind.is_some_and(|k| k != DeviceKind::Other)
}

fn kind_glyph_margin(size: u32) -> i32 {
    ((size as f32 / 22.0).round() as i32).max(1)
}

/// The bottom-right box of the kind glyph.
fn kind_glyph_area(size: u32) -> Area {
    let glyph = ((size as f32 / 3.0).round() as i32).max(6);
    let at = (size as i32 - glyph - kind_glyph_margin(size)).max(0);
    Area {
        x: at,
        y: at,
        w: glyph,
        h: glyph,
    }
}

/// The glyph clears its box and a 1 px ring first, so it reads against a
/// full-charge fill without a background square. In the percent modes the
/// digits reach this corner too; `render_mode` draws them last.
fn maybe_draw_kind_glyph(pixmap: &mut Pixmap, kind: Option<DeviceKind>, size: u32, color: Color) {
    let Some(kind) = kind.filter(|&k| has_kind_glyph(Some(k))) else {
        return;
    };
    let area = kind_glyph_area(size);
    knock_out(pixmap, area.grown(1));
    draw_bitmap(pixmap, kind_glyph(kind), area, color);
}

/// The clear halo around a status mark, and its gap to the digits.
fn mark_ring(size: u32) -> i32 {
    (size as i32 / 32).max(1)
}

/// Where the status mark goes: in `IconOnly` inside the body, left of the kind
/// glyph; in `PercentOnly` bottom-left, opposite the kind glyph, under the
/// digits; in `PercentInIcon` above the digits, over the battery's top edge.
fn mark_area(size: u32, mode: DisplayMode, mark: Bitmap, glyph: bool) -> Area {
    let (w, h) = (wide_width(size) as f32, size as f32);
    let g = battery_geometry(w, h);
    // Whole pixels per cell keep the mark crisp; the body has room for more.
    let per_22 = match mode {
        DisplayMode::IconOnly => 1.4,
        DisplayMode::PercentOnly | DisplayMode::PercentInIcon => 1.0,
    };
    let cell = (h * per_22 / 22.0).floor().max(1.0) as i32;
    let (width, height) = (mark.cols as i32 * cell, mark.row_count() as i32 * cell);
    let (centre_x, y) = match mode {
        DisplayMode::IconOnly => {
            let left = g.bx + g.sw / 2.0;
            let right = if glyph {
                (kind_glyph_area(size).x - 1) as f32
            } else {
                g.bx + g.bw - g.sw / 2.0
            };
            (
                (left + right) / 2.0,
                (g.by + (g.bh - height as f32) / 2.0).round() as i32,
            )
        }
        DisplayMode::PercentOnly => {
            let margin = kind_glyph_margin(size);
            ((margin + width / 2) as f32, size as i32 - margin - height)
        }
        DisplayMode::PercentInIcon => (g.bx + g.bw / 2.0, (h * 0.05) as i32),
    };
    Area {
        x: (centre_x - width as f32 / 2.0).round() as i32,
        y,
        w: width,
        h: height,
    }
}

/// Knocks a halo out of whatever is under the mark, then draws it.
fn draw_mark(pixmap: &mut Pixmap, mark: Bitmap, area: Area, color: Color) {
    let ring = mark_ring(pixmap.height());
    for cell in cell_areas(mark, area) {
        knock_out(pixmap, cell.grown(ring));
    }
    draw_bitmap(pixmap, mark, area, color);
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
/// 5 cells tall, separated by `DIGIT_GAP` of a cell. `dotted` leaves a pixel
/// between cells.
fn draw_number(
    pixmap: &mut Pixmap,
    value: u8,
    x: f32,
    y: f32,
    cell: f32,
    color: Color,
    dotted: bool,
) {
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
    let dot = if dotted { cell - 1.0 } else { cell };
    let paint = solid_paint(color);

    for (i, &d) in digits.iter().enumerate() {
        let ox = x + i as f32 * digit_stride;
        for (row, &mask) in DIGITS[d].iter().enumerate() {
            let oy = y + row as f32 * cell;
            for col in 0..3_u8 {
                // MSB of the 3-bit mask is the leftmost column.
                if mask & (0b100 >> col) != 0 {
                    let px = ox + col as f32 * cell;
                    if let Some(rect) = Rect::from_xywh(px, oy, dot, dot) {
                        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                    }
                }
            }
        }
    }
}

/// Where the digits may go, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Region {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Most of the canvas in `PercentOnly`, the battery's inside in
/// `PercentInIcon`; in both, below a status mark.
fn digit_region(size: u32, mode: DisplayMode, mark: Option<Bitmap>) -> Region {
    let (w, h) = (wide_width(size) as f32, size as f32);
    let region = match mode {
        DisplayMode::PercentInIcon => {
            let g = battery_geometry(w, h);
            Region {
                x: g.bx + g.sw,
                y: g.by + g.sw,
                w: g.bw - g.sw * 2.0,
                h: g.bh - g.sw * 2.0,
            }
        }
        DisplayMode::IconOnly | DisplayMode::PercentOnly => Region {
            x: w * 0.05,
            y: h * 0.05,
            w: w * 0.90,
            h: h * 0.90,
        },
    };
    let Some(mark) = mark else {
        return region;
    };
    let area = mark_area(size, mode, mark, false);
    let ring = mark_ring(size);
    if mode == DisplayMode::PercentOnly {
        let bottom = ((area.y - ring) as f32).min(region.y + region.h);
        return Region {
            h: (bottom - region.y).max(0.0),
            ..region
        };
    }
    let top = ((area.bottom() + ring) as f32).max(region.y);
    Region {
        y: top,
        h: (region.y + region.h - top).max(0.0),
        ..region
    }
}

/// Draws `value` centred in `region`, as large as fits in both width and height.
fn draw_percent_in_region(
    pixmap: &mut Pixmap,
    region: Region,
    value: u8,
    color: Color,
    dotted: bool,
) {
    let n = digit_count(value);

    // Largest cell that fits 5 rows in height and the digit block in width.
    let cell_from_h = region.h / 5.0;
    let cols = n as f32 * 3.0 + (n.saturating_sub(1)) as f32 * DIGIT_GAP;
    let cell_from_w = region.w / cols;
    let cell = cell_from_h.min(cell_from_w).max(0.0);

    let total_w = digits_width(n, cell);
    let total_h = 5.0 * cell;

    let x = region.x + ((region.w - total_w) / 2.0).max(0.0);
    let y = region.y + ((region.h - total_h) / 2.0).max(0.0);
    draw_number(pixmap, value, x, y, cell, color, dotted);
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
    for chunk in rgba_data.as_chunks::<4>().0.iter() {
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

    impl Theme {
        fn dark() -> Self {
            Self::new(Palette::default(), ColorScheme::Dark)
        }

        fn light() -> Self {
            Self::new(Palette::default(), ColorScheme::Light)
        }
    }

    /// The device-kind glyph punches a transparent ring around itself so it
    /// reads against a battery fill. The canvas is square, the digit block is
    /// centred and reaches the same bottom-right corner, so drawing the glyph
    /// after the digits erased part of the last digit — 21 px at 22 px, 163 px
    /// at 64 px, measured. `render_mode` draws the glyph first in the percent
    /// modes, and the status mark above the digits; this asserts no digit
    /// pixel is lost at any published size.
    #[test]
    fn neither_the_kind_glyph_nor_the_status_mark_erases_a_digit() {
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

        for mode in [DisplayMode::PercentOnly, DisplayMode::PercentInIcon] {
            for (status, percent, rgba) in statuses {
                let icons = renderer.render(status, Some(DeviceKind::Mouse), &theme, mode, false);
                for icon in icons {
                    let (w, h) = (icon.width as u32, icon.height as u32);
                    let mut digits = Pixmap::new(w, h).expect("digit mask pixmap");
                    let region = digit_region(h, mode, status_mark(status));
                    draw_percent_in_region(&mut digits, region, percent, color(rgba), false);

                    let lost = digits
                        .pixels()
                        .iter()
                        .enumerate()
                        .filter(|(i, pixel)| pixel.alpha() > 0 && icon.data[i * 4] == 0)
                        .count();
                    assert_eq!(
                        lost, 0,
                        "{mode:?} {status:?} at {w}x{h}: {lost} digit pixels erased"
                    );
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
        let mut sheet =
            Pixmap::new(cell_w * states.len() as u32, cell_h * rows as u32).expect("sheet");
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

    /// Writes contact sheets to the temp directory for visual review.
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
        draw_number(&mut pixmap, 5, 2.0, 2.0, 3.0, color, false);

        // At least one pixel must be non-transparent after drawing.
        let has_opaque = pixmap.data().as_chunks::<4>().0.iter().any(|px| px[3] != 0);
        assert!(has_opaque, "draw_number produced no visible pixels");
    }

    #[test]
    fn draw_number_100_fits_three_digits() {
        let size = 64_u32;
        let mut pixmap = Pixmap::new(size, size).unwrap();
        let color = Color::from_rgba8(255, 255, 255, 255);
        // Cell of 4 px → three digits use 3*(3*4 + 4) - 4 = 44 px wide, fits in 64.
        draw_number(&mut pixmap, 100, 0.0, 0.0, 4.0, color, false);
        let has_opaque = pixmap.data().as_chunks::<4>().0.iter().any(|px| px[3] != 0);
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

    // --- corner glyph tests -----------------------------------------------

    /// Helper: extract RGBA bytes from ksni::Icon (ARGB network order → RGBA).
    fn icon_to_rgba(icon: &ksni::Icon) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(icon.data.len());
        for px in icon.data.as_chunks::<4>().0.iter() {
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
    fn charging_and_low_differ_from_ok_in_shape_alone() {
        for mode in DisplayMode::ALL {
            for percent in [8, 15, 88, 100] {
                let ok = masks(PrimaryStatus::Ok { percent }, mode, false);
                let charging = masks(PrimaryStatus::Charging { percent }, mode, false);
                let low = masks(PrimaryStatus::Low { percent }, mode, false);
                for (i, size) in PUBLISHED_SIZES.iter().enumerate() {
                    let at = format!("{mode:?} {percent}% at {size} px");
                    assert_ne!(ok[i], charging[i], "{at}: charging looks like ok");
                    assert_ne!(ok[i], low[i], "{at}: low looks like ok");
                    assert_ne!(charging[i], low[i], "{at}: charging looks like low");
                }
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

    fn overlaps(a: Area, b: Area) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    /// The mark and its halo stay clear of the kind glyph and its ring, and
    /// nothing drawn after the mark covers any of its pixels.
    #[test]
    fn the_status_mark_is_whole_and_clear_of_the_kind_glyph() {
        for mode in DisplayMode::ALL {
            for status in [
                PrimaryStatus::Charging { percent: 100 },
                PrimaryStatus::Charging { percent: 40 },
                PrimaryStatus::Low { percent: 8 },
            ] {
                let mark = status_mark(status).expect("a mark");
                for size in PUBLISHED_SIZES {
                    let at = format!("{mode:?} {status:?} at {size} px");
                    let area = mark_area(size, mode, mark, true);
                    let halo = area.grown(mark_ring(size));
                    assert!(
                        !overlaps(halo, kind_glyph_area(size).grown(1)),
                        "{at}: {halo:?} meets the glyph"
                    );
                    assert!(
                        halo.x >= 0 && halo.y >= 0 && halo.bottom() <= size as i32,
                        "{at}: {halo:?} leaves the canvas"
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

    /// The windows draw `kind_glyph` too; at 22 px each cell is one pixel.
    #[test]
    fn the_corner_glyph_is_the_shared_kind_bitmap() {
        for kind in [
            DeviceKind::Mouse,
            DeviceKind::Keyboard,
            DeviceKind::Headset,
            DeviceKind::Controller,
        ] {
            let icon = TinySkiaRenderer { sizes: vec![22] }
                .render(
                    PrimaryStatus::Ok { percent: 100 },
                    Some(kind),
                    &Theme::dark(),
                    DisplayMode::IconOnly,
                    false,
                )
                .remove(0);
            let area = kind_glyph_area(22);
            let bitmap = kind_glyph(kind);
            assert_eq!((area.w, area.h), (7, 7));
            for row in 0..7 {
                for col in 0..7 {
                    let lit = alpha_at(&icon, area.x + col as i32, area.y + row as i32) > 0;
                    assert_eq!(lit, bitmap.is_set(col, row), "{kind:?} cell ({col}, {row})");
                }
            }
        }
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
            mode: DisplayMode::PercentInIcon,
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
}
