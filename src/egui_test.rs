//! Headless egui rendering for window tests: run frames without a display and
//! read back what was painted, where, and whether its clip rectangle cut it.

use eframe::egui;

use crate::domain::DeviceKind;

/// The narrowest a string may be drawn and still count as readable.
const MIN_READABLE_WIDTH: f32 = 24.0;

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
    text_at(size, contents, whole)
}

/// Every string one frame's `output` painted whole.
pub fn painted(output: &egui::FullOutput) -> Vec<Painted> {
    let mut painted = Vec::new();
    for clipped in &output.shapes {
        collect_text(&clipped.shape, clipped.clip_rect, &whole, &mut painted);
    }
    painted
}

fn whole(drawn: egui::Rect, visible: egui::Rect) -> bool {
    visible.width() + 0.5 >= drawn.width() && visible.height() + 0.5 >= drawn.height()
}

/// Two frames, because some widgets size themselves from the previous frame. The root clip rectangle is the screen, so text laid out past the
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
    contents: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    frame(ctx, size, None, events, contents)
}

/// `run_frame` with the input clock at `time`, seconds; left unset, egui
/// advances it a sixtieth of a second per frame.
pub fn run_frame_at(
    ctx: &egui::Context,
    size: [f32; 2],
    time: f64,
    contents: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    frame(ctx, size, Some(time), Vec::new(), contents)
}

fn frame(
    ctx: &egui::Context,
    size: [f32; 2],
    time: Option<f64>,
    events: Vec<egui::Event>,
    mut contents: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let input = egui::RawInput {
        time,
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(size[0], size[1]),
        )),
        events,
        ..Default::default()
    };
    discard_textures(ctx.run_ui(input, |ui| contents(ui)))
}

/// Whether `font` draws `c` from one of its own faces, not as the replacement
/// "◻". egui 0.36's `has_glyph` alone answers no for every character of the
/// face that holds "◻", one of the bundled emoji fonts, "⚡" among them.
pub fn has_glyph(
    fonts: &mut egui::epaint::text::FontsView<'_>,
    font: &egui::FontId,
    c: char,
) -> bool {
    let mut family = fonts.fonts.font(&font.family);
    family.has_glyph(c) || family.characters().contains_key(&c)
}

pub fn has_glyphs(
    fonts: &mut egui::epaint::text::FontsView<'_>,
    font: &egui::FontId,
    s: &str,
) -> bool {
    s.chars().all(|c| has_glyph(fonts, font, c))
}

/// One pass with no screen or input, for tests that need fonts or style loaded.
pub fn empty_pass(ctx: &egui::Context) {
    discard_textures(ctx.run_ui(egui::RawInput::default(), |_| {}));
}

/// No renderer uploads the textures here; epaint panics on dropping a delta
/// nobody applied.
fn discard_textures(mut output: egui::FullOutput) -> egui::FullOutput {
    output.textures_delta.clear();
    output
}

/// A primary-button click at `pos`: move, press, release, one frame each.
/// Returns the release frame's output, where a click's commands land.
pub fn click_at(
    ctx: &egui::Context,
    size: [f32; 2],
    pos: egui::Pos2,
    mut contents: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    };
    run_frame(
        ctx,
        size,
        vec![egui::Event::PointerMoved(pos)],
        &mut contents,
    );
    run_frame(ctx, size, vec![button(true)], &mut contents);
    run_frame(ctx, size, vec![button(false)], &mut contents)
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

/// The text scales every window is tested at: the default and WCAG 1.4.4's 200 %.
pub const TEXT_SCALES: [f32; 2] = [1.0, 2.0];

/// Fails when an `expected` string is not painted whole, or two strings
/// overlap. A string may wrap: translations are never shortened to fit.
pub fn assert_whole(painted: &[Painted], expected: &[String], context: &str) {
    for text in expected {
        assert!(
            painted.iter().any(|p| &p.text == text),
            "{context}: {text:?} is cut off or missing: {painted:?}"
        );
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

pub const KINDS: [DeviceKind; 5] = [
    DeviceKind::Mouse,
    DeviceKind::Keyboard,
    DeviceKind::Headset,
    DeviceKind::Controller,
    DeviceKind::Other,
];

/// One node of a frame's AccessKit tree; the context must have AccessKit on.
#[derive(Debug, Clone)]
pub struct Node {
    pub role: egui::accesskit::Role,
    pub name: Option<String>,
    pub rect: egui::Rect,
    pub live: egui::accesskit::Live,
}

/// Every node `output` sent to AccessKit.
pub fn nodes(output: &egui::FullOutput) -> Vec<Node> {
    let Some(update) = &output.platform_output.accesskit_update else {
        return Vec::new();
    };
    update
        .nodes
        .iter()
        .map(|(_, node)| Node {
            role: node.role(),
            name: node.label().or_else(|| node.value()).map(str::to_owned),
            rect: node.bounds().map_or(egui::Rect::NOTHING, |b| {
                egui::Rect::from_min_max(
                    egui::pos2(b.x0 as f32, b.y0 as f32),
                    egui::pos2(b.x1 as f32, b.y1 as f32),
                )
            }),
            live: node.live().unwrap_or(egui::accesskit::Live::Off),
        })
        .collect()
}

/// The polite live regions `output` sent, by name.
pub fn live_regions(output: &egui::FullOutput) -> Vec<String> {
    nodes(output)
        .into_iter()
        .filter(|node| node.live == egui::accesskit::Live::Polite)
        .filter_map(|node| node.name)
        .collect()
}

/// The nodes a pointer operates.
pub fn targets(output: &egui::FullOutput) -> Vec<Node> {
    use egui::accesskit::Role;
    nodes(output)
        .into_iter()
        .filter(|node| {
            matches!(
                node.role,
                Role::Button
                    | Role::CheckBox
                    | Role::RadioButton
                    | Role::ComboBox
                    | Role::Slider
                    | Role::SpinButton
                    | Role::TextInput
                    | Role::Link
            )
        })
        .collect()
}

/// Each target is at least `min` × `min` (WCAG 2.2 SC 2.5.8). A link inside a
/// line of text is the one exception the criterion allows: a circle of
/// diameter `min` on its centre must touch no other target.
pub fn assert_targets_at_least(targets: &[Node], min: f32) {
    assert!(!targets.is_empty(), "no targets were laid out");
    for (i, target) in targets.iter().enumerate() {
        let size = target.rect.size();
        if size.x + 0.01 >= min && size.y + 0.01 >= min {
            continue;
        }
        assert_eq!(
            target.role,
            egui::accesskit::Role::Link,
            "{:?} is {size:?}",
            target.name
        );
        let centre = target.rect.center();
        for (j, other) in targets.iter().enumerate() {
            assert!(
                i == j || other.rect.distance_to_pos(centre) >= min / 2.0,
                "{:?} is {size:?} and too close to {:?}",
                target.name,
                other.name
            );
        }
    }
}

/// The targets `contents` lays out on a `size` screen, styled as a window is
/// (`gui::apply` with the default look), after two frames.
pub fn targets_at(size: [f32; 2], mut contents: impl FnMut(&mut egui::Ui)) -> Vec<Node> {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    crate::gui::apply(&ctx, &crate::appearance::Appearance::default());
    run_frame(&ctx, size, Vec::new(), &mut contents);
    targets(&run_frame(&ctx, size, Vec::new(), &mut contents))
}
