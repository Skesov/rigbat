//! The General tab's footer: version, commit hash and project link.

use eframe::egui;

use super::widgets::{COPIED_FOR, focus_ring, secondary, secondary_font, text_width};
use crate::gui;

const FOOTER_SPACING: f32 = 4.0;
const FOOTER_SEPARATOR: &str = "·";

/// One weak, centred line: `text`, the commit hash if there is one, and a
/// link, split by dots; returns the link's response, the caller opens the target.
pub fn footer(
    ui: &mut egui::Ui,
    text: &str,
    commit: Option<&CommitHash<'_>>,
    link: &str,
) -> egui::Response {
    let font = secondary_font(ui);
    let separator = text_width(ui, FOOTER_SEPARATOR, &font) + 2.0 * FOOTER_SPACING;
    let mut width = text_width(ui, text, &font) + separator + text_width(ui, link, &font);
    if let Some(commit) = commit {
        width += commit.slot_width(ui, &font) + separator;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = FOOTER_SPACING;
        ui.add_space(((ui.available_width() - width) / 2.0 - FOOTER_SPACING).max(0.0));
        ui.label(secondary(ui, text));
        if let Some(commit) = commit {
            ui.label(secondary(ui, FOOTER_SEPARATOR));
            commit.show(ui, &font);
        }
        ui.label(secondary(ui, FOOTER_SEPARATOR));
        ui.link(egui::RichText::new(link).size(font.size))
    })
    .inner
}

/// A commit hash in the footer that copies itself when clicked and reads
/// `copied` in its place for `COPIED_FOR`.
pub struct CommitHash<'a> {
    pub hash: &'a str,
    pub copied: &'a str,
    /// What a screen reader announces for it.
    pub label: &'a str,
}

impl CommitHash<'_> {
    /// Wide enough for either text, so the line does not shift between them.
    fn slot_width(&self, ui: &egui::Ui, font: &egui::FontId) -> f32 {
        text_width(ui, self.hash, font).max(text_width(ui, self.copied, font))
    }

    fn show(&self, ui: &mut egui::Ui, font: &egui::FontId) {
        let color = gui::secondary_text(ui.visuals());
        let height = ui
            .painter()
            .layout_no_wrap(self.hash.to_owned(), font.clone(), color)
            .size()
            .y;
        let size = egui::vec2(self.slot_width(ui, font), height.max(gui::MIN_TARGET));
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        let now = ui.input(|input| input.time);
        if response.clicked() {
            ui.ctx().copy_text(self.hash.to_owned());
            ui.data_mut(|data| data.insert_temp(response.id, now));
        }
        let copied_at = ui.data(|data| data.get_temp::<f64>(response.id));
        let left = copied_at.map_or(0.0, |at| at + COPIED_FOR - now);
        let text = if left > 0.0 {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(left));
            self.copied
        } else {
            self.hash
        };
        let enabled = ui.is_enabled();
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, self.label)
        });
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);

        if ui.is_rect_visible(rect) {
            let galley = ui
                .painter()
                .layout_no_wrap(text.to_owned(), font.clone(), color);
            let text_rect = egui::Align2::CENTER_CENTER.anchor_size(rect.center(), galley.size());
            let painter = ui.painter();
            if response.hovered() {
                painter.hline(
                    text_rect.x_range(),
                    text_rect.bottom(),
                    egui::Stroke::new(1.0_f32, color),
                );
            }
            painter.galley(text_rect.min, galley, color);
            if response.has_focus() {
                focus_ring(ui, rect, 2.0);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::egui_test::{
        assert_whole, click_at, fully_painted_text_at, run_frame, run_frame_at,
    };
    use crate::settings::widgets::{CONTENT_MAX_WIDTH, PANEL_MARGIN};

    const SIZE: [f32; 2] = [CONTENT_MAX_WIDTH, 600.0];

    pub(in crate::settings) const COMMIT: CommitHash<'static> = CommitHash {
        hash: "e3cd47f",
        copied: "Copied",
        label: "Copy commit hash",
    };

    fn show_footer(ui: &mut egui::Ui) {
        footer(ui, "rigbat 1.0", Some(&COMMIT), "GitHub");
    }

    fn painted_text(output: &egui::FullOutput) -> Vec<String> {
        crate::egui_test::painted(output)
            .into_iter()
            .map(|p| p.text)
            .collect()
    }

    #[test]
    fn clicking_the_hash_copies_it_and_reads_copied_until_the_timeout() {
        let ctx = egui::Context::default();
        let output = run_frame(&ctx, SIZE, Vec::new(), show_footer);
        let hash = crate::egui_test::painted(&output)
            .into_iter()
            .find(|p| p.text == COMMIT.hash)
            .expect("the hash was painted");

        let output = click_at(&ctx, SIZE, hash.rect.center(), show_footer);
        assert!(
            output
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText(COMMIT.hash.to_owned())),
            "{:?}",
            output.platform_output.commands
        );
        let announced = output.platform_output.events.iter().any(|event| {
            let info = event.widget_info();
            info.typ == egui::WidgetType::Button && info.label.as_deref() == Some(COMMIT.label)
        });
        assert!(announced, "{:?}", output.platform_output.events);

        let output = run_frame(&ctx, SIZE, Vec::new(), show_footer);
        let painted = painted_text(&output);
        assert!(painted.iter().any(|t| t == COMMIT.copied), "{painted:?}");
        assert!(!painted.iter().any(|t| t == COMMIT.hash), "{painted:?}");
        let repaint = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        assert!(
            repaint <= std::time::Duration::from_secs_f64(COPIED_FOR),
            "{repaint:?}"
        );

        let output = run_frame_at(&ctx, SIZE, COPIED_FOR + 1.0, show_footer);
        let painted = painted_text(&output);
        assert!(painted.iter().any(|t| t == COMMIT.hash), "{painted:?}");
        assert!(!painted.iter().any(|t| t == COMMIT.copied), "{painted:?}");
    }

    #[test]
    fn the_footer_keeps_its_place_while_the_hash_reads_copied() {
        let ctx = egui::Context::default();
        let output = run_frame(&ctx, SIZE, Vec::new(), show_footer);
        let link_before = crate::egui_test::painted(&output)
            .into_iter()
            .find(|p| p.text == "GitHub")
            .expect("the link was painted");
        let hash = crate::egui_test::painted(&output)
            .into_iter()
            .find(|p| p.text == COMMIT.hash)
            .expect("the hash was painted");
        click_at(&ctx, SIZE, hash.rect.center(), show_footer);
        let output = run_frame(&ctx, SIZE, Vec::new(), show_footer);
        let link_after = crate::egui_test::painted(&output)
            .into_iter()
            .find(|p| p.text == "GitHub")
            .expect("the link was painted");
        assert_eq!(link_before.rect, link_after.rect);
    }

    /// Both texts of the hash slot, in every language and text scale, fit whole.
    #[test]
    fn the_footer_is_whole_in_every_language_and_text_scale() {
        use crate::i18n::{Lang, fl, loader};
        for (lang, scale) in Lang::ALL
            .into_iter()
            .flat_map(|lang| crate::egui_test::TEXT_SCALES.map(|scale| (lang, scale)))
        {
            let l = loader(lang);
            let (copied, label) = (fl!(l, "about-commit-copied"), fl!(l, "about-commit-copy"));
            let github = fl!(l, "about-github");
            for text in [COMMIT.hash, copied.as_str()] {
                let commit = CommitHash {
                    hash: text,
                    copied: &copied,
                    label: &label,
                };
                let width = gui::zoomed_width(crate::settings::WINDOW_MIN_SIZE, scale);
                let size = [width - 2.0 * PANEL_MARGIN, SIZE[1]];
                let painted = fully_painted_text_at(size, |ui| {
                    footer(ui, "rigbat 0.4.0+12", Some(&commit), &github);
                });
                let expected = ["rigbat 0.4.0+12", text, github.as_str()].map(str::to_owned);
                assert_whole(&painted, &expected, &format!("{lang:?} at {scale}"));
            }
        }
    }
}
