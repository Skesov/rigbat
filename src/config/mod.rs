use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{DisplayMode, Palette, TrayMode};
use crate::i18n::{self, Lang};

const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;
pub const DEFAULT_LOW_THRESHOLD: u8 = 20;

/// Per-device poll interval and low-threshold overrides.
/// Missing fields fall back to the global `Config` values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    pub poll_interval_secs: Option<u64>,
    pub low_threshold: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub display_mode: DisplayMode,
    /// Device names to hide from the tray and menu. Empty list = show all.
    /// A name this list never mentions is shown, including one for a device
    /// that is not currently discovered — the field only ever grows or
    /// shrinks by exactly the name a checkbox toggle names, never by
    /// rebuilding from whatever devices happen to be visible right now.
    pub hidden_devices: Vec<String>,
    /// Deprecated pre-T33 whitelist ("who to show"). Editing it through a
    /// partial device view could silently erase entries for devices absent
    /// from that view — the field it was replaced by, `hidden_devices`
    /// ("who to hide"), cannot have that failure mode because toggling one
    /// device never touches any other entry. Converted into `hidden_devices`
    /// once, by `app::supervisor` right after its first discovery sweep —
    /// not here: `config::load` has no device roster to convert against.
    /// This version never writes to it again once converted. Kept only so
    /// an old config file still deserializes.
    pub shown_devices: Vec<String>,
    /// Whether to send desktop notifications for low-battery crossings.
    /// Defaults to true; old config files without this key load as true
    /// because `#[serde(default)]` on the struct fills missing fields from Default.
    pub notifications_enabled: bool,
    /// Whether to show one tray icon per visible device or one aggregate icon.
    pub tray_mode: TrayMode,
    /// User-chosen device for the aggregate (PrimaryOnly) icon.
    /// None = automatic (the connected shown device with the lowest charge).
    pub primary_device: Option<String>,
    /// Global poll interval in seconds. Applies to all devices unless overridden.
    pub poll_interval_secs: u64,
    /// Global low-battery threshold (percent). Applies to all devices unless overridden.
    pub low_threshold: u8,
    /// Per-device overrides keyed by device name.
    pub device_overrides: HashMap<String, DeviceSettings>,
    /// UI language tag (`"ru"`); `None` follows the session locale. A string, not
    /// `Lang`, so a tag unknown to this build still loads.
    pub language: Option<String>,
    /// Colours inside the session's light or dark scheme.
    pub palette: Palette,
}

/// Moves every per-device setting from `from` to `to`, returning whether any
/// existed. Called when the inventory sees a device keep its transport and
/// locator — its hardware identity — while its name changes, which is what a
/// BlueZ alias edit or a firmware-supplied name change looks like from here.
///
/// Config is keyed by display name (the standing decision behind
/// `dedup-adr.md`), so without this a rename silently resets the device to
/// defaults: it reappears in the tray after being hidden, loses its threshold
/// and interval overrides, and stops being the aggregate icon's device.
pub fn rename_device(cfg: &mut Config, from: &str, to: &str) -> bool {
    let mut changed = false;

    for entry in &mut cfg.hidden_devices {
        if entry == from {
            *entry = to.to_string();
            changed = true;
        }
    }
    // Keep the first entry for `to` and drop any later one. `Vec::dedup` is
    // wrong here: it only collapses *adjacent* duplicates, so renaming with
    // `["old", "other", "new"]` left `["new", "other", "new"]`.
    let mut seen_to = false;
    cfg.hidden_devices
        .retain(|entry| entry != to || !std::mem::replace(&mut seen_to, true));

    if let Some(settings) = cfg.device_overrides.remove(from) {
        cfg.device_overrides.insert(to.to_string(), settings);
        changed = true;
    }

    if cfg.primary_device.as_deref() == Some(from) {
        cfg.primary_device = Some(to.to_string());
        changed = true;
    }

    changed
}

impl Default for Config {
    fn default() -> Self {
        Self {
            display_mode: DisplayMode::IconOnly,
            hidden_devices: Vec::new(),
            shown_devices: Vec::new(),
            notifications_enabled: true,
            tray_mode: TrayMode::PrimaryOnly,
            primary_device: None,
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            low_threshold: DEFAULT_LOW_THRESHOLD,
            device_overrides: HashMap::new(),
            language: None,
            palette: Palette::Catppuccin,
        }
    }
}

impl Config {
    pub fn lang(&self) -> Lang {
        i18n::resolve(self.language.as_deref())
    }

    /// Returns true if the device should be shown (absent from `hidden_devices`).
    pub fn is_shown(&self, name: &str) -> bool {
        !self.hidden_devices.iter().any(|n| n == name)
    }

    /// Effective poll interval for `name`: device override → global → clamp to at least 1.
    pub fn effective_poll_interval_secs(&self, name: &str) -> u64 {
        self.device_overrides
            .get(name)
            .and_then(|d| d.poll_interval_secs)
            .unwrap_or(self.poll_interval_secs)
            .max(1)
    }

    /// Effective low-battery threshold for `name`: device override → global →
    /// clamp to the percent domain.
    ///
    /// The clamp is not decoration: `serde` accepts any `u8`, and the settings
    /// window's 5-50 range does not constrain a hand-edited `config.json`. A
    /// stored `250` would make `percent <= threshold` true for every
    /// discharging device, painting a full battery red and reporting `low` to
    /// waybar forever. Clamping keeps a nonsensical value inside the domain
    /// `BatteryReading::percent` already guarantees.
    pub fn effective_low_threshold(&self, name: &str) -> u8 {
        self.device_overrides
            .get(name)
            .and_then(|d| d.low_threshold)
            .unwrap_or(self.low_threshold)
            .min(100)
    }
}

/// ~/.config/rigbat/config.json (XDG). None if home directory is not available.
pub fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "rigbat")
        .map(|dirs| dirs.config_dir().join("config.json"))
}

/// Reads config. File missing or invalid → Config::default() (log invalid files,
/// do not panic). Never panics.
///
/// Never converts a legacy `shown_devices` whitelist into `hidden_devices`:
/// that conversion needs a discovered device roster, which no caller of
/// `load()` has at hand (the tray does not have one yet either, this early).
/// `app::supervisor` performs it once, after its first discovery sweep.
pub fn load() -> Config {
    match config_path() {
        Some(path) => load_from(&path),
        None => Config::default(),
    }
}

/// Reads the config at `path`, exactly as stored. Split out of `load()` so
/// tests can exercise it against a temporary file instead of the real
/// `config_path()`. `pub(crate)` so `settings::save_edit`'s own tests can use
/// the same seam.
pub(crate) fn load_from(path: &Path) -> Config {
    match read(path) {
        Ok(cfg) => cfg.unwrap_or_default(),
        Err(e) => {
            tracing::warn!("{e:#}; using defaults");
            Config::default()
        }
    }
}

/// Reads and parses the config at `path`: `Ok(None)` if there is no file,
/// `Err` if it cannot be read or does not parse.
pub fn read(path: &Path) -> anyhow::Result<Option<Config>> {
    use anyhow::Context as _;
    let data = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str::<Config>(&data)
        .map(Some)
        .with_context(|| format!("config parse error ({})", path.display()))
}

/// Saves atomically: create directory, write to temp file, rename.
pub fn save(config: &Config) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let path = config_path().context("cannot determine config directory")?;
    save_to(&path, config)
}

/// Writes `config` atomically to `path`. Split out of `save()` so tests can
/// exercise a save/load round trip against a temporary file instead of the
/// real `config_path()`. `pub(crate)` so `settings::save_edit`'s own tests
/// can use the same seam.
///
/// The temp file is named after this process's pid and a per-process
/// counter (`unique_tmp_path`), not a fixed `config.json.tmp`: the tray and
/// a settings window can both save around the same moment, and a fixed name
/// let whichever process's `rename` won take a file the other was still
/// writing. On any failure the temp file is removed rather than left behind.
pub(crate) fn save_to(path: &Path, config: &Config) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create config directory {dir:?}"))?;

    let json = serde_json::to_string_pretty(config).context("failed to serialize config")?;

    let tmp_path = unique_tmp_path(path, std::process::id());
    let result = std::fs::write(&tmp_path, &json)
        .with_context(|| format!("failed to write temp config {tmp_path:?}"))
        .and_then(|()| {
            std::fs::rename(&tmp_path, path)
                .with_context(|| format!("failed to rename {tmp_path:?} to {path:?}"))
        });

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// Builds the atomic-save temp path for `path`, unique to `pid` and to this
/// call within that process (`config.json` → `config.json.<pid>-<n>.tmp`).
/// `pid` is a parameter, not `std::process::id()` read internally, so tests
/// can simulate two different processes without spawning real ones.
fn unique_tmp_path(path: &Path, pid: u32) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("json.{pid}-{n}.tmp"))
}

/// Returns `true` if any path in the event matches the target config file name.
/// Comparing by file name is sufficient because the watch is non-recursive and
/// scoped to the config directory.
fn event_touches(event: &notify::Event, target: &Path) -> bool {
    let name = target.file_name();
    event.paths.iter().any(|p| p.file_name() == name)
}

/// True for events that can change the config file's content or identity
/// (create, data write, rename). Excludes `Access` (open/read/close) and
/// metadata-only events — our own `load()` reads otherwise generate Access
/// events on the watched file and feed an infinite inotify loop.
fn is_content_change(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    use notify::event::ModifyKind;
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Name(_))
            | EventKind::Modify(ModifyKind::Any)
    )
}

/// Watches the config file's parent directory and pushes a fresh [`Config`] into
/// `tx` whenever the file changes on disk. Quietly does nothing if the path or
/// the watcher is unavailable (config watching is best-effort, never fatal).
pub fn watch_file(tx: tokio::sync::watch::Sender<Config>) {
    let Some(path) = config_path() else {
        tracing::warn!(
            "no config directory available; settings changes will not apply until restart"
        );
        return;
    };
    let Some(dir) = path.parent().map(|p| p.to_path_buf()) else {
        tracing::warn!(
            "config path {path:?} has no parent directory; settings changes will not apply until restart"
        );
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
            Err(e) => {
                tracing::warn!(
                    "failed to create config file watcher: {e}; settings changes will not apply until restart"
                );
                return;
            }
        };

        // Watch the parent directory non-recursively so rename-based atomic
        // writes (temp file → rename) are captured.
        use notify::Watcher as _;
        if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
            tracing::warn!(
                "failed to watch config directory {dir:?}: {e}; settings changes will not apply until restart"
            );
            return;
        }

        // Keep `watcher` alive for the lifetime of this loop.
        for result in raw_rx {
            let Ok(event) = result else { continue };
            if is_content_change(&event.kind) && event_touches(&event, &path) {
                let cfg = load();
                let applied = tx.send_if_modified(|cur| {
                    if *cur != cfg {
                        *cur = cfg;
                        true
                    } else {
                        false
                    }
                });
                if applied {
                    tracing::debug!("config reload applied");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn rename_device_moves_every_per_device_setting() {
        let mut cfg = Config {
            hidden_devices: vec!["old".to_string(), "other".to_string()],
            primary_device: Some("old".to_string()),
            ..Config::default()
        };
        cfg.device_overrides.insert(
            "old".to_string(),
            DeviceSettings {
                low_threshold: Some(35),
                poll_interval_secs: None,
            },
        );

        assert!(rename_device(&mut cfg, "old", "new"));
        assert_eq!(
            cfg.hidden_devices,
            vec!["new".to_string(), "other".to_string()]
        );
        assert_eq!(cfg.primary_device, Some("new".to_string()));
        assert!(!cfg.device_overrides.contains_key("old"));
        assert_eq!(
            cfg.device_overrides
                .get("new")
                .and_then(|d| d.low_threshold),
            Some(35)
        );
    }

    /// `Vec::dedup` collapses only adjacent duplicates, so a hidden list where
    /// the old and new names are separated by another device kept both.
    #[test]
    fn rename_device_does_not_duplicate_a_non_adjacent_hidden_entry() {
        let mut cfg = Config {
            hidden_devices: vec!["old".to_string(), "other".to_string(), "new".to_string()],
            ..Config::default()
        };

        assert!(rename_device(&mut cfg, "old", "new"));
        assert_eq!(
            cfg.hidden_devices,
            vec!["new".to_string(), "other".to_string()]
        );
    }

    #[test]
    fn rename_device_reports_no_change_when_the_name_is_unknown() {
        let mut cfg = Config {
            hidden_devices: vec!["other".to_string()],
            ..Config::default()
        };

        assert!(!rename_device(&mut cfg, "old", "new"));
        assert_eq!(cfg.hidden_devices, vec!["other".to_string()]);
    }

    /// A device hidden under both names — possible if it was seen under the
    /// new name before the rename was noticed — must end up hidden once.
    #[test]
    fn rename_device_does_not_duplicate_a_hidden_entry() {
        let mut cfg = Config {
            hidden_devices: vec!["old".to_string(), "new".to_string()],
            ..Config::default()
        };

        assert!(rename_device(&mut cfg, "old", "new"));
        assert_eq!(cfg.hidden_devices, vec!["new".to_string()]);
    }

    use super::*;

    #[test]
    fn default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.display_mode, DisplayMode::IconOnly);
        assert!(cfg.hidden_devices.is_empty());
    }

    #[test]
    fn serde_round_trip() {
        let cfg = Config {
            display_mode: DisplayMode::PercentInIcon,
            hidden_devices: vec!["mouse".to_string(), "keyboard".to_string()],
            notifications_enabled: false,
            tray_mode: TrayMode::PerDevice,
            primary_device: Some("mouse".to_string()),
            palette: Palette::Everforest,
            ..Config::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""palette":"everforest""#), "{json}");
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
    fn a_config_without_a_palette_loads_catppuccin() {
        let cfg: Config = serde_json::from_str(r#"{"display_mode":"percent_only"}"#).unwrap();
        assert_eq!(cfg.palette, Palette::Catppuccin);
        assert_eq!(cfg.display_mode, DisplayMode::PercentOnly);
    }

    #[test]
    fn partial_json_uses_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn is_shown_empty_hidden_devices_allows_all() {
        let cfg = Config::default();
        assert!(cfg.is_shown("mouse"));
        assert!(cfg.is_shown("keyboard"));
        assert!(cfg.is_shown("anything"));
    }

    #[test]
    fn is_shown_hides_only_the_named_device() {
        let cfg = Config {
            display_mode: DisplayMode::IconOnly,
            hidden_devices: vec!["mouse".to_string()],
            notifications_enabled: true,
            tray_mode: TrayMode::PrimaryOnly,
            primary_device: None,
            ..Config::default()
        };
        assert!(!cfg.is_shown("mouse"));
        assert!(cfg.is_shown("keyboard"));
        // Never mentioned in hidden_devices — shown by default, including a
        // name that is not currently discovered.
        assert!(cfg.is_shown("headset"));
    }

    // --- load_from / save_to round trip ---------------------------------------

    /// Unique scratch config file path under the OS temp dir for one test.
    /// Never the real `config_path()` — these tests must not touch
    /// `~/.config/rigbat/config.json`.
    fn scratch_config_path(test_name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rigbat-config-test-{test_name}-{}-{n}.json",
            std::process::id()
        ))
    }

    #[test]
    fn save_then_load_preserves_hidden_devices() {
        let path = scratch_config_path("round-trip");
        let cfg = Config {
            hidden_devices: vec!["mouse".to_string(), "headset".to_string()],
            ..Config::default()
        };
        save_to(&path, &cfg).unwrap();
        let restored = load_from(&path);
        assert_eq!(restored.hidden_devices, cfg.hidden_devices);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let path = scratch_config_path("missing");
        assert_eq!(load_from(&path), Config::default());
    }

    #[test]
    fn read_tells_missing_from_broken() {
        let path = scratch_config_path("read");
        assert!(read(&path).unwrap().is_none());
        std::fs::write(&path, "{ not json").unwrap();
        let err = format!("{:#}", read(&path).unwrap_err());
        assert!(err.contains("config parse error"), "{err}");
        assert_eq!(load_from(&path), Config::default());
        save_to(&path, &Config::default()).unwrap();
        assert_eq!(read(&path).unwrap(), Some(Config::default()));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unique_tmp_path_differs_between_processes() {
        let path = PathBuf::from("/tmp/rigbat-test/config.json");
        assert_ne!(unique_tmp_path(&path, 111), unique_tmp_path(&path, 222));
    }

    #[test]
    fn unique_tmp_path_differs_within_same_process() {
        let path = PathBuf::from("/tmp/rigbat-test/config.json");
        assert_ne!(unique_tmp_path(&path, 111), unique_tmp_path(&path, 111));
    }

    /// Unique scratch directory (not file) under the OS temp dir, for tests
    /// that need to inspect the directory's contents rather than one path.
    fn scratch_config_dir(test_name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rigbat-config-test-dir-{test_name}-{}-{n}",
            std::process::id()
        ))
    }

    /// Forces `save_to` to fail after the temp file is written: `path` is
    /// made an existing directory, so `rename(tmp_path, path)` fails with
    /// EISDIR. Regression test for C5's cleanup requirement — the earlier
    /// version left the temp file behind on any failure.
    #[test]
    fn save_to_cleans_up_temp_file_on_rename_failure() {
        let dir = scratch_config_dir("save-failure");
        let path = dir.join("config.json");
        std::fs::create_dir_all(&path).unwrap();

        assert!(save_to(&path, &Config::default()).is_err());

        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "config.json")
            .collect();
        assert!(leftover.is_empty(), "temp file left behind: {leftover:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `load_from` must not convert a legacy `shown_devices` whitelist — it
    /// has no device roster to convert against. `app::supervisor` performs
    /// that conversion, once, after its first discovery sweep.
    #[test]
    fn load_from_does_not_migrate_legacy_shown_devices() {
        let path = scratch_config_path("no-migration-on-load");
        std::fs::write(&path, r#"{"shown_devices": ["mouse"]}"#).unwrap();
        let cfg = load_from(&path);
        assert_eq!(cfg.shown_devices, vec!["mouse".to_string()]);
        assert!(cfg.hidden_devices.is_empty());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn language_is_optional_and_tolerates_unknown_tags() {
        let old: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(old.language, None);

        let ru: Config = serde_json::from_str(r#"{"language":"ru"}"#).unwrap();
        assert_eq!(ru.lang(), Lang::Ru);

        let newer: Config = serde_json::from_str(r#"{"language":"de"}"#).unwrap();
        assert_eq!(newer.lang(), i18n::system());
    }

    #[test]
    fn is_content_change_access_read_is_false() {
        let kind = notify::EventKind::Access(notify::event::AccessKind::Read);
        assert!(!is_content_change(&kind));
    }

    #[test]
    fn is_content_change_access_open_is_false() {
        let kind = notify::EventKind::Access(notify::event::AccessKind::Open(
            notify::event::AccessMode::Any,
        ));
        assert!(!is_content_change(&kind));
    }

    #[test]
    fn is_content_change_modify_data_is_true() {
        let kind = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));
        assert!(is_content_change(&kind));
    }

    #[test]
    fn is_content_change_modify_name_to_is_true() {
        let kind = notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::To,
        ));
        assert!(is_content_change(&kind));
    }

    #[test]
    fn is_content_change_create_file_is_true() {
        let kind = notify::EventKind::Create(notify::event::CreateKind::File);
        assert!(is_content_change(&kind));
    }

    #[test]
    fn is_content_change_modify_metadata_is_false() {
        let kind = notify::EventKind::Modify(notify::event::ModifyKind::Metadata(
            notify::event::MetadataKind::AccessTime,
        ));
        assert!(!is_content_change(&kind));
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

    // --- effective_poll_interval_secs ---------------------------------------

    #[test]
    fn effective_poll_interval_secs_global_default_when_no_override() {
        let cfg = Config::default();
        assert_eq!(
            cfg.effective_poll_interval_secs("mouse"),
            DEFAULT_POLL_INTERVAL_SECS
        );
    }

    #[test]
    fn effective_poll_interval_secs_override_wins() {
        let mut cfg = Config::default();
        cfg.device_overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: None,
            },
        );
        assert_eq!(cfg.effective_poll_interval_secs("mouse"), 30);
        // Other devices still use global.
        assert_eq!(
            cfg.effective_poll_interval_secs("keyboard"),
            DEFAULT_POLL_INTERVAL_SECS
        );
    }

    #[test]
    fn effective_poll_interval_secs_zero_clamped_to_one() {
        let mut cfg = Config::default();
        cfg.device_overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(0),
                low_threshold: None,
            },
        );
        assert_eq!(cfg.effective_poll_interval_secs("mouse"), 1);
    }

    #[test]
    fn effective_poll_interval_secs_global_zero_clamped_to_one() {
        let cfg = Config {
            poll_interval_secs: 0,
            ..Config::default()
        };
        assert_eq!(cfg.effective_poll_interval_secs("mouse"), 1);
    }

    // --- effective_low_threshold --------------------------------------------

    #[test]
    fn effective_low_threshold_global_default_when_no_override() {
        let cfg = Config::default();
        assert_eq!(cfg.effective_low_threshold("mouse"), DEFAULT_LOW_THRESHOLD);
    }

    #[test]
    fn effective_low_threshold_override_wins() {
        let mut cfg = Config::default();
        cfg.device_overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(10),
            },
        );
        assert_eq!(cfg.effective_low_threshold("mouse"), 10);
        assert_eq!(
            cfg.effective_low_threshold("keyboard"),
            DEFAULT_LOW_THRESHOLD
        );
    }

    /// A hand-edited config.json can carry any `u8`; above 100 every
    /// discharging device would classify as low forever.
    #[test]
    fn effective_low_threshold_clamps_above_full_charge() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(255),
            },
        );
        let cfg = Config {
            low_threshold: 250,
            device_overrides: overrides,
            ..Config::default()
        };
        assert_eq!(cfg.effective_low_threshold("mouse"), 100);
        assert_eq!(cfg.effective_low_threshold("keyboard"), 100);
    }

    // --- serde with device_overrides ----------------------------------------

    #[test]
    fn serde_round_trip_with_device_overrides() {
        let mut cfg = Config {
            poll_interval_secs: 30,
            low_threshold: 15,
            ..Config::default()
        };
        cfg.device_overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(10),
                low_threshold: Some(5),
            },
        );
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, restored);
    }

    #[test]
    fn empty_json_poll_interval_and_threshold_default() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(cfg.low_threshold, DEFAULT_LOW_THRESHOLD);
        assert!(cfg.device_overrides.is_empty());
    }
}
