// This module is part of the tray icon infrastructure used in T4b/T4c.
// Types are public API but not yet wired into main, hence dead_code for now.
#![allow(dead_code)]

use crate::domain::PrimaryStatus;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform,
};

pub trait IconRenderer: Send + Sync {
    fn render(&self, status: PrimaryStatus, theme: &Theme) -> Vec<ksni::Icon>;
}

/// Цвета темы в формате RGBA (straight alpha).
pub struct Theme {
    pub normal: [u8; 4],
    pub low: [u8; 4],
    pub charging: [u8; 4],
    pub offline: [u8; 4],
}

impl Theme {
    /// Тёмная тема: светлый передний план (Nord off-white).
    pub fn dark() -> Self {
        Self {
            normal: [216, 222, 233, 255],
            low: [191, 97, 106, 255],
            charging: [163, 190, 140, 255],
            offline: [216, 222, 233, 180],
        }
    }

    /// Светлая тема: тёмный передний план (Nord polar night).
    pub fn light() -> Self {
        Self {
            normal: [59, 66, 82, 255],
            low: [191, 97, 106, 255],
            charging: [163, 190, 140, 255],
            offline: [59, 66, 82, 180],
        }
    }
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
    fn render(&self, status: PrimaryStatus, theme: &Theme) -> Vec<ksni::Icon> {
        let (color_rgba, fill_ratio, is_offline) = match status {
            PrimaryStatus::Offline => (theme.offline, 0.0_f32, true),
            PrimaryStatus::Charging { percent } => {
                (theme.charging, f32::from(percent) / 100.0, false)
            }
            PrimaryStatus::Low { percent } => (theme.low, f32::from(percent) / 100.0, false),
            PrimaryStatus::Ok { percent } => (theme.normal, f32::from(percent) / 100.0, false),
        };

        self.sizes
            .iter()
            .filter_map(|&size| render_icon(size, color_rgba, fill_ratio, is_offline))
            .collect()
    }
}

/// Рендерит одну иконку батареи размером `n×n`.
/// Возвращает `None` только если `Pixmap::new` не смог выделить буфер
/// (в практике не происходит для размеров <= 64).
#[allow(dead_code)]
fn render_icon(
    n: u32,
    color_rgba: [u8; 4],
    fill_ratio: f32,
    is_offline: bool,
) -> Option<ksni::Icon> {
    let mut pixmap = Pixmap::new(n, n)?;

    let s = n as f32;

    // Габариты корпуса батареи
    let body_x = s * 0.05;
    let body_y = s * 0.31;
    let body_w = s * 0.80;
    let body_h = s * 0.38;

    // Носик справа
    let nub_w = s * 0.06;
    let nub_h = s * 0.16;
    let nub_x = body_x + body_w;
    let nub_y = body_y + (body_h - nub_h) / 2.0;

    let stroke_w = (s * 0.06).max(1.0);
    let half_stroke = stroke_w / 2.0;

    let [r, g, b, a] = color_rgba;
    let paint_color = Color::from_rgba8(r, g, b, a);

    // Заливка пропорционально заряду
    if fill_ratio > 0.0 {
        let inner_x = body_x + half_stroke;
        let inner_y = body_y + half_stroke;
        let inner_h = body_h - stroke_w;
        let inner_w = (body_w - stroke_w) * fill_ratio;

        if inner_w > 0.0 && inner_h > 0.0 {
            let fill_rect = tiny_skia::Rect::from_xywh(inner_x, inner_y, inner_w, inner_h);
            if let Some(rect) = fill_rect {
                let mut fill_paint = Paint::default();
                fill_paint.set_color(paint_color);
                pixmap.fill_rect(rect, &fill_paint, Transform::identity(), None);
            }
        }
    }

    // Обводка корпуса
    {
        let mut stroke_paint = Paint::default();
        stroke_paint.set_color(paint_color);
        stroke_paint.anti_alias = true;

        let stroke = Stroke {
            width: stroke_w,
            line_cap: LineCap::Square,
            line_join: LineJoin::Miter,
            ..Stroke::default()
        };

        let mut pb = PathBuilder::new();
        pb.move_to(body_x, body_y);
        pb.line_to(body_x + body_w, body_y);
        pb.line_to(body_x + body_w, body_y + body_h);
        pb.line_to(body_x, body_y + body_h);
        pb.close();
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &stroke_paint, &stroke, Transform::identity(), None);
        }
    }

    // Носик
    {
        let nub_rect = tiny_skia::Rect::from_xywh(nub_x, nub_y, nub_w, nub_h);
        if let Some(rect) = nub_rect {
            let path = PathBuilder::from_rect(rect);
            let mut nub_paint = Paint::default();
            nub_paint.set_color(paint_color);
            pixmap.fill_path(
                &path,
                &nub_paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    // Диагональная линия для Offline
    if is_offline {
        let mut stroke_paint = Paint::default();
        stroke_paint.set_color(paint_color);
        stroke_paint.anti_alias = true;

        let stroke = Stroke {
            width: stroke_w,
            line_cap: LineCap::Round,
            ..Stroke::default()
        };

        let mut pb = PathBuilder::new();
        pb.move_to(body_x, body_y);
        pb.line_to(body_x + body_w, body_y + body_h);
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &stroke_paint, &stroke, Transform::identity(), None);
        }
    }

    // Конвертация: premultiplied RGBA → ARGB32 network byte order (A,R,G,B)
    let rgba_data = pixmap.data();
    let mut argb_data = Vec::with_capacity(rgba_data.len());
    for chunk in rgba_data.chunks_exact(4) {
        let pr = chunk[0];
        let pg = chunk[1];
        let pb = chunk[2];
        let pa = chunk[3];
        argb_data.push(pa);
        argb_data.push(pr);
        argb_data.push(pg);
        argb_data.push(pb);
    }

    Some(ksni::Icon {
        width: n as i32,
        height: n as i32,
        data: argb_data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_sizes() -> Vec<u32> {
        TinySkiaRenderer::default().sizes.clone()
    }

    #[test]
    fn ok_renders_correct_count() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(PrimaryStatus::Ok { percent: 50 }, &Theme::dark());
        assert_eq!(icons.len(), default_sizes().len());
    }

    #[test]
    fn ok_icon_dimensions_and_data_len() {
        let renderer = TinySkiaRenderer::default();
        let sizes = renderer.sizes.clone();
        let icons = renderer.render(PrimaryStatus::Ok { percent: 50 }, &Theme::dark());

        for icon in &icons {
            assert_eq!(icon.width, icon.height, "width must equal height");
            assert!(
                sizes.contains(&(icon.width as u32)),
                "width {} not in sizes {:?}",
                icon.width,
                sizes
            );
            let expected_len = (icon.width * icon.height * 4) as usize;
            assert_eq!(
                icon.data.len(),
                expected_len,
                "data len mismatch for size {}",
                icon.width
            );
        }
    }

    #[test]
    fn offline_renders_valid_icons() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(PrimaryStatus::Offline, &Theme::dark());
        assert_eq!(icons.len(), default_sizes().len());
        for icon in &icons {
            let expected_len = (icon.width * icon.height * 4) as usize;
            assert_eq!(icon.data.len(), expected_len);
        }
    }

    #[test]
    fn charging_renders_valid_icons() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(PrimaryStatus::Charging { percent: 75 }, &Theme::dark());
        assert_eq!(icons.len(), default_sizes().len());
        for icon in &icons {
            let expected_len = (icon.width * icon.height * 4) as usize;
            assert_eq!(icon.data.len(), expected_len);
        }
    }

    #[test]
    fn low_renders_valid_icons() {
        let renderer = TinySkiaRenderer::default();
        let icons = renderer.render(PrimaryStatus::Low { percent: 10 }, &Theme::dark());
        assert_eq!(icons.len(), default_sizes().len());
        for icon in &icons {
            let expected_len = (icon.width * icon.height * 4) as usize;
            assert_eq!(icon.data.len(), expected_len);
        }
    }

    #[test]
    fn dark_and_light_themes_differ_by_normal() {
        assert_ne!(
            Theme::dark().normal,
            Theme::light().normal,
            "dark and light themes must have different normal colors"
        );
    }

    #[test]
    fn render_dark_differs_from_light() {
        let renderer = TinySkiaRenderer::default();
        let status = PrimaryStatus::Ok { percent: 50 };
        let dark_icons = renderer.render(status, &Theme::dark());
        let light_icons = renderer.render(status, &Theme::light());

        assert!(!dark_icons.is_empty());
        assert!(!light_icons.is_empty());

        // Хотя бы один пиксель отличается в первой иконке
        let dark_data = &dark_icons[0].data;
        let light_data = &light_icons[0].data;
        assert_ne!(dark_data, light_data, "dark and light renders must differ");
    }
}
