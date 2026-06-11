use std::collections::HashMap;
use std::sync::Arc;

use ksni::{MenuItem, ToolTip, Tray, TrayMethods};
use tokio::sync::{Notify, watch};

use crate::app::supervisor::TrayState;
use crate::appearance::ColorScheme;
use crate::config::{Config, TrayMode};
use crate::domain::{
    BatteryReading, DeviceInfo, DeviceKind, PrimaryStatus, classify, freedesktop_icon_name,
};
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

/// Returns the device the single (aggregate) icon represents.
///
/// Priority: explicit user choice (if still shown) → first connected shown
/// device → first shown device → None.
fn featured_name(state: &crate::app::supervisor::TrayState, cfg: &Config) -> Option<String> {
    let shown: Vec<&(DeviceInfo, Option<BatteryReading>)> = state
        .devices
        .iter()
        .filter(|(i, _)| cfg.is_shown(&i.name))
        .collect();

    if let Some(name) = &cfg.primary_device
        && shown.iter().any(|(i, _)| &i.name == name)
    {
        return Some(name.clone());
    }

    shown
        .iter()
        .find(|(_, r)| r.is_some())
        .or_else(|| shown.first())
        .map(|(i, _)| i.name.clone())
}

// ---------------------------------------------------------------------------
// RigbatTray — unified SNI item for both aggregate and per-device icons
// ---------------------------------------------------------------------------

pub struct RigbatTray {
    /// `Some(name)` = per-device icon; `None` = aggregate/primary icon.
    pub key: Option<String>,
    pub rx: watch::Receiver<TrayState>,
    pub theme_rx: watch::Receiver<ColorScheme>,
    pub config: watch::Receiver<Config>,
    pub renderer: Box<dyn IconRenderer>,
    pub refresh: Arc<Notify>,
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

        let (status, kind): (PrimaryStatus, Option<DeviceKind>) = match &self.key {
            Some(name) => {
                // Build owned data, drop the borrows before any await.
                let state = self.rx.borrow();
                let cfg = self.config.borrow();
                let found = state
                    .devices
                    .iter()
                    .find(|(info, _)| &info.name == name)
                    .map(|(info, reading)| {
                        (
                            classify(*reading, cfg.effective_low_threshold(name)),
                            info.kind,
                        )
                    });
                match found {
                    Some((s, k)) => (s, Some(k)),
                    None => (PrimaryStatus::Offline, None),
                }
            }
            None => {
                let state = self.rx.borrow();
                let cfg = self.config.borrow();
                let featured = featured_name(&state, &cfg);
                match featured.and_then(|n| {
                    state
                        .devices
                        .iter()
                        .find(|(i, _)| i.name == n)
                        .map(|(info, reading)| {
                            (
                                classify(*reading, cfg.effective_low_threshold(&info.name)),
                                info.kind,
                            )
                        })
                }) {
                    Some((s, k)) => (s, Some(k)),
                    None => (PrimaryStatus::Offline, None),
                }
            }
        };
        self.renderer.render(status, kind, &theme, mode)
    }

    fn tool_tip(&self) -> ToolTip {
        let title = match &self.key {
            Some(name) => {
                let state = self.rx.borrow();
                state
                    .devices
                    .iter()
                    .find(|(info, _)| &info.name == name)
                    .map(|(info, reading)| format_device_entry(info, *reading))
                    .unwrap_or_else(|| format!("{}: offline", name))
            }
            None => {
                let state = self.rx.borrow();
                let cfg = self.config.borrow();
                featured_name(&state, &cfg)
                    .and_then(|n| state.devices.iter().find(|(i, _)| i.name == n))
                    .map(|(info, reading)| format_device_entry(info, *reading))
                    .unwrap_or_else(|| "No devices".into())
            }
        };
        ToolTip {
            title,
            ..ToolTip::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        // Build the full-roster menu shared by both PrimaryOnly and PerDevice modes.
        // Collect all data from borrows before building menu items (borrows are sync,
        // no await here, but keeping scopes tight documents intent).
        let rows = {
            let state = self.rx.borrow();
            let cfg = self.config.borrow();

            // Determine which device name to mark with the bullet.
            let highlight: Option<String> = match &self.key {
                Some(k) => Some(k.clone()),
                None => featured_name(&state, &cfg),
            };

            // Collect (dev_name, label, icon_name) for each shown device.
            let rows: Vec<(String, String, String)> = state
                .devices
                .iter()
                .filter(|(info, _)| cfg.is_shown(&info.name))
                .map(|(info, reading)| {
                    let prefix = if highlight.as_deref() == Some(info.name.as_str()) {
                        "\u{25cf} " // "● "
                    } else {
                        "  "
                    };
                    let entry = format_device_entry(info, *reading);
                    let label = format!("{prefix}{entry}");
                    let icon = freedesktop_icon_name(info.kind).to_owned();
                    (info.name.clone(), label, icon)
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
                            eprintln!("rigbat: failed to save tray device selection: {e}");
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
            activate: Box::new(|app: &mut Self| app.refresh.notify_waiters()),
            ..ksni::menu::StandardItem::default()
        }));

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: "Settings\u{2026}".into(),
            activate: Box::new(|_: &mut Self| match std::env::current_exe() {
                Ok(exe) => {
                    if let Err(e) = std::process::Command::new(exe).arg("settings").spawn() {
                        eprintln!("rigbat: failed to launch settings window: {e}");
                    }
                }
                Err(e) => eprintln!("rigbat: cannot find own executable: {e}"),
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
    refresh: &Arc<Notify>,
) {
    // Compute desired key list without holding any watch::Ref across an await.
    let desired: Vec<Option<String>> = {
        let state = rx.borrow();
        let cfg = config_rx.borrow();
        let mut shown: Vec<String> = Vec::new();
        for (info, _) in &state.devices {
            if cfg.is_shown(&info.name) && !shown.contains(&info.name) {
                shown.push(info.name.clone());
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
                    eprintln!("rigbat: failed to spawn tray icon for {key:?}: {e}");
                }
            }
        }
    }

    // Re-render surviving icons so fresh data is reflected.
    for handle in items.values() {
        let _ = handle.update(|_| {}).await;
    }

    if items.len() != prev_len {
        eprintln!("rigbat: showing {} tray icon(s)", items.len());
    }
}

// ---------------------------------------------------------------------------
// run — the manager loop, called from run_tray
// ---------------------------------------------------------------------------

pub async fn run(
    mut rx: watch::Receiver<TrayState>,
    mut theme_rx: watch::Receiver<ColorScheme>,
    mut config_rx: watch::Receiver<Config>,
    refresh: Arc<Notify>,
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
    use super::{desired_keys, featured_name, sanitize};
    use crate::app::supervisor::TrayState;
    use crate::config::{Config, TrayMode};
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

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

    fn make_state(devices: Vec<(DeviceInfo, Option<BatteryReading>)>) -> TrayState {
        use crate::domain::PrimaryStatus;
        TrayState {
            primary: if devices.is_empty() { None } else { Some(0) },
            primary_status: PrimaryStatus::Offline,
            devices,
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
}
