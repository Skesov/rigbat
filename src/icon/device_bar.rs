use tiny_skia::{Color, PathBuilder, Pixmap, Stroke, Transform};

use super::bitmap::{Area, SILHOUETTE_CELLS, fill_area};
use super::{DESIGN_SIZE, solid_paint};

/// `v` pixels of the 22 px design at `size`, rounded to a whole pixel.
pub(super) fn at_scale(size: u32, v: i32) -> i32 {
    (v as f32 * size as f32 / DESIGN_SIZE).round() as i32
}

/// The silhouette's square: horizontally centred, on the top edge.
pub(super) fn silhouette_area(size: u32) -> Area {
    let side = at_scale(size, SILHOUETTE_CELLS as i32);
    Area {
        x: (size as i32 - side) / 2,
        y: 0,
        w: side,
        h: side,
    }
}

/// The thin battery under the silhouette: at 22 px an outline over rows
/// 16–21 and columns 0–20, the nub in column 21, and the fill one pixel
/// clear of the outline.
pub(super) struct DeviceBar {
    pub(super) outline: Area,
    pub(super) nub: Area,
    pub(super) line: i32,
}

impl DeviceBar {
    pub(super) fn new(size: u32) -> Self {
        let (w, h) = (size as i32, size as i32);
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
    pub(super) fn draw_frame(&self, pixmap: &mut Pixmap, color: Color, dashed: bool) {
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

    pub(super) fn draw_fill(&self, pixmap: &mut Pixmap, percent: u8, color: Color) {
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
    pub(super) fn draw_slash(&self, pixmap: &mut Pixmap, color: Color) {
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
pub(super) fn dashes(side: Area, line: i32) -> impl Iterator<Item = Area> {
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
