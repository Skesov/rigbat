#![allow(dead_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMode {
    #[default]
    IconOnly,
    PercentOnly,
    PercentInIcon,
    PercentBesideIcon,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub display_mode: DisplayMode,
    /// Device names to display as icons. Empty list = show all.
    pub shown_devices: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            display_mode: DisplayMode::IconOnly,
            shown_devices: Vec::new(),
        }
    }
}

impl Config {
    /// Returns true if the device should be shown (empty list = show all).
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
            display_mode: DisplayMode::PercentBesideIcon,
            shown_devices: vec!["mouse".to_string(), "keyboard".to_string()],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, restored);
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
        };
        assert!(cfg.is_shown("mouse"));
        assert!(!cfg.is_shown("keyboard"));
        assert!(!cfg.is_shown("headset"));
    }
}
