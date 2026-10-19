use crate::domain::DeviceKind;

/// Guesses the device type from its display name using case-insensitive keyword
/// substring match, plus whole-word match for earbud names.
///
/// Match order: Mouse → Keyboard → Headset → Controller → Other.
/// The first matching category wins.
pub fn guess_kind(name: &str) -> DeviceKind {
    let lower = name.to_lowercase();

    // A category word states what the device is; a product-line word only
    // hints, and vendors reuse a line across categories — Logitech ships both
    // a "G Pro" mouse and a "G Pro X" headset. Categories must therefore be
    // matched first.
    const GENERIC: &[(&str, DeviceKind)] = &[
        ("mouse", DeviceKind::Mouse),
        ("keyboard", DeviceKind::Keyboard),
        ("headset", DeviceKind::Headset),
        ("headphone", DeviceKind::Headset),
        ("earphone", DeviceKind::Headset),
        ("earbud", DeviceKind::Headset),
        ("buds", DeviceKind::Headset),
        ("controller", DeviceKind::Controller),
        ("gamepad", DeviceKind::Controller),
    ];

    // Product lines and model numbers. Only consulted when no category word
    // appears, so a line name can never override an explicit one.
    const MODELS: &[(&str, DeviceKind)] = &[
        ("aerox", DeviceKind::Mouse),
        ("rival", DeviceKind::Mouse),
        ("viper", DeviceKind::Mouse),
        ("deathadder", DeviceKind::Mouse),
        ("basilisk", DeviceKind::Mouse),
        ("mx anywhere", DeviceKind::Mouse),
        ("mx master", DeviceKind::Mouse),
        ("g pro", DeviceKind::Mouse),
        ("m705", DeviceKind::Mouse),
        ("m750", DeviceKind::Mouse),
        ("apex", DeviceKind::Keyboard),
        ("huntsman", DeviceKind::Keyboard),
        ("k70", DeviceKind::Keyboard),
        ("k95", DeviceKind::Keyboard),
        ("g915", DeviceKind::Keyboard),
        ("air75", DeviceKind::Keyboard),
        ("air96", DeviceKind::Keyboard),
        ("nuphy", DeviceKind::Keyboard),
        ("keychron", DeviceKind::Keyboard),
        ("hitune", DeviceKind::Headset),
        ("arctis", DeviceKind::Headset),
        ("wh-", DeviceKind::Headset),
        ("wf-", DeviceKind::Headset),
        ("momentum", DeviceKind::Headset),
        ("qc", DeviceKind::Headset),
        ("dualsense", DeviceKind::Controller),
        ("dualshock", DeviceKind::Controller),
        ("xbox", DeviceKind::Controller),
        ("joycon", DeviceKind::Controller),
        ("8bitdo", DeviceKind::Controller),
    ];

    for (kw, kind) in GENERIC {
        if lower.contains(kw) {
            return *kind;
        }
    }
    // Whole words only: "ear" sits inside "Nearby", "Gear", "Clear".
    const HEADSET_WORDS: &[&str] = &[
        "ear",
        "buds",
        "earbuds",
        "airpods",
        "headphone",
        "headphones",
        "earphone",
    ];
    if lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| HEADSET_WORDS.contains(&word))
    {
        return DeviceKind::Headset;
    }
    for (kw, kind) in MODELS {
        if lower.contains(kw) {
            return *kind;
        }
    }

    DeviceKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Logitech ships a "G Pro" mouse and a "G Pro X" headset, so the
    /// category word has to outrank the product line.
    #[test]
    fn guess_kind_category_word_beats_product_line() {
        assert_eq!(
            guess_kind("Logitech G Pro X Wireless Headset"),
            DeviceKind::Headset
        );
        assert_eq!(guess_kind("Logitech G Pro Wireless"), DeviceKind::Mouse);
    }

    #[test]
    fn guess_kind_category_word_beats_product_line_for_keyboards() {
        assert_eq!(guess_kind("NuPhy Air75 V2-2"), DeviceKind::Keyboard);
        assert_eq!(
            guess_kind("Some Air75 Gaming Mouse"),
            DeviceKind::Mouse,
            "an explicit category word must win over a keyboard model number"
        );
    }

    #[test]
    fn guess_kind_mouse() {
        assert_eq!(guess_kind("MX Anywhere 3"), DeviceKind::Mouse);
    }

    #[test]
    fn guess_kind_keyboard() {
        assert_eq!(guess_kind("NuPhy Air75 V2-2"), DeviceKind::Keyboard);
    }

    #[test]
    fn guess_kind_headset() {
        assert_eq!(guess_kind("UGREEN HiTune Max5"), DeviceKind::Headset);
    }

    #[test]
    fn guess_kind_earbuds_are_headsets() {
        for name in [
            "Nothing Ear (2)",
            "Nothing Ear",
            "AirPods Pro",
            "Galaxy Buds+",
            "Sony Headphones",
        ] {
            assert_eq!(guess_kind(name), DeviceKind::Headset, "{name}");
        }
    }

    #[test]
    fn guess_kind_ear_inside_a_word_is_not_a_headset() {
        assert_eq!(guess_kind("Nearby Keyboard"), DeviceKind::Keyboard);
        assert_eq!(guess_kind("Nearby Tracker"), DeviceKind::Other);
        assert_eq!(guess_kind("Gear Tag"), DeviceKind::Other);
    }

    #[test]
    fn guess_kind_controller() {
        assert_eq!(guess_kind("DualSense"), DeviceKind::Controller);
    }

    #[test]
    fn guess_kind_other() {
        assert_eq!(guess_kind("Unknown Thing"), DeviceKind::Other);
    }

    #[test]
    fn guess_kind_case_insensitive() {
        assert_eq!(guess_kind("AEROX"), DeviceKind::Mouse);
    }
}
