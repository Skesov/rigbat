mod battery;
mod bitmap;
mod device_bar;
mod digits;
mod theme;

use crate::domain::{DeviceKind, DisplayMode, PrimaryStatus};
use battery::{
    battery_geometry, draw_battery_fill, draw_battery_nub, draw_battery_outline, draw_cross_line,
};
use bitmap::{Area, Bitmap, cell_areas, draw_bitmap, knock_out, silhouette, status_mark};
use device_bar::{DeviceBar, silhouette_area};
use digits::draw_percent_in_region;
pub use theme::Theme;
use theme::dimmed;
use tiny_skia::{Color, Paint, Pixmap};

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

const PUBLISHED_SIZES: [u32; 5] = [22, 24, 32, 44, 64];

const DESIGN_SIZE: f32 = 22.0;

pub struct TinySkiaRenderer {
    pub sizes: Vec<u32>,
}

impl Default for TinySkiaRenderer {
    fn default() -> Self {
        Self {
            sizes: PUBLISHED_SIZES.to_vec(),
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
            .filter_map(|&size| render_online(size, layout, &look))
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
/// pixmap does not gain width — it only shrinks. The canvas is square.
/// Returns `None` only if `Pixmap::new` fails (does not happen for sizes ≤ 64).
fn render_online(size: u32, layout: Layout, look: &Look) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(size, size)?;
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
fn render_offline(size: u32, rgba: [u8; 4], layout: Layout) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(size, size)?;
    let c = color(rgba);
    if let Layout::Device(bitmap) = layout {
        draw_bitmap(&mut pixmap, bitmap, silhouette_area(size), c);
        let bar = DeviceBar::new(size);
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

/// A solid-color paint with anti-aliasing disabled. AA is unnecessary for the
/// axis-aligned rectangles we draw and tiny-skia's AA path can panic on very
/// thin rects at small icon sizes, which would crash the tray.
fn solid_paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = false;
    paint
}

/// The gap between the canvas edge and a mark in a corner.
fn corner_margin(size: u32) -> i32 {
    ((size as f32 / DESIGN_SIZE).round() as i32).max(1)
}

/// The clear halo around a status mark, and its gap to the digits.
fn mark_ring(size: u32) -> i32 {
    (size as i32 / 32).max(1)
}

/// Where the status mark goes: for `Battery` centred inside the body; for
/// `Device` in the top-right corner, over the silhouette; for `Digits`
/// bottom-left, under the digits.
fn mark_area(size: u32, layout: Layout, mark: Bitmap) -> Area {
    let (w, h) = (size as f32, size as f32);
    // Whole pixels per cell keep the mark crisp; the body has room for more.
    let per_design_px = match layout {
        Layout::Battery => 1.4,
        Layout::Device(_) | Layout::Digits => 1.0,
    };
    let cell = (h * per_design_px / DESIGN_SIZE).floor().max(1.0) as i32;
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
        Layout::Device(_) => (size as i32 - width, 0),
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
    let (w, h) = (size as f32, size as f32);
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

/// Converts tiny-skia premultiplied RGBA to ARGB32 network byte order (A,R,G,B).
fn pixmap_to_icon(pixmap: Pixmap) -> ksni::Icon {
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let rgba_data = pixmap.data();
    let mut argb_data = Vec::with_capacity(rgba_data.len());
    for chunk in rgba_data.as_chunks::<4>().0.iter() {
        argb_data.extend([chunk[3], chunk[0], chunk[1], chunk[2]]);
    }
    ksni::Icon {
        width: w,
        height: h,
        data: argb_data,
    }
}

#[cfg(test)]
mod tests;
