use serde::{Deserialize, Serialize};

use crate::i18n::{Lang, fl, loader};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMode {
    #[default]
    IconOnly,
    PercentOnly,
    PercentInIcon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayMode {
    #[default]
    PrimaryOnly, // one aggregate icon (default)
    PerDevice, // one icon per shown device
}

/// The colours inside the system's light or dark scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Palette {
    #[default]
    Catppuccin,
    Everforest,
    Gnome,
    Nord,
}

impl Palette {
    pub const ALL: [Palette; 4] = [
        Palette::Catppuccin,
        Palette::Everforest,
        Palette::Gnome,
        Palette::Nord,
    ];

    pub fn label(self, lang: Lang) -> String {
        let l = loader(lang);
        match self {
            Palette::Catppuccin => fl!(l, "palette-catppuccin"),
            Palette::Everforest => fl!(l, "palette-everforest"),
            Palette::Gnome => fl!(l, "palette-gnome"),
            Palette::Nord => fl!(l, "palette-nord"),
        }
    }
}

impl DisplayMode {
    /// All display modes in display order, used to build the Settings menu.
    pub const ALL: [DisplayMode; 3] = [
        DisplayMode::IconOnly,
        DisplayMode::PercentOnly,
        DisplayMode::PercentInIcon,
    ];

    /// Human-readable label used in the settings window radio group.
    pub fn label(self, lang: Lang) -> String {
        let l = loader(lang);
        match self {
            DisplayMode::IconOnly => fl!(l, "display-icon-only"),
            DisplayMode::PercentOnly => fl!(l, "display-percent-only"),
            DisplayMode::PercentInIcon => fl!(l, "display-percent-in-icon"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_mode_all_has_three_variants() {
        assert_eq!(DisplayMode::ALL.len(), 3);
    }

    #[test]
    fn display_mode_all_contains_each_variant() {
        assert!(DisplayMode::ALL.contains(&DisplayMode::IconOnly));
        assert!(DisplayMode::ALL.contains(&DisplayMode::PercentOnly));
        assert!(DisplayMode::ALL.contains(&DisplayMode::PercentInIcon));
    }

    #[test]
    fn display_mode_label_non_empty() {
        for lang in Lang::ALL {
            for mode in DisplayMode::ALL {
                assert!(
                    !mode.label(lang).is_empty(),
                    "label for {mode:?} is empty in {lang:?}"
                );
            }
        }
    }

    #[test]
    fn display_mode_label_values() {
        assert_eq!(DisplayMode::IconOnly.label(Lang::En), "Battery icon only");
        assert_eq!(
            DisplayMode::PercentOnly.label(Lang::En),
            "Percentage as text"
        );
        assert_eq!(
            DisplayMode::PercentInIcon.label(Lang::En),
            "Percentage inside icon"
        );
        assert_eq!(DisplayMode::IconOnly.label(Lang::Ru), "Только значок");
    }

    #[test]
    fn every_palette_has_a_label_in_every_language() {
        for lang in Lang::ALL {
            for palette in Palette::ALL {
                assert!(!palette.label(lang).is_empty(), "{palette:?} in {lang:?}");
            }
        }
    }
}
