mod digits;

use crate::appearance::ColorScheme;
use crate::domain::{DeviceKind, DisplayMode, Palette, PrimaryStatus};
use crate::palette::{self, DIM, GRAPHIC_CONTRAST, Rgb};
use digits::draw_percent_in_region;
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
        let layout = Layout::new(mode, kind);
        // `resolve_for` never pairs offline with `stale`.
        let (rgba, percent) = match status {
            PrimaryStatus::Offline => {
                return self
                    .sizes
                    .iter()
                    .filter_map(|&size| render_offline(size, theme.offline, layout))
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
            mark: status_mark(status),
            retained,
        };
        self.sizes
            .iter()
            .filter_map(|&size| render_mode(size, layout, &look))
            .collect()
    }
}

/// One reading, whatever the size. A low reading is never `retained`: it
/// must not get quieter.
struct Look {
    /// Outline, nub, digits, status mark and silhouette — never dimmed.
    color: Color,
    /// The fill bar, dimmed by `DIM` when retained.
    fill: Color,
    percent: u8,
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

/// What an icon draws: `DeviceAndBattery` without a silhouette is `Battery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Battery,
    Device(Bitmap),
    Digits,
}

impl Layout {
    fn new(mode: DisplayMode, kind: Option<DeviceKind>) -> Self {
        match (mode, kind.and_then(silhouette)) {
            (DisplayMode::DeviceAndBattery, Some(bitmap)) => Self::Device(bitmap),
            (DisplayMode::IconOnly | DisplayMode::DeviceAndBattery, _) => Self::Battery,
            (DisplayMode::PercentOnly, _) => Self::Digits,
        }
    }
}

/// SNI hosts (e.g. COSMIC) fit the icon into a roughly square slot, so a wider
/// pixmap does not gain width — it only shrinks. Keep the canvas square and fill it.
const WIDE_ASPECT: f32 = 1.0;

/// Canvas width for a given icon height.
fn wide_width(height: u32) -> u32 {
    ((height as f32) * WIDE_ASPECT).round() as u32
}

/// Returns `None` only if `Pixmap::new` fails (does not happen for sizes ≤ 64).
fn render_mode(size: u32, layout: Layout, look: &Look) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(wide_width(size), size)?;
    let g = battery_geometry(pixmap.width() as f32, pixmap.height() as f32);
    match layout {
        Layout::Battery => {
            draw_battery_fill(&mut pixmap, &g, f32::from(look.percent) / 100.0, look.fill);
            draw_battery_outline(&mut pixmap, &g, look.color, look.retained);
            draw_battery_nub(&mut pixmap, &g, look.color);
        }
        Layout::Device(bitmap) => {
            draw_bitmap(&mut pixmap, bitmap, silhouette_area(size), look.color);
            let bar = DeviceBar::new(size);
            bar.draw_frame(&mut pixmap, look.color, look.retained);
            bar.draw_fill(&mut pixmap, look.percent, look.fill);
        }
        Layout::Digits => {}
    }
    if let Some(mark) = look.mark {
        draw_mark(&mut pixmap, mark, mark_area(size, layout, mark), look.color);
    }
    if layout == Layout::Digits {
        let region = digit_region(size, look.mark);
        draw_percent_in_region(&mut pixmap, region, look.percent, look.color, look.retained);
    }
    Some(pixmap_to_icon(pixmap))
}

/// In the offline colour, which is already dimmed: the silhouette over a
/// slashed empty bar, or else the crossed battery.
fn render_offline(n: u32, rgba: [u8; 4], layout: Layout) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(wide_width(n), n)?;
    let c = color(rgba);
    if let Layout::Device(bitmap) = layout {
        draw_bitmap(&mut pixmap, bitmap, silhouette_area(n), c);
        let bar = DeviceBar::new(n);
        bar.draw_frame(&mut pixmap, c, false);
        bar.draw_slash(&mut pixmap, c);
    } else {
        let g = battery_geometry(pixmap.width() as f32, pixmap.height() as f32);
        draw_battery_outline(&mut pixmap, &g, c, false);
        draw_battery_nub(&mut pixmap, &g, c);
        draw_cross_line(&mut pixmap, &g, c);
    }
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
// Bitmaps: device silhouettes and status marks
// ---------------------------------------------------------------------------

/// A one-colour pixel drawing: bit `cols - 1 - c` of `rows[r]` lights cell `(c, r)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bitmap {
    cols: u32,
    rows: &'static [u16],
}

impl Bitmap {
    fn row_count(self) -> u32 {
        self.rows.len() as u32
    }

    fn is_set(self, col: u32, row: u32) -> bool {
        col < self.cols
            && self
                .rows
                .get(row as usize)
                .is_some_and(|bits| bits & (1 << (self.cols - 1 - col)) != 0)
    }

    /// Lit cells as `(col, row)`.
    fn cells(self) -> impl Iterator<Item = (u32, u32)> {
        (0..self.row_count())
            .flat_map(move |row| (0..self.cols).map(move |col| (col, row)))
            .filter(move |&(col, row)| self.is_set(col, row))
    }
}

/// Side of a silhouette in cells: one pixel each at 22 px.
const SILHOUETTE_CELLS: u32 = 14;

/// The `DeviceAndBattery` silhouette; `Other` has none.
fn silhouette(kind: DeviceKind) -> Option<Bitmap> {
    let rows: &'static [u16] = match kind {
        DeviceKind::Mouse => &[
            0b00001110111000,
            0b00011110111100,
            0b00111100011110,
            0b00111101011110,
            0b00111101011110,
            0b00111100011110,
            0b00111110111110,
            0b00111111111110,
            0b00111111111110,
            0b00111111111110,
            0b00111111111110,
            0b00111111111110,
            0b00011111111100,
            0b00001111111000,
        ],
        DeviceKind::Keyboard => &[
            0b00000000000000,
            0b00000000000000,
            0b00000000000000,
            0b11111111111111,
            0b11111111111111,
            0b11011011011011,
            0b11111111111111,
            0b11111111111111,
            0b11110000001111,
            0b11111111111111,
            0b11111111111111,
            0b00000000000000,
            0b00000000000000,
            0b00000000000000,
        ],
        DeviceKind::Headset => &[
            0b00001111110000,
            0b00111111111100,
            0b01110000001110,
            0b01100000000110,
            0b11000000000011,
            0b11000000000011,
            0b11000000000011,
            0b11000000000011,
            0b11110000001111,
            0b11110000001111,
            0b11110000001111,
            0b11110000001111,
            0b11110000001111,
            0b01110000001110,
        ],
        DeviceKind::Controller => &[
            0b00000000000000,
            0b00000000000000,
            0b01111111111110,
            0b11111111111111,
            0b11101111111011,
            0b11000111111111,
            0b11101111101111,
            0b11111111111111,
            0b11111111111111,
            0b11111000011111,
            0b11110000001111,
            0b11110000001111,
            0b11100000000111,
            0b01100000000110,
        ],
        DeviceKind::Other => return None,
    };
    Some(Bitmap {
        cols: SILHOUETTE_CELLS,
        rows,
    })
}

const BOLT: Bitmap = Bitmap {
    cols: 5,
    rows: &[
        0b00011, 0b00110, 0b01100, 0b11111, 0b00110, 0b01100, 0b11000,
    ],
};

/// The shape that tells charging apart from an ordinary reading without
/// colour — the icon's `CHARGING_SIGN`; low is marked by colour alone.
fn status_mark(status: PrimaryStatus) -> Option<Bitmap> {
    match status {
        PrimaryStatus::Charging { .. } => Some(BOLT),
        PrimaryStatus::Ok { .. } | PrimaryStatus::Low { .. } | PrimaryStatus::Offline => None,
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
    fn right(self) -> i32 {
        self.x + self.w
    }

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

fn fill_area(pixmap: &mut Pixmap, area: Area, color: Color) {
    if let Some(rect) = area.rect() {
        pixmap.fill_rect(rect, &solid_paint(color), Transform::identity(), None);
    }
}

fn draw_bitmap(pixmap: &mut Pixmap, bitmap: Bitmap, area: Area, color: Color) {
    for cell in cell_areas(bitmap, area) {
        fill_area(pixmap, cell, color);
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

/// `v` pixels of the 22 px design at `size`, rounded to a whole pixel.
fn at_scale(size: u32, v: i32) -> i32 {
    (v as f32 * size as f32 / 22.0).round() as i32
}

/// The silhouette's square: horizontally centred, on the top edge.
fn silhouette_area(size: u32) -> Area {
    let side = at_scale(size, SILHOUETTE_CELLS as i32);
    Area {
        x: (wide_width(size) as i32 - side) / 2,
        y: 0,
        w: side,
        h: side,
    }
}

/// The thin battery under the silhouette: at 22 px an outline over rows
/// 16–21 and columns 0–20, the nub in column 21, and the fill one pixel
/// clear of the outline.
struct DeviceBar {
    outline: Area,
    nub: Area,
    line: i32,
}

impl DeviceBar {
    fn new(size: u32) -> Self {
        let (w, h) = (wide_width(size) as i32, size as i32);
        let top = at_scale(size, 16);
        let right = w - at_scale(size, 1);
        let (nub_top, nub_bottom) = (at_scale(size, 18), at_scale(size, 20));
        Self {
            outline: Area {
                x: 0,
                y: top,
                w: right,
                h: h - top,
            },
            nub: Area {
                x: right,
                y: nub_top,
                w: w - right,
                h: nub_bottom - nub_top,
            },
            line: at_scale(size, 1).max(1),
        }
    }

    /// The outline and nub; `dashed` breaks each side into dashes two lines
    /// long, one line apart.
    fn draw_frame(&self, pixmap: &mut Pixmap, color: Color, dashed: bool) {
        let (o, line) = (self.outline, self.line);
        let sides = [
            Area { h: line, ..o },
            Area {
                y: o.bottom() - line,
                h: line,
                ..o
            },
            Area { w: line, ..o },
            Area {
                x: o.right() - line,
                w: line,
                ..o
            },
        ];
        for side in sides {
            if dashed {
                for dash in dashes(side, line) {
                    fill_area(pixmap, dash, color);
                }
            } else {
                fill_area(pixmap, side, color);
            }
        }
        fill_area(pixmap, self.nub, color);
    }

    fn draw_fill(&self, pixmap: &mut Pixmap, percent: u8, color: Color) {
        let inset = 2 * self.line;
        let inner = Area {
            x: self.outline.x + inset,
            y: self.outline.y + inset,
            w: self.outline.w - 2 * inset,
            h: self.outline.h - 2 * inset,
        };
        let w = (inner.w as f32 * f32::from(percent.min(100)) / 100.0).round() as i32;
        if w > 0 && inner.h > 0 {
            fill_area(pixmap, Area { w, ..inner }, color);
        }
    }

    /// One diagonal across the outline, top-left to bottom-right.
    fn draw_slash(&self, pixmap: &mut Pixmap, color: Color) {
        let (o, half) = (self.outline, self.line as f32 / 2.0);
        let stroke = Stroke {
            width: self.line as f32,
            ..Stroke::default()
        };
        let mut pb = PathBuilder::new();
        pb.move_to(o.x as f32 + half, o.y as f32 + half);
        pb.line_to(o.right() as f32 - half, o.bottom() as f32 - half);
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(
                &path,
                &solid_paint(color),
                &stroke,
                Transform::identity(),
                None,
            );
        }
    }
}

/// `side` cut into dashes `2 * line` long with `line` gaps, along its long axis.
fn dashes(side: Area, line: i32) -> impl Iterator<Item = Area> {
    let along_x = side.w >= side.h;
    let length = if along_x { side.w } else { side.h };
    let period = (3 * line).max(1) as usize;
    (0..length).step_by(period).map(move |at| {
        let dash = (2 * line).min(length - at);
        if along_x {
            Area {
                x: side.x + at,
                w: dash,
                ..side
            }
        } else {
            Area {
                y: side.y + at,
                h: dash,
                ..side
            }
        }
    })
}

/// The gap between the canvas edge and a mark in a corner.
fn corner_margin(size: u32) -> i32 {
    ((size as f32 / 22.0).round() as i32).max(1)
}

/// The clear halo around a status mark, and its gap to the digits.
fn mark_ring(size: u32) -> i32 {
    (size as i32 / 32).max(1)
}

/// Where the status mark goes: for `Battery` centred inside the body; for
/// `Device` in the top-right corner, over the silhouette; for `Digits`
/// bottom-left, under the digits.
fn mark_area(size: u32, layout: Layout, mark: Bitmap) -> Area {
    let (w, h) = (wide_width(size) as f32, size as f32);
    // Whole pixels per cell keep the mark crisp; the body has room for more.
    let per_22 = match layout {
        Layout::Battery => 1.4,
        Layout::Device(_) | Layout::Digits => 1.0,
    };
    let cell = (h * per_22 / 22.0).floor().max(1.0) as i32;
    let (width, height) = (mark.cols as i32 * cell, mark.row_count() as i32 * cell);
    let (x, y) = match layout {
        Layout::Battery => {
            let g = battery_geometry(w, h);
            let centre = g.bx + g.bw / 2.0;
            (
                (centre - width as f32 / 2.0).round() as i32,
                (g.by + (g.bh - height as f32) / 2.0).round() as i32,
            )
        }
        Layout::Device(_) => (wide_width(size) as i32 - width, 0),
        Layout::Digits => {
            let margin = corner_margin(size);
            (margin, size as i32 - margin - height)
        }
    };
    Area {
        x,
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

/// Where the digits may go, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Region {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Most of the canvas, above a status mark.
fn digit_region(size: u32, mark: Option<Bitmap>) -> Region {
    let (w, h) = (wide_width(size) as f32, size as f32);
    let region = Region {
        x: w * 0.05,
        y: h * 0.05,
        w: w * 0.90,
        h: h * 0.90,
    };
    let Some(mark) = mark else {
        return region;
    };
    let area = mark_area(size, Layout::Digits, mark);
    let bottom = ((area.y - mark_ring(size)) as f32).min(region.y + region.h);
    Region {
        h: (bottom - region.y).max(0.0),
        ..region
    }
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
mod tests;
