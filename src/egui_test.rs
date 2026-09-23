//! Headless egui rendering for window tests: run frames without a display and
//! read back what was painted, where, and whether its clip rectangle cut it.

use eframe::egui;

/// The narrowest a string may be drawn and still count as readable.
pub const MIN_READABLE_WIDTH: f32 = 24.0;

/// One string as it landed on screen.
#[derive(Debug)]
pub struct Painted {
    pub text: String,
    pub rect: egui::Rect,
    pub lines: usize,
}

/// Every string `contents` painted at least `MIN_READABLE_WIDTH` wide after
/// its clip rectangle. A test that only collected galley strings passed
/// against R45's broken build: the text was there, the clip hid it.
pub fn painted_text_at(size: [f32; 2], contents: impl FnMut(&mut egui::Ui)) -> Vec<String> {
    text_at(size, contents, |_drawn, visible| {
        visible.width() >= MIN_READABLE_WIDTH && visible.height() > 0.0
    })
    .into_iter()
    .map(|painted| painted.text)
    .collect()
}

/// Strings drawn whole, not cut by their clip rectangle.
pub fn fully_painted_text_at(size: [f32; 2], contents: impl FnMut(&mut egui::Ui)) -> Vec<Painted> {
    text_at(size, contents, |drawn, visible| {
        visible.width() + 0.5 >= drawn.width() && visible.height() + 0.5 >= drawn.height()
    })
}

/// Two frames, because `egui_extras`' table sizes itself from the previous
/// frame. The root clip rectangle is the screen, so text laid out past the
/// window edge counts as cut.
fn text_at(
    size: [f32; 2],
    mut contents: impl FnMut(&mut egui::Ui),
    keep: impl Fn(egui::Rect, egui::Rect) -> bool,
) -> Vec<Painted> {
    let ctx = egui::Context::default();
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(size[0], size[1]),
        )),
        ..Default::default()
    };
    let mut painted = Vec::new();
    for _ in 0..2 {
        let output = ctx.run_ui(input(), |ui| contents(ui));
        painted.clear();
        for clipped in output.shapes {
            collect_text(&clipped.shape, clipped.clip_rect, &keep, &mut painted);
        }
    }
    painted
}

fn collect_text(
    shape: &egui::Shape,
    clip: egui::Rect,
    keep: &impl Fn(egui::Rect, egui::Rect) -> bool,
    out: &mut Vec<Painted>,
) {
    match shape {
        egui::Shape::Text(text) => {
            let drawn = egui::Rect::from_min_size(text.pos, text.galley.size());
            if keep(drawn, drawn.intersect(clip)) {
                out.push(Painted {
                    text: text.galley.text().to_owned(),
                    rect: drawn,
                    lines: text.galley.rows.len(),
                });
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, clip, keep, out);
            }
        }
        _ => {}
    }
}

/// Fails when a non-empty string wraps onto a second line or overlaps another.
pub fn assert_single_lines_without_overlap(painted: &[Painted]) {
    let labels: Vec<_> = painted.iter().filter(|p| !p.text.is_empty()).collect();
    for p in &labels {
        assert_eq!(p.lines, 1, "{:?} wraps onto a second line", p.text);
    }
    for (i, a) in labels.iter().enumerate() {
        for b in &labels[i + 1..] {
            let overlap = a.rect.intersect(b.rect);
            assert!(
                overlap.width() <= 0.5 || overlap.height() <= 0.5,
                "{:?} overlaps {:?}",
                a.text,
                b.text
            );
        }
    }
}
