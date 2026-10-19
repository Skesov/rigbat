use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::domain::{
    DisplayMode, OFFLINE_SHELF_LIFE_MAX, OFFLINE_SHELF_LIFE_MIN, Palette, TrayMode, WindowTheme,
};
use crate::i18n::{self, Lang};

const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;
pub const DEFAULT_LOW_THRESHOLD: u8 = 20;
const DEFAULT_HIDE_OFFLINE_AFTER_MINS: u32 = 120;

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
    /// Device names to hide from the tray and menu; any other name is shown.
    /// Edited one name at a time, never rebuilt from the devices visible now.
    pub hidden_devices: Vec<String>,
    /// Deprecated whitelist ("who to show"), kept so an old file loads.
    /// `app::supervisor` converts it into `hidden_devices` once, after its
    /// first discovery sweep: loading has no device roster to convert against.
    pub shown_devices: Vec<String>,
    /// Whether to send desktop notifications for low-battery crossings.
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
    /// Minutes a device that is not online stays on the status surfaces after
    /// its last reading. Read through `hide_offline_after`.
    pub hide_offline_after_mins: u32,
    /// UI language tag (`"ru"`); `None` follows the session locale. A string, not
    /// `Lang`, so a tag unknown to this build still loads.
    pub language: Option<String>,
    /// Colours inside the light or dark scheme.
    pub palette: Palette,
    pub theme: WindowTheme,
}

/// Moves every per-device setting from `from` to `to`, returning whether any
/// existed. Called when the inventory sees a device keep its hardware
/// identity under a new name (a BlueZ alias edit). Config is keyed by display
/// name, so without this a rename would reset the device to defaults.
pub fn rename_device(cfg: &mut Config, from: &str, to: &str) -> bool {
    let mut changed = false;

    for entry in &mut cfg.hidden_devices {
        if entry == from {
            *entry = to.to_string();
            changed = true;
        }
    }
    // Keeps the first entry for `to`. Not `Vec::dedup`: it only collapses adjacent duplicates.
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
            hide_offline_after_mins: DEFAULT_HIDE_OFFLINE_AFTER_MINS,
            language: None,
            palette: Palette::Catppuccin,
            theme: WindowTheme::System,
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
    /// clamp to the percent domain, since a hand-edited `config.json` can hold
    /// any `u8` and above 100 every discharging device would read low.
    pub fn effective_low_threshold(&self, name: &str) -> u8 {
        self.device_overrides
            .get(name)
            .and_then(|d| d.low_threshold)
            .unwrap_or(self.low_threshold)
            .min(100)
    }

    /// How long a device that is not online stays shown after its last
    /// reading, clamped into `OFFLINE_SHELF_LIFE_MIN..=OFFLINE_SHELF_LIFE_MAX`
    /// whatever a hand-edited `config.json` says.
    pub fn hide_offline_after(&self) -> Duration {
        Duration::from_secs(u64::from(self.hide_offline_after_mins) * 60)
            .clamp(OFFLINE_SHELF_LIFE_MIN, OFFLINE_SHELF_LIFE_MAX)
    }
}

/// ~/.config/rigbat (XDG). None if home directory is not available.
fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "rigbat").map(|dirs| dirs.config_dir().to_path_buf())
}

/// ~/.config/rigbat/config.json (XDG). None if home directory is not available.
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.json"))
}

/// Reads config. File missing or invalid → Config::default() in memory only: an
/// invalid file is logged and kept, since `save_to` refuses to write over it.
/// Never converts `shown_devices`, which needs a device roster.
pub fn load() -> Config {
    match config_path() {
        Some(path) => load_from(&path),
        None => Config::default(),
    }
}

/// `load` for an explicit `path`, so tests never touch the real file.
pub(crate) fn load_from(path: &Path) -> Config {
    match read(path) {
        Ok(cfg) => cfg.unwrap_or_default(),
        Err(e) => {
            tracing::warn!("{e:#}; using defaults, nothing is saved until it is fixed");
            Config::default()
        }
    }
}

/// Reads and parses the config at `path`: `Ok(None)` if there is no file,
/// `Err` if it cannot be read or does not parse.
pub fn read(path: &Path) -> anyhow::Result<Option<Config>> {
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
    let path = config_path().context("cannot determine config directory")?;
    save_to(&path, config)
}

/// Writes `config` atomically to `path`. The temp file is unique per process
/// and call (`unique_tmp_path`): the tray and a settings window can save at
/// the same moment. It is removed on any failure.
///
/// Refuses with [`Unreadable`] while the file at `path` exists but does not
/// read or parse, so no writer replaces a broken-but-recoverable file with
/// defaults plus one change.
pub(crate) fn save_to(path: &Path, config: &Config) -> anyhow::Result<()> {
    read(path).map_err(|e| e.context(Unreadable))?;

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

/// Why a save was refused: the file on disk does not read or parse.
#[derive(Debug, Clone, Copy)]
pub struct Unreadable;

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not saving over a config file that does not parse; fix or remove it first")
    }
}

/// `config.json` → `config.json.<pid>-<n>.tmp`, unique to `pid` and to this
/// call. `pid` is a parameter so tests can simulate two processes.
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
    let Some(dir) = config_dir() else {
        tracing::warn!(
            "no config directory available; settings changes will not apply until restart"
        );
        return;
    };
    let path = dir.join("config.json");

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

        // `load()` at startup already warned about a file that was broken then.
        let mut broken = read(&path).is_err();
        // Keep `watcher` alive for the lifetime of this loop.
        for result in raw_rx {
            let Ok(event) = result else { continue };
            if is_content_change(&event.kind) && event_touches(&event, &path) {
                reload(&path, &tx, &mut broken);
            }
        }
    });
}

/// Applies the file at `path` to `tx`. A file that does not parse keeps the
/// current config live and warns once per broken spell; no file means defaults.
fn reload(path: &Path, tx: &tokio::sync::watch::Sender<Config>, broken: &mut bool) {
    let cfg = match read(path) {
        Ok(cfg) => cfg.unwrap_or_default(),
        Err(e) => {
            if !*broken {
                tracing::warn!("{e:#}; keeping the current settings until it is fixed");
            }
            *broken = true;
            return;
        }
    };
    if std::mem::take(broken) {
        tracing::info!("config parses again");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ScratchDir;

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

    #[test]
    fn serde_round_trip() {
        let cfg = Config {
            display_mode: DisplayMode::DeviceAndBattery,
            hidden_devices: vec!["mouse".to_string(), "keyboard".to_string()],
            notifications_enabled: false,
            tray_mode: TrayMode::PerDevice,
            primary_device: Some("mouse".to_string()),
            palette: Palette::Everforest,
            theme: WindowTheme::Light,
            ..Config::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""palette":"everforest""#), "{json}");
        assert!(json.contains(r#""theme":"light""#), "{json}");
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, restored);
    }

    #[test]
    fn display_mode_snake_case() {
        let mode = DisplayMode::DeviceAndBattery;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, r#""device_and_battery""#);
    }

    #[test]
    fn a_config_with_percent_in_icon_loads_percent_only() {
        let cfg: Config = serde_json::from_str(r#"{"display_mode":"percent_in_icon"}"#).unwrap();
        assert_eq!(cfg.display_mode, DisplayMode::PercentOnly);
    }

    #[test]
    fn a_config_without_a_palette_loads_catppuccin() {
        let cfg: Config = serde_json::from_str(r#"{"display_mode":"percent_only"}"#).unwrap();
        assert_eq!(cfg.palette, Palette::Catppuccin);
        assert_eq!(cfg.display_mode, DisplayMode::PercentOnly);
    }

    #[test]
    fn a_config_without_a_theme_follows_the_system() {
        let cfg: Config = serde_json::from_str(r#"{"palette":"nord"}"#).unwrap();
        assert_eq!(cfg.theme, WindowTheme::System);
        for (json, theme) in [
            (r#"{"theme":"system"}"#, WindowTheme::System),
            (r#"{"theme":"light"}"#, WindowTheme::Light),
            (r#"{"theme":"dark"}"#, WindowTheme::Dark),
        ] {
            assert_eq!(serde_json::from_str::<Config>(json).unwrap().theme, theme);
        }
    }

    #[test]
    fn partial_json_uses_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.display_mode, DisplayMode::IconOnly);
        assert!(cfg.hidden_devices.is_empty());
        assert_eq!(cfg.tray_mode, TrayMode::PrimaryOnly);
        assert!(cfg.notifications_enabled);
        assert_eq!(cfg.primary_device, None);
        assert_eq!(cfg.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(cfg.low_threshold, DEFAULT_LOW_THRESHOLD);
        assert!(cfg.device_overrides.is_empty());
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
            hidden_devices: vec!["mouse".to_string()],
            ..Config::default()
        };
        assert!(!cfg.is_shown("mouse"));
        assert!(cfg.is_shown("keyboard"));
        // Never mentioned in hidden_devices — shown by default, including a
        // name that is not currently discovered.
        assert!(cfg.is_shown("headset"));
    }

    fn scratch_config(test_name: &str) -> (ScratchDir, PathBuf) {
        let scratch = ScratchDir::new(test_name);
        let path = scratch.join("config.json");
        (scratch, path)
    }

    #[test]
    fn save_then_load_preserves_hidden_devices() {
        let (_scratch, path) = scratch_config("round-trip");
        let cfg = Config {
            hidden_devices: vec!["mouse".to_string(), "headset".to_string()],
            ..Config::default()
        };
        save_to(&path, &cfg).unwrap();
        let restored = load_from(&path);
        assert_eq!(restored.hidden_devices, cfg.hidden_devices);
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let (_scratch, path) = scratch_config("missing");
        assert_eq!(load_from(&path), Config::default());
    }

    #[test]
    fn read_tells_missing_from_broken() {
        let (_scratch, path) = scratch_config("read");
        assert!(read(&path).unwrap().is_none());
        std::fs::write(&path, "{ not json").unwrap();
        let err = format!("{:#}", read(&path).unwrap_err());
        assert!(err.contains("config parse error"), "{err}");
        assert_eq!(load_from(&path), Config::default());
        std::fs::remove_file(&path).unwrap();
        save_to(&path, &Config::default()).unwrap();
        assert_eq!(read(&path).unwrap(), Some(Config::default()));
    }

    #[test]
    fn save_to_refuses_to_write_over_a_file_that_does_not_parse() {
        let (_scratch, path) = scratch_config("broken-save");
        let broken = "{ \"low_threshold\": 30, typo }";
        std::fs::write(&path, broken).unwrap();
        let err = save_to(&path, &Config::default()).unwrap_err();
        assert!(err.is::<Unreadable>(), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn reload_keeps_the_live_config_while_the_file_does_not_parse() {
        let (_scratch, path) = scratch_config("broken-reload");
        let live = Config {
            hidden_devices: vec!["mouse".to_string()],
            low_threshold: 35,
            ..Config::default()
        };
        let (tx, rx) = tokio::sync::watch::channel(live.clone());
        let mut broken = false;

        std::fs::write(&path, "{ not json").unwrap();
        reload(&path, &tx, &mut broken);
        assert_eq!(*rx.borrow(), live);
        assert!(broken);

        let fixed = Config {
            low_threshold: 10,
            ..live.clone()
        };
        std::fs::write(&path, serde_json::to_string(&fixed).unwrap()).unwrap();
        reload(&path, &tx, &mut broken);
        assert_eq!(*rx.borrow(), fixed);
        assert!(!broken);
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

    /// `path` is an existing directory, so the rename after the temp file is
    /// written fails with EISDIR.
    #[test]
    fn save_to_cleans_up_temp_file_on_rename_failure() {
        let scratch = ScratchDir::new("save-failure");
        let dir = scratch.path();
        let path = dir.join("config.json");
        std::fs::create_dir_all(&path).unwrap();

        assert!(save_to(&path, &Config::default()).is_err());

        let leftover: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "config.json")
            .collect();
        assert!(leftover.is_empty(), "temp file left behind: {leftover:?}");
    }

    /// `load_from` must not convert a legacy `shown_devices` whitelist — it
    /// has no device roster to convert against. `app::supervisor` performs
    /// that conversion, once, after its first discovery sweep.
    #[test]
    fn load_from_does_not_migrate_legacy_shown_devices() {
        let (_scratch, path) = scratch_config("no-migration-on-load");
        std::fs::write(&path, r#"{"shown_devices": ["mouse"]}"#).unwrap();
        let cfg = load_from(&path);
        assert_eq!(cfg.shown_devices, vec!["mouse".to_string()]);
        assert!(cfg.hidden_devices.is_empty());
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
    fn is_content_change_keeps_only_content_events() {
        use notify::EventKind;
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RenameMode,
        };
        let cases = [
            (EventKind::Access(AccessKind::Read), false),
            (EventKind::Access(AccessKind::Open(AccessMode::Any)), false),
            (EventKind::Modify(ModifyKind::Data(DataChange::Any)), true),
            (EventKind::Modify(ModifyKind::Name(RenameMode::To)), true),
            (EventKind::Create(CreateKind::File), true),
            (
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
                false,
            ),
        ];
        for (kind, expected) in cases {
            assert_eq!(is_content_change(&kind), expected, "{kind:?}");
        }
    }

    #[test]
    fn event_touches_only_the_config_file() {
        let target = PathBuf::from("/home/user/.config/rigbat/config.json");
        let event_at = |path: &str| notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from(path)],
            attrs: Default::default(),
        };
        for (path, expected) in [
            ("/home/user/.config/rigbat/config.json", true),
            ("/home/user/.config/rigbat/other.json", false),
            ("/home/user/.config/rigbat/config.json.tmp", false),
        ] {
            assert_eq!(event_touches(&event_at(path), &target), expected, "{path}");
        }
    }

    fn mouse_override(poll_interval_secs: Option<u64>, low_threshold: Option<u8>) -> Config {
        let mut cfg = Config::default();
        cfg.device_overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs,
                low_threshold,
            },
        );
        cfg
    }

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
        let cfg = mouse_override(Some(30), None);
        assert_eq!(cfg.effective_poll_interval_secs("mouse"), 30);
        // Other devices still use global.
        assert_eq!(
            cfg.effective_poll_interval_secs("keyboard"),
            DEFAULT_POLL_INTERVAL_SECS
        );
    }

    #[test]
    fn effective_poll_interval_secs_zero_clamped_to_one() {
        let cfg = mouse_override(Some(0), None);
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

    #[test]
    fn effective_low_threshold_global_default_when_no_override() {
        let cfg = Config::default();
        assert_eq!(cfg.effective_low_threshold("mouse"), DEFAULT_LOW_THRESHOLD);
    }

    #[test]
    fn effective_low_threshold_override_wins() {
        let cfg = mouse_override(None, Some(10));
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
        let cfg = Config {
            low_threshold: 250,
            ..mouse_override(None, Some(255))
        };
        assert_eq!(cfg.effective_low_threshold("mouse"), 100);
        assert_eq!(cfg.effective_low_threshold("keyboard"), 100);
    }

    #[test]
    fn serde_round_trip_with_device_overrides() {
        let cfg = Config {
            poll_interval_secs: 30,
            low_threshold: 15,
            ..mouse_override(Some(10), Some(5))
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, restored);
    }

    #[test]
    fn an_offline_device_hides_after_two_hours_by_default() {
        let old: Config = serde_json::from_str(r#"{"poll_interval_secs":60}"#).unwrap();
        for cfg in [Config::default(), old] {
            assert_eq!(cfg.hide_offline_after_mins, 120);
            assert_eq!(cfg.hide_offline_after(), Duration::from_secs(2 * 3600));
        }
    }

    #[test]
    fn hide_offline_after_survives_a_round_trip() {
        let (_scratch, path) = scratch_config("hide-offline-after");
        let cfg = Config {
            hide_offline_after_mins: 360,
            ..Config::default()
        };
        save_to(&path, &cfg).unwrap();
        assert_eq!(load_from(&path), cfg);
    }

    #[test]
    fn hide_offline_after_clamps_a_hand_edited_value() {
        let at = |mins| {
            Config {
                hide_offline_after_mins: mins,
                ..Config::default()
            }
            .hide_offline_after()
        };
        assert_eq!(at(0), OFFLINE_SHELF_LIFE_MIN);
        assert_eq!(at(u32::MAX), OFFLINE_SHELF_LIFE_MAX);
        assert_eq!(at(45), Duration::from_secs(45 * 60));
    }
}
