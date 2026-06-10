use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub display_mode: DisplayMode,
    /// Device names to display as icons. Empty list = show all.
    // wired in D2 (Visible devices) / U4b (multi-icon)
    #[allow(dead_code)]
    pub shown_devices: Vec<String>,
    /// Whether to send desktop notifications for low-battery crossings.
    /// Defaults to true; old config files without this key load as true
    /// because `#[serde(default)]` on the struct fills missing fields from Default.
    pub notifications_enabled: bool,
    /// Whether to show one tray icon per visible device or one aggregate icon.
    pub tray_mode: TrayMode,
    /// User-chosen device for the aggregate (PrimaryOnly) icon.
    /// None = automatic (first connected among shown devices).
    pub primary_device: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            display_mode: DisplayMode::IconOnly,
            shown_devices: Vec::new(),
            notifications_enabled: true,
            tray_mode: TrayMode::PrimaryOnly,
            primary_device: None,
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
    pub fn label(self) -> &'static str {
        match self {
            DisplayMode::IconOnly => "Battery icon only",
            DisplayMode::PercentOnly => "Percentage as text",
            DisplayMode::PercentInIcon => "Percentage inside icon",
        }
    }
}

impl Config {
    /// Returns true if the device should be shown (empty list = show all).
    // wired in D2 (Visible devices) / U4b (multi-icon)
    #[allow(dead_code)]
    pub fn is_shown(&self, name: &str) -> bool {
        self.shown_devices.is_empty() || self.shown_devices.iter().any(|n| n == name)
    }
}

/// ~/.config/rigbat/config.json (XDG). None if home directory is not available.
pub fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "rigbat")
        .map(|dirs| dirs.config_dir().join("config.json"))
}

/// Reads config. File missing or invalid → Config::default() (log invalid files,
/// do not panic). Never panics.
pub fn load() -> Config {
    let path = match config_path() {
        Some(p) => p,
        None => return Config::default(),
    };

    let data = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Config::default(),
    };

    match serde_json::from_str::<Config>(&data) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("rigbat: config parse error ({path:?}): {e}");
            Config::default()
        }
    }
}

/// Saves atomically: create directory, write to temp file, rename.
pub fn save(config: &Config) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let path = config_path().context("cannot determine config directory")?;

    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create config directory {dir:?}"))?;

    let json = serde_json::to_string_pretty(config).context("failed to serialize config")?;

    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &json)
        .with_context(|| format!("failed to write temp config {tmp_path:?}"))?;

    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("failed to rename {tmp_path:?} to {path:?}"))?;

    Ok(())
}

/// Returns `true` if any path in the event matches the target config file name.
/// Comparing by file name is sufficient because the watch is non-recursive and
/// scoped to the config directory.
fn event_touches(event: &notify::Event, target: &Path) -> bool {
    let name = target.file_name();
    event.paths.iter().any(|p| p.file_name() == name)
}

/// Watches the config file's parent directory and pushes a fresh [`Config`] into
/// `tx` whenever the file changes on disk. Quietly does nothing if the path or
/// the watcher is unavailable (config watching is best-effort, never fatal).
pub fn watch_file(tx: tokio::sync::watch::Sender<Config>) {
    let Some(path) = config_path() else { return };
    let Some(dir) = path.parent().map(|p| p.to_path_buf()) else {
        return;
    };

    // Ensure the directory exists before watching. Best-effort.
    let _ = std::fs::create_dir_all(&dir);

    std::thread::spawn(move || {
        let (raw_tx, raw_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();

        // `recommended_watcher` uses a callback that receives `Result<Event>`.
        // Forward every item to the std::sync::mpsc channel so this thread can
        // process events synchronously without any async runtime.
        let mut watcher = match notify::recommended_watcher(move |ev| {
            let _ = raw_tx.send(ev);
        }) {
            Ok(w) => w,
            Err(_) => return,
        };

        // Watch the parent directory non-recursively so rename-based atomic
        // writes (temp file → rename) are captured.
        use notify::Watcher as _;
        if watcher
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .is_err()
        {
            return;
        }

        // Keep `watcher` alive for the lifetime of this loop.
        for result in raw_rx {
            let Ok(event) = result else { continue };
            if event_touches(&event, &path) {
                let cfg = load();
                tx.send_if_modified(|cur| {
                    if *cur != cfg {
                        *cur = cfg;
                        true
                    } else {
                        false
                    }
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.display_mode, DisplayMode::IconOnly);
        assert!(cfg.shown_devices.is_empty());
    }

    #[test]
    fn serde_round_trip() {
        let cfg = Config {
            display_mode: DisplayMode::PercentInIcon,
            shown_devices: vec!["mouse".to_string(), "keyboard".to_string()],
            notifications_enabled: false,
            tray_mode: TrayMode::PerDevice,
            primary_device: Some("mouse".to_string()),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, restored);
    }

    #[test]
    fn empty_json_tray_mode_defaults_primary_only() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.tray_mode, TrayMode::PrimaryOnly);
    }

    #[test]
    fn empty_json_notifications_enabled_defaults_true() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.notifications_enabled);
    }

    #[test]
    fn empty_json_primary_device_defaults_none() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.primary_device, None);
    }

    #[test]
    fn display_mode_snake_case() {
        let mode = DisplayMode::PercentInIcon;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, r#""percent_in_icon""#);
    }

    #[test]
    fn partial_json_uses_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn is_shown_empty_list_allows_all() {
        let cfg = Config::default();
        assert!(cfg.is_shown("mouse"));
        assert!(cfg.is_shown("keyboard"));
        assert!(cfg.is_shown("anything"));
    }

    #[test]
    fn is_shown_whitelist_filters() {
        let cfg = Config {
            display_mode: DisplayMode::IconOnly,
            shown_devices: vec!["mouse".to_string()],
            notifications_enabled: true,
            tray_mode: TrayMode::PrimaryOnly,
            primary_device: None,
        };
        assert!(cfg.is_shown("mouse"));
        assert!(!cfg.is_shown("keyboard"));
        assert!(!cfg.is_shown("headset"));
    }

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
        for mode in DisplayMode::ALL {
            assert!(!mode.label().is_empty(), "label for {mode:?} is empty");
        }
    }

    #[test]
    fn display_mode_label_values() {
        assert_eq!(DisplayMode::IconOnly.label(), "Battery icon only");
        assert_eq!(DisplayMode::PercentOnly.label(), "Percentage as text");
        assert_eq!(DisplayMode::PercentInIcon.label(), "Percentage inside icon");
    }

    #[test]
    fn event_touches_matching_filename() {
        use std::path::PathBuf;
        let target = PathBuf::from("/home/user/.config/rigbat/config.json");
        let event = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from("/home/user/.config/rigbat/config.json")],
            attrs: Default::default(),
        };
        assert!(event_touches(&event, &target));
    }

    #[test]
    fn event_touches_non_matching_filename() {
        use std::path::PathBuf;
        let target = PathBuf::from("/home/user/.config/rigbat/config.json");
        let event = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from("/home/user/.config/rigbat/other.json")],
            attrs: Default::default(),
        };
        assert!(!event_touches(&event, &target));
    }

    #[test]
    fn event_touches_tmp_file_excluded() {
        use std::path::PathBuf;
        let target = PathBuf::from("/home/user/.config/rigbat/config.json");
        let event = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from("/home/user/.config/rigbat/config.json.tmp")],
            attrs: Default::default(),
        };
        assert!(!event_touches(&event, &target));
    }
}
