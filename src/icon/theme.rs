use crate::appearance::ColorScheme;
use crate::domain::Palette;
use crate::palette::{self, DIM, GRAPHIC_CONTRAST, Rgb};

/// Theme colors in RGBA format (straight alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub normal: [u8; 4],
    pub low: [u8; 4],
    pub charging: [u8; 4],
    pub offline: [u8; 4],
}

/// Stand-ins for the host's panel, which rigbat cannot see, when measuring contrast.
pub(super) const NOMINAL_PANEL_DARK: Rgb = [0x1e, 0x1e, 0x1e];
pub(super) const NOMINAL_PANEL_LIGHT: Rgb = [0xf0, 0xf0, 0xf0];

impl Theme {
    /// The palette's colours, each a graphical object of at least 3:1 on the
    /// nominal panel; a colour that may be dimmed passes dimmed.
    pub fn new(palette: Palette, scheme: ColorScheme) -> Self {
        let s = palette::swatches(palette, scheme);
        let panel = match scheme {
            ColorScheme::Dark => NOMINAL_PANEL_DARK,
            ColorScheme::Light => NOMINAL_PANEL_LIGHT,
        };
        let graphic = |color, opacity| {
            let [r, g, b] = palette::readable(color, panel, GRAPHIC_CONTRAST, opacity);
            [r, g, b, 255]
        };
        let normal = graphic(s.fg, DIM);
        Self {
            normal,
            low: graphic(s.low, 1.0),
            charging: graphic(s.charging, DIM),
            offline: dimmed(normal),
        }
    }
}

pub(super) fn dimmed([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    [
        r,
        g,
        b,
        (f32::from(a) * DIM).round().clamp(0.0, 255.0) as u8,
    ]
}
