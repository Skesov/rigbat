//! Colour tables for each `Palette`, and the WCAG rule that keeps them readable.

use crate::appearance::ColorScheme;
use crate::domain::Palette;

pub type Rgb = [u8; 3];

/// Opacity of a reading that is not live, and of an offline icon, on every surface.
/// Not the 0.38 of a disabled control: a retained reading is still read.
pub const DIM: f32 = 0.70;
/// WCAG 2.1 SC 1.4.3, normal-size text.
pub const TEXT_CONTRAST: f32 = 4.5;
/// WCAG 2.1 SC 1.4.11, graphical objects.
pub const GRAPHIC_CONTRAST: f32 = 3.0;

const BLACK: Rgb = [0, 0, 0];
const WHITE: Rgb = [255, 255, 255];
const STEPS: u16 = 255;

/// One palette in one scheme, as its authors publish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swatches {
    pub bg: Rgb,
    pub fg: Rgb,
    pub charging: Rgb,
    pub low: Rgb,
    pub warn: Rgb,
    pub track: Rgb,
}

const fn swatches_of(bg: u32, fg: u32, charging: u32, low: u32, warn: u32, track: u32) -> Swatches {
    Swatches {
        bg: rgb(bg),
        fg: rgb(fg),
        charging: rgb(charging),
        low: rgb(low),
        warn: rgb(warn),
        track: rgb(track),
    }
}

const fn rgb(hex: u32) -> Rgb {
    let [_, r, g, b] = hex.to_be_bytes();
    [r, g, b]
}

pub const fn swatches(palette: Palette, scheme: ColorScheme) -> Swatches {
    use ColorScheme::{Dark, Light};
    match (palette, scheme) {
        (Palette::Catppuccin, Light) => {
            swatches_of(0xeff1f5, 0x4c4f69, 0x40a02b, 0xd20f39, 0xdf8e1d, 0xccd0da)
        }
        (Palette::Catppuccin, Dark) => {
            swatches_of(0x1e1e2e, 0xcdd6f4, 0xa6e3a1, 0xf38ba8, 0xf9e2af, 0x45475a)
        }
        (Palette::Everforest, Light) => {
            swatches_of(0xfdf6e3, 0x5c6a72, 0x8da101, 0xf85552, 0xdfa000, 0xe6e2cc)
        }
        (Palette::Everforest, Dark) => {
            swatches_of(0x2d353b, 0xd3c6aa, 0xa7c080, 0xe67e80, 0xdbbc7f, 0x475258)
        }
        (Palette::Gnome, Light) => {
            swatches_of(0xfafafb, 0x2e3436, 0x26a269, 0xc01c28, 0xe5a50a, 0xdeddda)
        }
        (Palette::Gnome, Dark) => {
            swatches_of(0x222226, 0xffffff, 0x33d17a, 0xf66151, 0xf6d32d, 0x3d3846)
        }
        (Palette::Nord, Light) => {
            swatches_of(0xeceff4, 0x3b4252, 0x456035, 0xbf616a, 0xd08770, 0xd8dee9)
        }
        (Palette::Nord, Dark) => {
            swatches_of(0x2e3440, 0xd8dee9, 0xa3be8c, 0xbf616a, 0xebcb8b, 0x434c5e)
        }
    }
}

/// `color` darkened or lightened along its own hue, away from `surface`, until
/// painted at `opacity` over `surface` it reaches `min_ratio`; as is if it already does.
pub fn readable(color: Rgb, surface: Rgb, min_ratio: f32, opacity: f32) -> Rgb {
    let target = if contrast_ratio(surface, BLACK) >= contrast_ratio(surface, WHITE) {
        BLACK
    } else {
        WHITE
    };
    (0..=STEPS)
        .map(|step| mix(color, target, f32::from(step) / f32::from(STEPS)))
        .find(|&c| contrast_ratio(over(c, surface, opacity), surface) >= min_ratio)
        .unwrap_or(target)
}

/// `color` painted at `opacity` over `surface`.
pub fn over(color: Rgb, surface: Rgb, opacity: f32) -> Rgb {
    mix(surface, color, opacity)
}

fn mix(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let channel = |a: u8, b: u8| {
        let (a, b) = (f32::from(a), f32::from(b));
        (a + (b - a) * t).round().clamp(0.0, 255.0) as u8
    };
    [
        channel(from[0], to[0]),
        channel(from[1], to[1]),
        channel(from[2], to[2]),
    ]
}

/// WCAG 2.1 contrast ratio between two opaque colours.
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn relative_luminance([r, g, b]: Rgb) -> f32 {
    let linear = |v: u8| {
        let s = f32::from(v) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMES: [ColorScheme; 2] = [ColorScheme::Light, ColorScheme::Dark];

    fn luminance(c: Rgb) -> f32 {
        relative_luminance(c)
    }

    /// Hue in degrees, `None` for a colour too grey to have a stable one.
    fn hue([r, g, b]: Rgb) -> Option<f32> {
        let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let chroma = max - min;
        if chroma < 24.0 {
            return None;
        }
        let h = if max == r {
            ((g - b) / chroma).rem_euclid(6.0)
        } else if max == g {
            (b - r) / chroma + 2.0
        } else {
            (r - g) / chroma + 4.0
        };
        Some(h * 60.0)
    }

    fn roles(s: &Swatches) -> [(&'static str, Rgb); 4] {
        [
            ("fg", s.fg),
            ("charging", s.charging),
            ("low", s.low),
            ("warn", s.warn),
        ]
    }

    #[test]
    fn a_readable_colour_is_left_as_the_table_has_it() {
        let s = swatches(Palette::Gnome, ColorScheme::Light);
        assert!(contrast_ratio(s.fg, s.bg) >= TEXT_CONTRAST);
        assert_eq!(readable(s.fg, s.bg, TEXT_CONTRAST, 1.0), s.fg);
    }

    #[test]
    fn every_role_reaches_the_ratio_on_its_own_hue_away_from_the_surface() {
        for palette in Palette::ALL {
            for scheme in SCHEMES {
                let s = swatches(palette, scheme);
                for (role, color) in roles(&s) {
                    for (min, opacity) in [(TEXT_CONTRAST, 1.0), (GRAPHIC_CONTRAST, DIM)] {
                        let out = readable(color, s.bg, min, opacity);
                        let ratio = contrast_ratio(over(out, s.bg, opacity), s.bg);
                        let at = format!("{palette:?} {scheme:?} {role} at {min}:1");
                        assert!(ratio >= min, "{at}: {ratio:.2}:1");
                        if out == color {
                            continue;
                        }
                        match scheme {
                            ColorScheme::Light => {
                                assert!(luminance(out) < luminance(color), "{at}")
                            }
                            ColorScheme::Dark => assert!(luminance(out) > luminance(color), "{at}"),
                        }
                        if let (Some(before), Some(after)) = (hue(color), hue(out)) {
                            let drift = (before - after).abs().min(360.0 - (before - after).abs());
                            assert!(drift < 6.0, "{at}: hue {before:.0}° → {after:.0}°");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dimming_is_measured_as_painted_over_the_surface() {
        let surface = [240, 240, 240];
        let green = rgb(0x40a02b);
        let opaque = readable(green, surface, GRAPHIC_CONTRAST, 1.0);
        let dimmed = readable(green, surface, GRAPHIC_CONTRAST, DIM);
        assert!(luminance(dimmed) < luminance(opaque));
        assert!(contrast_ratio(over(dimmed, surface, DIM), surface) >= GRAPHIC_CONTRAST);
    }

    #[test]
    fn contrast_ratio_spans_one_to_twenty_one() {
        assert!((contrast_ratio(BLACK, WHITE) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(WHITE, WHITE) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn every_palette_differs_in_both_schemes() {
        for scheme in SCHEMES {
            for (i, a) in Palette::ALL.iter().enumerate() {
                for b in Palette::ALL.iter().skip(i + 1) {
                    assert_ne!(swatches(*a, scheme), swatches(*b, scheme), "{a:?} {b:?}");
                }
            }
        }
    }
}
