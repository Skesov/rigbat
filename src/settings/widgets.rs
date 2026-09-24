//! Preference-page building blocks: a titled boxed list of rows, a row with a
//! title on the left and its control on the right, a switch, picture tiles,
//! and underlined tabs.

use eframe::egui;

pub const CONTENT_MAX_WIDTH: f32 = 640.0;

const ROW_MIN_HEIGHT: f32 = 48.0;
const ROW_PADDING_X: f32 = 14.0;
const ROW_PADDING_Y: f32 = 8.0;
const ROW_GAP: f32 = 16.0;
const GROUP_RADIUS: u8 = 8;
const GROUP_TITLE_GAP: f32 = 6.0;
const GROUP_GAP: f32 = 20.0;
const FOOTER_GAP: f32 = 6.0;
const SECONDARY_SCALE: f32 = 0.88;
const FOOTER_SPACING: f32 = 4.0;

const SWITCH_SIZE: egui::Vec2 = egui::vec2(40.0, 22.0);
const KNOB_INSET: f32 = 3.0;
const FOCUS_GAP: f32 = 2.5;
const FOCUS_WIDTH: f32 = 1.5;

const TILE_HEIGHT: f32 = 76.0;
pub const TILE_GAP: f32 = 10.0;
const TILE_CAPTION_GAP: f32 = 8.0;
const SELECTED_TILE_STROKE: f32 = 2.0;

const TAB_PADDING: egui::Vec2 = egui::vec2(12.0, 6.0);
const TAB_UNDERLINE: f32 = 3.0;

/// A strong title above a rounded box whose rows are split by separators, with
/// optional weak text under the box.
pub fn group(
    ui: &mut egui::Ui,
    title: &str,
    footer: Option<&str>,
    add_rows: impl FnOnce(&mut Rows<'_>),
) {
    indented(ui, |ui| {
        ui.label(egui::RichText::new(title).strong());
    });
    ui.add_space(GROUP_TITLE_GAP);
    let id = ui.id().with(("preference-group", title));
    let visuals = ui.visuals();
    egui::Frame::new()
        .fill(visuals.faint_bg_color)
        .stroke(visuals.widgets.noninteractive.bg_stroke)
        .corner_radius(GROUP_RADIUS)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add_rows(&mut Rows { ui, id, count: 0 });
        });
    if let Some(footer) = footer {
        ui.add_space(FOOTER_GAP);
        indented(ui, |ui| {
            ui.label(secondary(ui, footer));
        });
    }
    ui.add_space(GROUP_GAP);
}

/// Text outside a group's box, inset to line up with its rounded corner.
fn indented(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let inset = (ROW_PADDING_X / 2.0) as i8;
    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: inset,
            right: inset,
            ..egui::Margin::ZERO
        })
        .show(ui, add);
}

/// The rows of one `group`.
pub struct Rows<'a> {
    ui: &'a mut egui::Ui,
    id: egui::Id,
    count: usize,
}

impl Rows<'_> {
    /// Title and `subtitle` on the left, `control` right-aligned.
    pub fn row<R>(
        &mut self,
        title: &str,
        subtitle: impl FnOnce(&mut egui::Ui),
        control: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        self.separator();
        let text_id = self.id.with(self.count);
        let width = self.ui.available_width();
        self.ui
            .allocate_ui_with_layout(
                egui::vec2(width, ROW_MIN_HEIGHT),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.set_min_height(ROW_MIN_HEIGHT);
                    ui.add_space(ROW_PADDING_X);
                    let inner = control(ui);
                    ui.add_space(ROW_GAP);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                        ui.add_space(ROW_PADDING_X);
                        ui.vertical(|ui| centred_text(ui, text_id, title, subtitle));
                    });
                    inner
                },
            )
            .inner
    }

    /// Title on top, `content` below it across the whole row.
    pub fn block(&mut self, title: &str, content: impl FnOnce(&mut egui::Ui)) {
        self.separator();
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(
                ROW_PADDING_X as i8,
                (ROW_PADDING_Y + 2.0) as i8,
            ))
            .show(self.ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(title);
                ui.add_space(ROW_PADDING_Y);
                content(ui);
            });
    }

    fn separator(&mut self) {
        if self.count > 0 {
            let y = self.ui.cursor().top();
            let x = self.ui.max_rect().x_range();
            let stroke = self.ui.visuals().widgets.noninteractive.bg_stroke;
            self.ui.painter().hline(x, y, stroke);
        }
        self.count += 1;
    }
}

/// Title and subtitle, centred on the row's height when they are shorter.
///
/// A child `Ui` cannot be centred before its contents are laid out, so the
/// block's height is remembered from the last pass, and a pass that finds it
/// changed is discarded and rerun before it reaches the screen.
fn centred_text(
    ui: &mut egui::Ui,
    id: egui::Id,
    title: &str,
    subtitle: impl FnOnce(&mut egui::Ui),
) {
    let remembered = ui.ctx().data(|d| d.get_temp::<f32>(id)).unwrap_or(0.0);
    let padding = ((ROW_MIN_HEIGHT - remembered) / 2.0).max(ROW_PADDING_Y);
    ui.add_space(padding);
    let top = ui.cursor().top();
    ui.label(title);
    subtitle(ui);
    let height = ui.min_rect().bottom() - top;
    ui.add_space(padding);
    if (height - remembered).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(id, height));
        ui.ctx().request_discard("preference row height");
    }
}

/// Secondary text: dimmer and a little smaller than the body.
pub fn secondary(ui: &egui::Ui, text: &str) -> egui::RichText {
    let size = egui::TextStyle::Body.resolve(ui.style()).size * SECONDARY_SCALE;
    egui::RichText::new(text).size(size).weak()
}

/// A row subtitle: one weak line that wraps when it must.
pub fn subtitle(text: &str) -> impl FnOnce(&mut egui::Ui) + '_ {
    move |ui| {
        ui.label(secondary(ui, text));
    }
}

/// No subtitle.
pub fn none(_: &mut egui::Ui) {}

/// An on/off switch. `id` is global so that a test can find it; `label` is
/// what a screen reader announces.
pub fn switch(ui: &mut egui::Ui, id: egui::Id, on: &mut bool, label: &str) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(SWITCH_SIZE, egui::Sense::hover());
    let mut response = ui.interact(rect, id, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let enabled = ui.is_enabled();
    let state = *on;
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, state, label)
    });

    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(id, *on);
        let visuals = ui.visuals();
        let off_fill = visuals.widgets.inactive.bg_fill;
        let on_fill = visuals.selection.bg_fill;
        let mut track = off_fill.lerp_to_gamma(on_fill, how_on);
        if response.hovered() {
            track = track.lerp_to_gamma(visuals.text_color(), 0.08);
        }
        let radius = rect.height() / 2.0;
        let painter = ui.painter();
        painter.rect(
            rect,
            radius,
            track,
            visuals.widgets.noninteractive.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let knob_radius = radius - KNOB_INSET;
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        painter.circle(
            egui::pos2(x, rect.center().y),
            knob_radius,
            egui::Color32::WHITE,
            egui::Stroke::new(0.5, egui::Color32::from_black_alpha(60)),
        );
        if response.has_focus() {
            focus_ring(ui, rect, radius);
        }
    }
    response
}

/// One of a row of picture choices: `image` above `caption`, outlined with
/// the accent when selected.
pub fn tile(
    ui: &mut egui::Ui,
    width: f32,
    selected: bool,
    image: &egui::TextureHandle,
    image_size: f32,
    caption: &str,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, TILE_HEIGHT), egui::Sense::click());
    let enabled = ui.is_enabled();
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::RadioButton, enabled, selected, caption)
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.visuals();
    let stroke = if selected {
        egui::Stroke::new(SELECTED_TILE_STROKE, visuals.selection.bg_fill)
    } else if response.hovered() {
        visuals.widgets.hovered.bg_stroke
    } else {
        visuals.widgets.noninteractive.bg_stroke
    };
    let radius = f32::from(GROUP_RADIUS) - 2.0;
    let painter = ui.painter();
    painter.rect(
        rect,
        radius,
        visuals.extreme_bg_color,
        stroke,
        egui::StrokeKind::Inside,
    );

    let caption_color = if selected {
        visuals.strong_text_color()
    } else {
        visuals.text_color()
    };
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = painter.layout(
        caption.to_owned(),
        font,
        caption_color,
        rect.width() - 2.0 * TILE_CAPTION_GAP,
    );
    let content_height = image_size + TILE_CAPTION_GAP + galley.size().y;
    let top = rect.center().y - content_height / 2.0;
    let image_rect = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, top + image_size / 2.0),
        egui::Vec2::splat(image_size),
    );
    painter.image(
        image.id(),
        image_rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    let caption_pos = egui::pos2(
        rect.center().x - galley.size().x / 2.0,
        image_rect.bottom() + TILE_CAPTION_GAP,
    );
    painter.galley(caption_pos, galley, caption_color);
    if response.has_focus() {
        focus_ring(ui, rect, radius);
    }
    response
}

/// Width of each of `count` tiles sharing `available` width.
pub fn tile_width(available: f32, count: usize) -> f32 {
    let count = count.max(1) as f32;
    (available - TILE_GAP * (count - 1.0)) / count
}

/// Centred view-switcher tabs over a full-width rule; returns the index of
/// the tab clicked this frame.
pub fn tab_bar(ui: &mut egui::Ui, labels: &[String], selected: usize) -> Option<usize> {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let width: f32 = labels
        .iter()
        .map(|label| text_width(ui, label, &font) + 2.0 * TAB_PADDING.x)
        .sum();
    let rule = ui.painter().add(egui::Shape::Noop);
    let mut clicked = None;
    let bar = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
        for (index, label) in labels.iter().enumerate() {
            if tab(ui, index == selected, label).clicked() {
                clicked = Some(index);
            }
        }
    });
    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    ui.painter().set(
        rule,
        egui::Shape::hline(
            ui.max_rect().x_range(),
            bar.response.rect.bottom() - stroke.width / 2.0,
            stroke,
        ),
    );
    clicked
}

fn tab(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, egui::Color32::PLACEHOLDER);
    let size = galley.size() + 2.0 * TAB_PADDING + egui::vec2(0.0, TAB_UNDERLINE);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let enabled = ui.is_enabled();
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, enabled, selected, label)
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.visuals();
    let color = if selected {
        visuals.strong_text_color()
    } else if response.hovered() {
        visuals.text_color()
    } else {
        visuals.weak_text_color()
    };
    let painter = ui.painter();
    painter.galley(rect.min + TAB_PADDING, galley, color);
    if selected {
        let underline = egui::Rect::from_min_max(
            egui::pos2(
                rect.left() + TAB_PADDING.x / 2.0,
                rect.bottom() - TAB_UNDERLINE,
            ),
            egui::pos2(rect.right() - TAB_PADDING.x / 2.0, rect.bottom()),
        );
        painter.rect_filled(underline, TAB_UNDERLINE / 2.0, visuals.selection.bg_fill);
    }
    if response.has_focus() {
        focus_ring(ui, rect, 4.0);
    }
    response
}

/// `control` laid out left to right in a box `width` wide at the right of a
/// row, whose own layout runs right to left.
pub fn trailing<R>(ui: &mut egui::Ui, width: f32, control: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ui.available_height()),
        egui::Layout::left_to_right(egui::Align::Center),
        control,
    )
    .inner
}

/// One weak, centred line: `text` followed by a link.
pub fn footer(ui: &mut egui::Ui, text: &str, link: &str, url: &str) {
    let mut font = egui::TextStyle::Body.resolve(ui.style());
    font.size *= SECONDARY_SCALE;
    let width = text_width(ui, text, &font) + FOOTER_SPACING + text_width(ui, link, &font);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = FOOTER_SPACING;
        ui.add_space(((ui.available_width() - width) / 2.0 - FOOTER_SPACING).max(0.0));
        ui.label(secondary(ui, text));
        ui.hyperlink_to(egui::RichText::new(link).size(font.size), url);
    });
}

fn text_width(ui: &egui::Ui, text: &str, font: &egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
        .size()
        .x
}

fn focus_ring(ui: &egui::Ui, rect: egui::Rect, radius: f32) {
    let color = ui.visuals().widgets.hovered.fg_stroke.color;
    ui.painter().rect_stroke(
        rect.expand(FOCUS_GAP),
        radius + FOCUS_GAP,
        egui::Stroke::new(FOCUS_WIDTH, color),
        egui::StrokeKind::Outside,
    );
}
