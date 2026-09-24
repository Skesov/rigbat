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
    let mut painted = Vec::new();
    for _ in 0..2 {
        let output = run_frame(&ctx, size, Vec::new(), &mut contents);
        painted.clear();
        for clipped in output.shapes {
            collect_text(&clipped.shape, clipped.clip_rect, &keep, &mut painted);
        }
    }
    painted
}

/// One frame of `contents` on a `size` screen, with `events` as its input.
pub fn run_frame(
    ctx: &egui::Context,
    size: [f32; 2],
    events: Vec<egui::Event>,
    mut contents: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(size[0], size[1]),
        )),
        events,
        ..Default::default()
    };
    ctx.run_ui(input, |ui| contents(ui))
}

/// A primary-button click at `pos`: move, press, release, one frame each.
pub fn click_at(
    ctx: &egui::Context,
    size: [f32; 2],
    pos: egui::Pos2,
    mut contents: impl FnMut(&mut egui::Ui),
) {
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    };
    for event in [egui::Event::PointerMoved(pos), button(true), button(false)] {
        run_frame(ctx, size, vec![event], &mut contents);
    }
}

fn collect_text(
    shape: &egui::Shape,
    clip: egui::Rect,
    keep: &impl Fn(egui::Rect, egui::Rect) -> bool,
    out: &mut Vec<Painted>,
) {
    match shape {
        egui::Shape::Text(text) => {
            // A right-aligned galley's rect starts left of `pos`.
            let drawn = text.galley.rect.translate(text.pos.to_vec2());
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
    for p in painted.iter().filter(|p| !p.text.is_empty()) {
        assert_eq!(p.lines, 1, "{:?} wraps onto a second line", p.text);
    }
    assert_no_overlap(painted);
}

/// Fails when two non-empty strings overlap.
pub fn assert_no_overlap(painted: &[Painted]) {
    let labels: Vec<_> = painted.iter().filter(|p| !p.text.is_empty()).collect();
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
