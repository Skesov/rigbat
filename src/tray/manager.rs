use std::collections::HashMap;
use std::time::Instant;

use ksni::{MenuItem, ToolTip, Tray, TrayMethods};
use tokio::sync::watch;

use crate::app::refresh::RefreshSignal;
use crate::app::supervisor::TrayState;
use crate::appearance::ColorScheme;
use crate::config::{Config, TrayMode};
use crate::domain::{DeviceState, Presence, PrimaryStatus, classify, freedesktop_icon_name};
use crate::tray::format_device_entry;
use crate::tray::icon::{IconRenderer, Theme, TinySkiaRenderer};

// ---------------------------------------------------------------------------
// sanitize: map non-ASCII-alphanumeric chars to '-' for stable SNI ids
// ---------------------------------------------------------------------------

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// ---------------------------------------------------------------------------
// desired_keys: compute the set of icon keys from mode and shown device names
// ---------------------------------------------------------------------------

/// Returns the list of tray icon keys for the given mode and shown device names.
/// `None` is the aggregate/primary icon; `Some(name)` is a per-device icon.
pub fn desired_keys(mode: TrayMode, shown: &[String]) -> Vec<Option<String>> {
    match (mode, shown.is_empty()) {
        // No devices → always one "No devices" aggregate icon.
        (_, true) => vec![None],
        // One aggregate icon.
        (TrayMode::PrimaryOnly, _) => vec![None],
        // One icon per shown device.
        (TrayMode::PerDevice, _) => shown.iter().map(|n| Some(n.clone())).collect(),
    }
}

// ---------------------------------------------------------------------------
// featured_name — the device the aggregate icon represents
// ---------------------------------------------------------------------------

/// Picks the featured device by name from an already-shown-filtered roster.
///
/// `shown` is `(name, online)` pairs; `online` is the tiebreak signal used
/// when there is no explicit choice — `Presence::Online` for the tray, but
/// callers with no presence information (the one-shot CLI path) can pass any
/// other "prefer this one" signal, such as "this poll returned a reading".
///
/// Priority: explicit choice (if still shown) → first online → first shown → None.
/// Extracted from `featured_name` so the `--waybar` CLI mode can reuse the
/// exact selection the aggregate tray icon uses, instead of reimplementing it
/// against a `Row` shape that carries no `DeviceState`/`Presence`.
pub fn select_featured(shown: &[(&str, bool)], primary_device: Option<&str>) -> Option<String> {
    if let Some(name) = primary_device
        && shown.iter().any(|(n, _)| *n == name)
    {
        return Some(name.to_string());
    }

    shown
        .iter()
        .find(|(_, online)| *online)
        .or_else(|| shown.first())
        .map(|(name, _)| name.to_string())
}

/// Returns the device the single (aggregate) icon represents.
///
/// Priority: explicit user choice (if still shown) → first connected shown
/// device → first shown device → None.
fn featured_name(state: &crate::app::supervisor::TrayState, cfg: &Config) -> Option<String> {
    let shown: Vec<(&str, bool)> = state
        .devices
        .iter()
        .filter(|d| cfg.is_shown(&d.info.name))
        .map(|d| (d.info.name.as_str(), d.presence == Presence::Online))
        .collect();

    select_featured(&shown, cfg.primary_device.as_deref())
}

// ---------------------------------------------------------------------------
// resolve — the single answer to "which device does this icon represent?"
// ---------------------------------------------------------------------------

/// The device a tray icon stands for, resolved against the current state and config.
struct Resolved {
    state: DeviceState,
    status: PrimaryStatus,
}

/// Resolves an icon's device from its key, the current tray state and config.
///
/// Per-device icon (`key = Some(name)`): that device, if present and shown.
/// A device excluded by `shown_devices` resolves to `None` even if `key`
/// still names it: `desired_keys` only creates per-device icons from the
/// shown list, so a keyed-but-hidden state is transient (config changed,
/// reconcile has not yet retired the icon) and showing it for one frame
/// would be the bug.
/// Aggregate icon (`key = None`): `featured_name`'s pick.
///
/// `classify` only knows readings, not reachability, so `Unreachable`/
/// `Disconnected` are mapped to `PrimaryStatus::Offline` here rather than
/// teaching `classify` about presence.
fn resolve_for(key: Option<&str>, state: &TrayState, cfg: &Config) -> Option<Resolved> {
    let name = match key {
        Some(name) => {
            if cfg.is_shown(name) {
                name.to_string()
            } else {
                return None;
            }
        }
        None => featured_name(state, cfg)?,
    };
    let device = state.devices.iter().find(|d| d.info.name == name)?;
    let status = if device.presence == Presence::Online {
        classify(
            device.last_reading,
            cfg.effective_low_threshold(&device.info.name),
        )
    } else {
        PrimaryStatus::Offline
    };
    Some(Resolved {
        state: device.clone(),
        status,
    })
}

// ---------------------------------------------------------------------------
// RigbatTray — unified SNI item for both aggregate and per-device icons
// ---------------------------------------------------------------------------

const MENU_HINT: &str = "Click a device to feature it";

pub struct RigbatTray {
    /// `Some(name)` = per-device icon; `None` = aggregate/primary icon.
    pub key: Option<String>,
    pub rx: watch::Receiver<TrayState>,
    pub theme_rx: watch::Receiver<ColorScheme>,
    pub config: watch::Receiver<Config>,
    pub renderer: Box<dyn IconRenderer>,
    pub refresh: RefreshSignal,
}

impl RigbatTray {
    /// Resolves this icon's device. `None` when the icon has nothing to show
    /// (no devices, or the keyed device disappeared / is hidden).
    fn resolve(&self) -> Option<Resolved> {
        let state = self.rx.borrow();
        let cfg = self.config.borrow();
        resolve_for(self.key.as_deref(), &state, &cfg)
        // `state` and `cfg` (watch::Ref) are dropped here, before any await.
    }
}

impl Tray for RigbatTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        match &self.key {
            Some(k) => format!("rigbat-{}", sanitize(k)),
            None => "rigbat".into(),
        }
    }

    fn title(&self) -> String {
        match &self.key {
            Some(k) => k.clone(),
            None => "rigbat".into(),
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let theme = match *self.theme_rx.borrow() {
            ColorScheme::Dark => Theme::dark(),
            ColorScheme::Light => Theme::light(),
        };
        let mode = self.config.borrow().display_mode;

        let resolved = self.resolve();
        let status = resolved
            .as_ref()
            .map_or(PrimaryStatus::Offline, |r| r.status);
        let kind = resolved.map(|r| r.state.info.kind);
        self.renderer.render(status, kind, &theme, mode)
    }

    fn tool_tip(&self) -> ToolTip {
        let title = self
            .resolve()
            .map(|r| format_device_entry(&r.state, Instant::now()))
            .unwrap_or_else(|| match &self.key {
                Some(name) => format!("{name}: offline"),
                None => "No devices".into(),
            });
        ToolTip {
            title,
            ..ToolTip::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        // Build the full-roster menu shared by both PrimaryOnly and PerDevice modes.
        // Collect all data from borrows before building menu items (borrows are sync,
        // no await here, but keeping scopes tight documents intent).
        // Determine which device name to mark with the bullet.
        let highlight: Option<String> = self.resolve().map(|r| r.state.info.name);
        let now = Instant::now();

        let rows = {
            let state = self.rx.borrow();
            let cfg = self.config.borrow();

            // Collect (dev_name, label, icon_name) for each shown device.
            let rows: Vec<(String, String, String)> = state
                .devices
                .iter()
                .filter(|d| cfg.is_shown(&d.info.name))
                .map(|d| {
                    let prefix = if highlight.as_deref() == Some(d.info.name.as_str()) {
                        "\u{25cf} " // "● "
                    } else {
                        "  "
                    };
                    let entry = format_device_entry(d, now);
                    let label = format!("{prefix}{entry}");
                    let icon = freedesktop_icon_name(d.info.kind).to_owned();
                    (d.info.name.clone(), label, icon)
                })
                .collect();

            rows
        };
        // All watch borrows are released here.

        let mut items: Vec<MenuItem<Self>> = Vec::new();

        if rows.is_empty() {
            items.push(MenuItem::Standard(ksni::menu::StandardItem {
                label: "No devices".into(),
                enabled: false,
                ..ksni::menu::StandardItem::default()
            }));
        } else {
            items.push(MenuItem::Standard(ksni::menu::StandardItem {
                label: MENU_HINT.into(),
                enabled: false,
                ..ksni::menu::StandardItem::default()
            }));
            for (dev_name, label, icon_name) in rows {
                // Read-modify-write: load the latest on-disk config and change only
                // primary_device, so other fields set by the settings window are not
                // clobbered. Concurrent edits while the settings window is open can
                // cause a lost-update on primary_device — rare and low-severity.
                let name = dev_name.clone();
                items.push(MenuItem::Standard(ksni::menu::StandardItem {
                    label,
                    icon_name,
                    enabled: true,
                    activate: Box::new(move |_: &mut Self| {
                        let mut cfg = crate::config::load();
                        cfg.primary_device = if cfg.primary_device.as_deref() == Some(name.as_str())
                        {
                            // Clicking the featured device returns to automatic.
                            None
                        } else {
                            Some(name.clone())
                        };
                        if let Err(e) = crate::config::save(&cfg) {
                            tracing::error!("failed to save tray device selection: {e}");
                        }
                    }),
                    ..ksni::menu::StandardItem::default()
                }));
            }
        }

        items.push(MenuItem::Separator);

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: "Refresh".into(),
            icon_name: "view-refresh".into(),
            activate: Box::new(|app: &mut Self| app.refresh.trigger()),
            ..ksni::menu::StandardItem::default()
        }));

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: "Settings\u{2026}".into(),
            activate: Box::new(|_: &mut Self| match std::env::current_exe() {
                Ok(exe) => {
                    if let Err(e) = std::process::Command::new(exe).arg("settings").spawn() {
                        tracing::error!("failed to launch settings window: {e}");
                    }
                }
                Err(e) => tracing::error!("cannot find own executable: {e}"),
            }),
            ..ksni::menu::StandardItem::default()
        }));

        items.push(MenuItem::Separator);

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: "Quit".into(),
            activate: Box::new(|_| std::process::exit(0)),
            ..ksni::menu::StandardItem::default()
        }));

        items
    }
}

// ---------------------------------------------------------------------------
// reconcile — sync live SNI items against the desired icon set
// ---------------------------------------------------------------------------

async fn reconcile(
    items: &mut HashMap<Option<String>, ksni::Handle<RigbatTray>>,
    rx: &watch::Receiver<TrayState>,
    theme_rx: &watch::Receiver<ColorScheme>,
    config_rx: &watch::Receiver<Config>,
    refresh: &RefreshSignal,
) {
    // Compute desired key list without holding any watch::Ref across an await.
    let desired: Vec<Option<String>> = {
        let state = rx.borrow();
        let cfg = config_rx.borrow();
        let mut shown: Vec<String> = Vec::new();
        for d in &state.devices {
            if cfg.is_shown(&d.info.name) && !shown.contains(&d.info.name) {
                shown.push(d.info.name.clone());
            }
        }
        desired_keys(cfg.tray_mode, &shown)
    }; // borrows dropped here

    let prev_len = items.len();

    // Retire icons whose keys are no longer desired.
    let to_remove: Vec<Option<String>> = items
        .keys()
        .filter(|k| !desired.contains(k))
        .cloned()
        .collect();
    for key in to_remove {
        if let Some(handle) = items.remove(&key) {
            // ksni 0.3.4 keeps the SNI item alive on drop; shutdown() unregisters it.
            handle.shutdown().await;
        }
    }

    // Spawn new icons for keys that just became desired.
    for key in &desired {
        if !items.contains_key(key) {
            let tray = RigbatTray {
                key: key.clone(),
                rx: rx.clone(),
                theme_rx: theme_rx.clone(),
                config: config_rx.clone(),
                renderer: Box::new(TinySkiaRenderer::default()),
                refresh: refresh.clone(),
            };
            match tray.spawn().await {
                Ok(handle) => {
                    items.insert(key.clone(), handle);
                }
                Err(e) => {
                    tracing::error!("failed to spawn tray icon for {key:?}: {e}");
                }
            }
        }
    }

    // Re-render surviving icons so fresh data is reflected.
    for handle in items.values() {
        let _ = handle.update(|_| {}).await;
    }

    if items.len() != prev_len {
        tracing::info!("showing {} tray icon(s)", items.len());
    }
}

// ---------------------------------------------------------------------------
// run — the manager loop, called from run_tray
// ---------------------------------------------------------------------------

pub async fn run(
    mut rx: watch::Receiver<TrayState>,
    mut theme_rx: watch::Receiver<ColorScheme>,
    mut config_rx: watch::Receiver<Config>,
    refresh: RefreshSignal,
) {
    let mut items: HashMap<Option<String>, ksni::Handle<RigbatTray>> = HashMap::new();

    loop {
        reconcile(&mut items, &rx, &theme_rx, &config_rx, &refresh).await;

        tokio::select! {
            r = rx.changed() => if r.is_err() { break; },
            r = theme_rx.changed() => if r.is_err() { break; },
            r = config_rx.changed() => if r.is_err() { break; },
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{MENU_HINT, desired_keys, featured_name, resolve_for, sanitize, select_featured};
    use crate::app::supervisor::TrayState;
    use crate::config::{Config, TrayMode};
    use crate::domain::{
        BatteryReading, ChargeState, DeviceInfo, DeviceKind, DeviceState, Presence, PrimaryStatus,
    };

    // --- menu hint ------------------------------------------------------------

    #[test]
    fn menu_hint_text_is_exact() {
        assert_eq!(MENU_HINT, "Click a device to feature it");
    }

    // --- sanitize -----------------------------------------------------------

    #[test]
    fn sanitize_alphanumeric_unchanged() {
        assert_eq!(sanitize("mouse123"), "mouse123");
    }

    #[test]
    fn sanitize_spaces_become_dash() {
        assert_eq!(sanitize("MX Master 3"), "MX-Master-3");
    }

    #[test]
    fn sanitize_punctuation_becomes_dash() {
        assert_eq!(sanitize("kbd/bt#1"), "kbd-bt-1");
    }

    #[test]
    fn sanitize_empty_string() {
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn sanitize_non_ascii_becomes_dash() {
        assert_eq!(sanitize("héadset"), "h-adset");
    }

    // --- desired_keys -------------------------------------------------------

    #[test]
    fn desired_keys_primary_only_returns_one_none() {
        let shown = vec!["mouse".to_string(), "keyboard".to_string()];
        assert_eq!(desired_keys(TrayMode::PrimaryOnly, &shown), vec![None]);
    }

    #[test]
    fn desired_keys_per_device_returns_some_per_name() {
        let shown = vec!["mouse".to_string(), "keyboard".to_string()];
        let result = desired_keys(TrayMode::PerDevice, &shown);
        assert_eq!(
            result,
            vec![Some("mouse".to_string()), Some("keyboard".to_string()),]
        );
    }

    #[test]
    fn desired_keys_empty_shown_returns_one_none_regardless_of_mode() {
        assert_eq!(desired_keys(TrayMode::PrimaryOnly, &[]), vec![None]);
        assert_eq!(desired_keys(TrayMode::PerDevice, &[]), vec![None]);
    }

    // --- select_featured ------------------------------------------------------

    #[test]
    fn select_featured_explicit_choice_wins_when_shown() {
        let shown = [("mouse", true), ("keyboard", false)];
        assert_eq!(
            select_featured(&shown, Some("keyboard")),
            Some("keyboard".to_string())
        );
    }

    #[test]
    fn select_featured_explicit_choice_ignored_when_not_shown() {
        let shown = [("mouse", true), ("keyboard", false)];
        assert_eq!(
            select_featured(&shown, Some("gamepad")),
            Some("mouse".to_string())
        );
    }

    #[test]
    fn select_featured_no_choice_prefers_online() {
        let shown = [("mouse", false), ("keyboard", true)];
        assert_eq!(select_featured(&shown, None), Some("keyboard".to_string()));
    }

    #[test]
    fn select_featured_no_online_falls_back_to_first() {
        let shown = [("mouse", false), ("keyboard", false)];
        assert_eq!(select_featured(&shown, None), Some("mouse".to_string()));
    }

    #[test]
    fn select_featured_empty_returns_none() {
        assert_eq!(select_featured(&[], None), None);
    }

    // --- featured_name -------------------------------------------------------

    fn make_info(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            kind: DeviceKind::Mouse,
            transport: crate::domain::Transport::Sysfs,
            locator: None,
        }
    }

    fn make_reading(percent: u8) -> BatteryReading {
        BatteryReading::new(percent, ChargeState::Discharging)
    }

    /// Builds a `TrayState` from (info, reading) pairs: `Some` reading means
    /// `Online`, `None` means never seen (`Unreachable`, nothing retained) —
    /// matching what these tests exercised before presence existed.
    fn make_state(devices: Vec<(DeviceInfo, Option<BatteryReading>)>) -> TrayState {
        TrayState {
            devices: devices
                .into_iter()
                .map(|(info, reading)| DeviceState {
                    info,
                    last_reading: reading,
                    last_seen: reading.map(|_| Instant::now()),
                    presence: if reading.is_some() {
                        Presence::Online
                    } else {
                        Presence::Unreachable
                    },
                    estimate: crate::domain::Estimate::Unknown,
                })
                .collect(),
        }
    }

    fn cfg_with_primary(primary: Option<&str>) -> Config {
        Config {
            primary_device: primary.map(|s| s.to_string()),
            ..Config::default()
        }
    }

    #[test]
    fn featured_name_explicit_shown_returns_that_name() {
        let state = make_state(vec![
            (make_info("mouse"), Some(make_reading(80))),
            (make_info("keyboard"), Some(make_reading(50))),
        ]);
        let cfg = cfg_with_primary(Some("keyboard"));
        assert_eq!(featured_name(&state, &cfg), Some("keyboard".to_string()));
    }

    #[test]
    fn featured_name_explicit_hidden_falls_back_to_first_connected() {
        // "gamepad" is not in shown_devices, so the explicit choice is ignored.
        let mut cfg = cfg_with_primary(Some("gamepad"));
        cfg.shown_devices = vec!["mouse".to_string(), "keyboard".to_string()];
        let state = make_state(vec![
            (make_info("mouse"), Some(make_reading(80))),
            (make_info("keyboard"), Some(make_reading(50))),
            (make_info("gamepad"), Some(make_reading(30))),
        ]);
        // "gamepad" is not shown, so falls back to first connected shown: "mouse"
        assert_eq!(featured_name(&state, &cfg), Some("mouse".to_string()));
    }

    #[test]
    fn featured_name_no_explicit_returns_first_connected_shown() {
        let state = make_state(vec![
            (make_info("mouse"), None),
            (make_info("keyboard"), Some(make_reading(60))),
        ]);
        let cfg = cfg_with_primary(None);
        assert_eq!(featured_name(&state, &cfg), Some("keyboard".to_string()));
    }

    #[test]
    fn featured_name_no_connected_returns_first_shown() {
        let state = make_state(vec![
            (make_info("mouse"), None),
            (make_info("keyboard"), None),
        ]);
        let cfg = cfg_with_primary(None);
        // No connected device; falls back to first shown.
        assert_eq!(featured_name(&state, &cfg), Some("mouse".to_string()));
    }

    #[test]
    fn featured_name_no_devices_returns_none() {
        let state = make_state(vec![]);
        let cfg = cfg_with_primary(None);
        assert_eq!(featured_name(&state, &cfg), None);
    }

    #[test]
    fn featured_name_all_filtered_by_shown_returns_none() {
        let mut cfg = cfg_with_primary(None);
        cfg.shown_devices = vec!["trackpad".to_string()];
        let state = make_state(vec![
            (make_info("mouse"), Some(make_reading(80))),
            (make_info("keyboard"), Some(make_reading(50))),
        ]);
        // None of the devices match the whitelist.
        assert_eq!(featured_name(&state, &cfg), None);
    }

    // --- resolve_for ----------------------------------------------------------

    #[test]
    fn resolve_for_per_device_key_present_and_shown() {
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(80)))]);
        let cfg = cfg_with_primary(None);
        let resolved = resolve_for(Some("mouse"), &state, &cfg).unwrap();
        assert_eq!(resolved.state.info.name, "mouse");
        assert_eq!(resolved.state.last_reading, Some(make_reading(80)));
    }

    #[test]
    fn resolve_for_per_device_key_hidden_by_shown_devices_returns_none() {
        let mut cfg = cfg_with_primary(None);
        cfg.shown_devices = vec!["keyboard".to_string()];
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(80)))]);
        assert!(resolve_for(Some("mouse"), &state, &cfg).is_none());
    }

    #[test]
    fn resolve_for_per_device_key_absent_from_state_returns_none() {
        let state = make_state(vec![(make_info("keyboard"), Some(make_reading(50)))]);
        let cfg = cfg_with_primary(None);
        assert!(resolve_for(Some("mouse"), &state, &cfg).is_none());
    }

    #[test]
    fn resolve_for_aggregate_key_uses_explicit_primary_device() {
        let state = make_state(vec![
            (make_info("mouse"), Some(make_reading(80))),
            (make_info("keyboard"), Some(make_reading(50))),
        ]);
        let cfg = cfg_with_primary(Some("keyboard"));
        let resolved = resolve_for(None, &state, &cfg).unwrap();
        assert_eq!(resolved.state.info.name, "keyboard");
    }

    #[test]
    fn resolve_for_aggregate_key_no_devices_returns_none() {
        let state = make_state(vec![]);
        let cfg = cfg_with_primary(None);
        assert!(resolve_for(None, &state, &cfg).is_none());
    }

    #[test]
    fn resolve_for_status_uses_per_device_low_threshold_override() {
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(15)))]);
        let mut cfg = cfg_with_primary(None);

        // Global threshold (20) would already flag 15% as Low; override it down
        // so the global default alone would report Ok, isolating the override.
        cfg.low_threshold = 5;
        let resolved = resolve_for(Some("mouse"), &state, &cfg).unwrap();
        assert!(matches!(resolved.status, PrimaryStatus::Ok { .. }));

        cfg.device_overrides.insert(
            "mouse".to_string(),
            crate::config::DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(20),
            },
        );
        let resolved = resolve_for(Some("mouse"), &state, &cfg).unwrap();
        assert!(matches!(resolved.status, PrimaryStatus::Low { .. }));
    }

    #[test]
    fn resolve_for_unreachable_device_reports_offline_despite_retained_low_reading() {
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![DeviceState {
                info: make_info("mouse"),
                last_reading: Some(make_reading(5)), // would classify as Low
                last_seen: Some(Instant::now()),
                presence: Presence::Unreachable,
                estimate: crate::domain::Estimate::Unknown,
            }],
        };
        let resolved = resolve_for(Some("mouse"), &state, &cfg).unwrap();
        assert_eq!(resolved.status, PrimaryStatus::Offline);
    }
}
