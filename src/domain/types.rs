#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Mouse,
    Keyboard,
    Headset,
    Controller,
    Other,
}

/// Guesses the device type from its display name using case-insensitive keyword substring match.
///
/// Match order: Mouse → Keyboard → Headset → Controller → Other.
/// The first matching category wins.
pub fn guess_kind(name: &str) -> DeviceKind {
    let lower = name.to_lowercase();

    const MOUSE_KEYWORDS: &[&str] = &[
        "mouse",
        "aerox",
        "rival",
        "viper",
        "deathadder",
        "basilisk",
        "mx anywhere",
        "mx master",
        "g pro",
        "m705",
        "m750",
    ];
    const KEYBOARD_KEYWORDS: &[&str] = &[
        "keyboard", "apex", "huntsman", "k70", "k95", "g915", "air75", "air96", "nuphy", "keychron",
    ];
    const HEADSET_KEYWORDS: &[&str] = &[
        "headphone",
        "headset",
        "earphone",
        "buds",
        "hitune",
        "arctis",
        "wh-",
        "wf-",
        "momentum",
        "qc",
    ];
    const CONTROLLER_KEYWORDS: &[&str] = &[
        "dualsense",
        "dualshock",
        "controller",
        "gamepad",
        "xbox",
        "joycon",
    ];

    if MOUSE_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return DeviceKind::Mouse;
    }
    if KEYBOARD_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return DeviceKind::Keyboard;
    }
    if HEADSET_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return DeviceKind::Headset;
    }
    if CONTROLLER_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return DeviceKind::Controller;
    }

    DeviceKind::Other
}

/// Returns the freedesktop icon-theme name for a device kind.
///
/// The returned string can be used directly as a `gtk::Image` icon name.
pub fn freedesktop_icon_name(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Mouse => "input-mouse",
        DeviceKind::Keyboard => "input-keyboard",
        DeviceKind::Headset => "audio-headset",
        DeviceKind::Controller => "input-gaming",
        DeviceKind::Other => "battery",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Discharging,
    Charging,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub percent: u8, // invariant: 0..=100
    pub state: ChargeState,
}

impl BatteryReading {
    /// Creates a reading, constraining percent to range 0..=100.
    pub fn new(percent: u8, state: ChargeState) -> Self {
        Self {
            percent: percent.min(100),
            state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn freedesktop_icon_name_mouse() {
        assert_eq!(freedesktop_icon_name(DeviceKind::Mouse), "input-mouse");
    }

    #[test]
    fn freedesktop_icon_name_keyboard() {
        assert_eq!(
            freedesktop_icon_name(DeviceKind::Keyboard),
            "input-keyboard"
        );
    }

    #[test]
    fn freedesktop_icon_name_headset() {
        assert_eq!(freedesktop_icon_name(DeviceKind::Headset), "audio-headset");
    }

    #[test]
    fn freedesktop_icon_name_controller() {
        assert_eq!(
            freedesktop_icon_name(DeviceKind::Controller),
            "input-gaming"
        );
    }

    #[test]
    fn freedesktop_icon_name_other() {
        assert_eq!(freedesktop_icon_name(DeviceKind::Other), "battery");
    }
}
