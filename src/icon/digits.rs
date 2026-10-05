//! Tray digits from the embedded League Gothic outlines (`assets/fonts/`).

use super::Region;
use anyhow::{Context, Result, anyhow};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, MetadataProvider};
use std::sync::LazyLock;
use tiny_skia::{
    Color, FillRule, Paint, Path, PathBuilder, Pixmap, PixmapPaint, PremultipliedColorU8, Rect,
    Transform,
};

const REGULAR: &[u8] = include_bytes!("../../assets/fonts/LeagueGothic-Digits.ttf");
const CONDENSED: &[u8] = include_bytes!("../../assets/fonts/LeagueGothicCondensed-Digits.ttf");

/// Space between two digits' ink, as a fraction of the ink height.
const GAP: f32 = 0.12;

/// One digit's outline in font units, y down, with its ink box.
struct Glyph {
    path: Path,
    ink: Rect,
}

/// `0`–`9` of one face, indexed by digit.
struct Face(Vec<Glyph>);

impl Face {
    fn load(data: &[u8]) -> Result<Self> {
        let font = FontRef::new(data).context("parsing the font")?;
        let charmap = font.charmap();
        let outlines = font.outline_glyphs();
        let glyphs = ('0'..='9')
            .map(|ch| {
                let id = charmap
                    .map(ch)
                    .ok_or_else(|| anyhow!("no glyph for {ch:?}"))?;
                let outline = outlines
                    .get(id)
                    .ok_or_else(|| anyhow!("no outline for {ch:?}"))?;
                let mut pen = FlipPen(PathBuilder::new());
                outline
                    .draw(
                        DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
                        &mut pen,
                    )
                    .with_context(|| format!("drawing {ch:?}"))?;
                let path = pen.0.finish().ok_or_else(|| anyhow!("{ch:?} is empty"))?;
                let ink = path
                    .compute_tight_bounds()
                    .ok_or_else(|| anyhow!("{ch:?} has no ink box"))?;
                Ok(Glyph { path, ink })
            })
            .collect::<Result<_>>()?;
        Ok(Self(glyphs))
    }
}

struct Fonts {
    regular: Face,
    condensed: Face,
}

impl Fonts {
    fn load() -> Result<Self> {
        Ok(Self {
            regular: Face::load(REGULAR).context("LeagueGothic-Digits.ttf")?,
            condensed: Face::load(CONDENSED).context("LeagueGothicCondensed-Digits.ttf")?,
        })
    }
}

/// Loaded once; a broken font warns once and leaves the digits undrawn.
static FONTS: LazyLock<Option<Fonts>> = LazyLock::new(|| {
    Fonts::load()
        .inspect_err(|e| tracing::warn!("tray digits disabled: {e:#}"))
        .ok()
});

/// Font units are y-up, the pixmap is y-down.
struct FlipPen(PathBuilder);

impl OutlinePen for FlipPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, -y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, -y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to(cx0, -cy0, x, -y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.cubic_to(cx0, -cy0, cx1, -cy1, x, -y);
    }

    fn close(&mut self) {
        self.0.close();
    }
}

/// The glyphs of `value` (0–100), left to right: `100` in the condensed face
/// so it keeps the height of two digits.
fn glyphs(fonts: &Fonts, value: u8) -> Vec<&Glyph> {
    let (face, digits) = if value >= 100 {
        (&fonts.condensed, vec![1, 0, 0])
    } else if value >= 10 {
        (&fonts.regular, vec![value / 10, value % 10])
    } else {
        (&fonts.regular, vec![value])
    };
    digits
        .into_iter()
        .filter_map(|d| face.0.get(usize::from(d)))
        .collect()
}

/// Draws `value` centred in `region`, as large as fits in both width and
/// height; digits sit apart by their ink, not their advances.
pub(super) fn draw_percent_in_region(
    pixmap: &mut Pixmap,
    region: Region,
    value: u8,
    color: Color,
    dotted: bool,
) {
    let Some(fonts) = FONTS.as_ref() else {
        return;
    };
    if !dotted {
        draw_glyphs(pixmap, fonts, region, value, color);
        return;
    }
    let Some(mut scratch) = Pixmap::new(pixmap.width(), pixmap.height()) else {
        return;
    };
    draw_glyphs(&mut scratch, fonts, region, value, color);
    cut_grid(&mut scratch);
    pixmap.draw_pixmap(
        0,
        0,
        scratch.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

fn draw_glyphs(pixmap: &mut Pixmap, fonts: &Fonts, region: Region, value: u8, color: Color) {
    let glyphs = glyphs(fonts, value);
    let (Some(top), Some(bottom)) = (
        glyphs.iter().map(|g| g.ink.top()).reduce(f32::min),
        glyphs.iter().map(|g| g.ink.bottom()).reduce(f32::max),
    ) else {
        return;
    };
    let height = bottom - top;
    let gap = GAP * height;
    let width = glyphs.iter().map(|g| g.ink.width()).sum::<f32>()
        + gap * glyphs.len().saturating_sub(1) as f32;
    let scale = (region.w / width).min(region.h / height);
    if !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let x0 = region.x + (region.w - width * scale) / 2.0;
    let y0 = region.y + (region.h - height * scale) / 2.0 - top * scale;
    let mut paint = Paint::default();
    paint.set_color(color);
    // Paths, unlike thin rects, rasterise safely with anti-aliasing.
    paint.anti_alias = true;
    let mut cursor = 0.0;
    for glyph in glyphs {
        let dx = x0 + (cursor - glyph.ink.left()) * scale;
        let transform = Transform::from_row(scale, 0.0, 0.0, scale, dx, y0);
        pixmap.fill_path(&glyph.path, &paint, FillRule::Winding, transform, None);
        cursor += glyph.ink.width() + gap;
    }
}

/// The spacing of the clear grid that marks a not-live reading; a 3 px grid
/// leaves the smallest icons illegible.
fn grid_period(size: u32) -> u32 {
    if size <= 24 {
        return 4;
    }
    (size as f32 / 11.0).round().max(3.0) as u32
}

/// Clears 1 px lines along both axes, every `grid_period` pixels.
fn cut_grid(pixmap: &mut Pixmap) {
    let width = pixmap.width();
    let period = grid_period(pixmap.height());
    for (i, pixel) in pixmap.pixels_mut().iter_mut().enumerate() {
        let (x, y) = (i as u32 % width, i as u32 / width);
        if x % period == period - 1 || y % period == period - 1 {
            *pixel = PremultipliedColorU8::TRANSPARENT;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_fonts_parse_and_draw_every_digit() {
        for (name, data) in [("regular", REGULAR), ("condensed", CONDENSED)] {
            let face = Face::load(data).expect(name);
            assert_eq!(face.0.len(), 10, "{name}");
            for (d, glyph) in face.0.iter().enumerate() {
                assert!(
                    glyph.ink.width() > 0.0 && glyph.ink.height() > 0.0,
                    "{name} {d}: empty outline"
                );
            }
        }
        assert!(FONTS.is_some());
    }

    #[test]
    fn the_grid_is_wider_on_small_icons() {
        assert_eq!(grid_period(22), 4);
        assert_eq!(grid_period(32), 3);
        assert_eq!(grid_period(44), 4);
        assert_eq!(grid_period(64), 6);
    }
}
