use tiny_skia::{Color, Pixmap, Rect, Transform};

use super::solid_paint;
use crate::domain::{DeviceKind, PrimaryStatus};

/// A one-colour pixel drawing: bit `cols - 1 - c` of `rows[r]` lights cell `(c, r)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Bitmap {
    pub(super) cols: u32,
    pub(super) rows: &'static [u16],
}

impl Bitmap {
    pub(super) fn row_count(self) -> u32 {
        self.rows.len() as u32
    }

    pub(super) fn is_set(self, col: u32, row: u32) -> bool {
        col < self.cols
            && self
                .rows
                .get(row as usize)
                .is_some_and(|bits| bits & (1 << (self.cols - 1 - col)) != 0)
    }

    /// Lit cells as `(col, row)`.
    pub(super) fn cells(self) -> impl Iterator<Item = (u32, u32)> {
        (0..self.row_count())
            .flat_map(move |row| (0..self.cols).map(move |col| (col, row)))
            .filter(move |&(col, row)| self.is_set(col, row))
    }
}

/// Side of a silhouette in cells: one pixel each at 22 px.
pub(super) const SILHOUETTE_CELLS: u32 = 14;

/// The `DeviceAndBattery` silhouette; `Other` has none.
pub(super) fn silhouette(kind: DeviceKind) -> Option<Bitmap> {
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

pub(super) const BOLT: Bitmap = Bitmap {
    cols: 5,
    rows: &[
        0b00011, 0b00110, 0b01100, 0b11111, 0b00110, 0b01100, 0b11000,
    ],
};

/// The shape that tells charging apart from an ordinary reading without
/// colour — the icon's `CHARGING_SIGN`; low is marked by colour alone.
pub(super) fn status_mark(status: PrimaryStatus) -> Option<Bitmap> {
    match status {
        PrimaryStatus::Charging { .. } => Some(BOLT),
        PrimaryStatus::Ok { .. } | PrimaryStatus::Low { .. } | PrimaryStatus::Offline => None,
    }
}

/// A whole-pixel box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Area {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) w: i32,
    pub(super) h: i32,
}

impl Area {
    pub(super) fn right(self) -> i32 {
        self.x + self.w
    }

    pub(super) fn bottom(self) -> i32 {
        self.y + self.h
    }

    pub(super) fn grown(self, by: i32) -> Self {
        Self {
            x: self.x - by,
            y: self.y - by,
            w: self.w + 2 * by,
            h: self.h + 2 * by,
        }
    }

    pub(super) fn rect(self) -> Option<Rect> {
        Rect::from_xywh(self.x as f32, self.y as f32, self.w as f32, self.h as f32)
    }
}

/// Each lit cell of `bitmap` stretched over `area`; cell edges round to whole
/// pixels, so cells differ by at most one pixel and never leave a seam.
pub(super) fn cell_areas(bitmap: Bitmap, area: Area) -> impl Iterator<Item = Area> {
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

pub(super) fn fill_area(pixmap: &mut Pixmap, area: Area, color: Color) {
    if let Some(rect) = area.rect() {
        pixmap.fill_rect(rect, &solid_paint(color), Transform::identity(), None);
    }
}

pub(super) fn draw_bitmap(pixmap: &mut Pixmap, bitmap: Bitmap, area: Area, color: Color) {
    for cell in cell_areas(bitmap, area) {
        fill_area(pixmap, cell, color);
    }
}

/// Clears `area` to transparent, whatever was drawn there.
pub(super) fn knock_out(pixmap: &mut Pixmap, area: Area) {
    if let Some(rect) = area.rect() {
        let mut paint = solid_paint(Color::TRANSPARENT);
        paint.blend_mode = tiny_skia::BlendMode::Source;
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}
