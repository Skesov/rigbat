use std::collections::HashMap;
use std::time::{Duration, Instant};

use ksni::{MenuItem, ToolTip, Tray, TrayMethods};
use tokio::sync::watch;

use crate::app::refresh::RefreshSignal;
use crate::app::supervisor::TrayState;
use crate::appearance::ColorScheme;
use crate::config::{Config, TrayMode};
use crate::domain::{
    DeviceState, Presence, PrimaryStatus, classify, classify_stale, format_device_entry,
    freedesktop_icon_name, select_featured,
};
use crate::tray::icon::{IconRenderer, Theme, TinySkiaRenderer};

// ---------------------------------------------------------------------------
// sanitize: map non-ASCII-alphanumeric chars to '-' for stable SNI ids
// ---------------------------------------------------------------------------

/// Builds the SNI item id for a device name.
///
/// The readable stem keeps the id recognisable in a bus listing, but on its
/// own it collides: every character outside `[A-Za-z0-9]` becomes `-`, so
/// "Foo Bar" and "Foo-Bar" produce one stem, and a name in a non-Latin script
/// produces nothing but dashes. Hosts key per-item state such as remembered
/// position on this id, so a suffix derived from the full name keeps distinct
/// devices distinct whatever characters they use.
fn sanitize(s: &str) -> String {
    let stem: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{stem}-{:08x}", name_hash(s))
}

/// FNV-1a over the original name. Not cryptographic — it only has to separate
/// names a host would otherwise see as one.
fn name_hash(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// How long a device that is not `Online` keeps its tray presence after its
/// last reading.
///
/// Matches the supervisor's `DISCONNECTED_RETENTION` deliberately: the roster
/// and the tray forget a silent device on the same schedule, so an icon never
/// outlives the entry behind it. A day is long enough that a peripheral left
/// off overnight is where the user left it, and short enough that a mouse
/// unused for a week is not still claiming a slot.
const RETAINED_ICON_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether a device should appear in the tray at all: not hidden by the user,
/// and still saying something (`DeviceState::is_currently_informative`).
///
/// Every place that turns `TrayState` into tray output goes through this, so
/// the icon list, the menu roster and the aggregate icon's pick can never
/// disagree about which devices exist.
fn tray_visible(device: &DeviceState, cfg: &Config, now: Instant) -> bool {
    cfg.is_shown(&device.info.name) && device.is_currently_informative(now, RETAINED_ICON_MAX_AGE)
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
fn featured_name(
    state: &crate::app::supervisor::TrayState,
    cfg: &Config,
    now: Instant,
) -> Option<String> {
    let shown: Vec<(&str, bool)> = state
        .devices
        .iter()
        .filter(|d| tray_visible(d, cfg, now))
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
    /// `true` when `status` classifies a retained reading from a device that
    /// is not currently `Online` — the icon should render dimmed.
    stale: bool,
}

/// Resolves an icon's device from its key, the current tray state and config.
///
/// Per-device icon (`key = Some(name)`): that device, if present and shown.
/// A device named in `hidden_devices` resolves to `None` even if `key` still
/// names it: `desired_keys` only creates per-device icons from the shown
/// list, so a keyed-but-hidden state is transient (config changed, reconcile
/// has not yet retired the icon) and showing it for one frame would be the
/// bug.
/// Aggregate icon (`key = None`): `featured_name`'s pick.
///
/// `classify` only knows readings, not reachability. A device that is not
/// `Online` still shows its retained percentage (marked `stale`) as long as
/// one exists — Bluetooth peripherals sleep constantly, and a device whose
/// last-known charge is 88% should not flash "offline" just because it is
/// asleep. Only a device with no reading at all falls back to
/// `PrimaryStatus::Offline`.
///
/// The retained charge state does not survive the presence drop, though: a
/// stored `ChargeState::Charging` describes a live condition that is no
/// longer known to be true, and (worse) it outranks `Low` in `classify`'s
/// priority, hiding a low battery behind a stale green icon. So a stale
/// reading is classified by `classify_stale`, which looks only at the
/// percentage.
fn resolve_for(
    key: Option<&str>,
    state: &TrayState,
    cfg: &Config,
    now: Instant,
) -> Option<Resolved> {
    let name = match key {
        Some(name) => name.to_string(),
        None => featured_name(state, cfg, now)?,
    };
    let device = state
        .devices
        .iter()
        .find(|d| d.info.name == name)
        .filter(|d| tray_visible(d, cfg, now))?;
    let low_threshold = cfg.effective_low_threshold(&device.info.name);
    let (status, stale) = if device.presence == Presence::Online {
        (classify(device.last_reading, low_threshold), false)
    } else if let Some(reading) = device.last_reading {
        (classify_stale(reading.percent, low_threshold), true)
    } else {
        (PrimaryStatus::Offline, false)
    };
    Some(Resolved {
        state: device.clone(),
        status,
        stale,
    })
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
    pub refresh: RefreshSignal,
}

impl RigbatTray {
    /// Resolves this icon's device. `None` when the icon has nothing to show
    /// (no devices, or the keyed device disappeared / is hidden).
    fn resolve(&self) -> Option<Resolved> {
        let state = self.rx.borrow();
        let cfg = self.config.borrow();
        resolve_for(self.key.as_deref(), &state, &cfg, Instant::now())
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

    /// COSMIC shows no hover tooltip for tray icons, so for the aggregate
    /// icon this is the only textual channel naming the device it stands for.
    fn title(&self) -> String {
        match &self.key {
            Some(k) => k.clone(),
            None => self
                .resolve()
                .map_or_else(|| "rigbat".to_owned(), |r| r.state.info.name),
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
        let stale = resolved.as_ref().is_some_and(|r| r.stale);
        let kind = resolved.map(|r| r.state.info.kind);
        self.renderer.render(status, kind, &theme, mode, stale)
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
                .filter(|d| tray_visible(d, &cfg, now))
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
            // Device rows report status; they are not controls. Clicking one used
            // to write `primary_device`, which only the aggregate icon consumes —
            // in TrayMode::PerDevice there is no aggregate icon, so the click wrote
            // to disk and changed nothing a user could see.
            for (_dev_name, label, icon_name) in rows {
                items.push(MenuItem::Standard(ksni::menu::StandardItem {
                    label,
                    icon_name,
                    enabled: false,
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
                Ok(exe) => match std::process::Command::new(exe).arg("settings").spawn() {
                    // `Child` has no `Drop` that reaps, so dropping the handle
                    // leaves the exited settings process as a zombie for the
                    // tray's whole lifetime — one per click. Reap it on a
                    // throwaway thread, which lives exactly as long as the
                    // window does. A blocking wait must not run on the tray's
                    // own thread, and this closure is not on the tokio runtime.
                    Ok(mut child) => {
                        std::thread::spawn(move || {
                            if let Err(e) = child.wait() {
                                tracing::warn!("settings process could not be reaped: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::error!("failed to launch settings window: {e}"),
                },
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
        let now = Instant::now();
        let mut shown: Vec<String> = Vec::new();
        for d in &state.devices {
            if tray_visible(d, &cfg, now) && !shown.contains(&d.info.name) {
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

    use super::{RETAINED_ICON_MAX_AGE, desired_keys, featured_name, resolve_for, sanitize};
    use crate::app::supervisor::TrayState;
    use crate::config::{Config, TrayMode};
    use crate::domain::{
        BatteryReading, ChargeState, DeviceInfo, DeviceKind, DeviceState, Presence, PrimaryStatus,
    };
    use std::time::Duration;

    // --- sanitize -----------------------------------------------------------

    #[test]
    fn sanitize_keeps_a_readable_stem() {
        assert!(sanitize("mouse123").starts_with("mouse123-"));
        assert!(sanitize("MX Master 3").starts_with("MX-Master-3-"));
        assert!(sanitize("kbd/bt#1").starts_with("kbd-bt-1-"));
    }

    #[test]
    fn sanitize_is_stable_for_the_same_name() {
        assert_eq!(sanitize("MX Master 3"), sanitize("MX Master 3"));
    }

    /// Names that differ only in characters the stem flattens must still get
    /// distinct ids — hosts key per-item state on this string.
    #[test]
    fn sanitize_separates_names_the_stem_cannot() {
        assert_ne!(sanitize("Foo Bar"), sanitize("Foo-Bar"));
        assert_ne!(sanitize("héadset"), sanitize("hèadset"));
        assert_ne!(sanitize("Мышь"), sanitize("Клава"));
    }

    #[test]
    fn sanitize_empty_string_still_yields_an_id() {
        assert!(!sanitize("").is_empty());
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

    /// Builds a `TrayState` from (info, reading) pairs: `Some` reading means
    /// `Online`, `None` means never seen (`Unreachable`, nothing retained) —
    /// matching what these tests exercised before presence existed.
    /// A device that is not `Online` but still remembers a reading taken
    /// `age` ago — the state a sleeping Bluetooth peripheral is in, and the
    /// one `RETAINED_ICON_MAX_AGE` puts a shelf life on.
    fn retained(name: &str, percent: u8, age: Duration) -> DeviceState {
        DeviceState {
            info: make_info(name),
            last_reading: Some(make_reading(percent)),
            last_seen: Instant::now().checked_sub(age),
            presence: Presence::Unreachable,
            estimate: crate::domain::Estimate::Unknown,
        }
    }

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
        assert_eq!(
            featured_name(&state, &cfg, Instant::now()),
            Some("keyboard".to_string())
        );
    }

    #[test]
    fn featured_name_explicit_hidden_falls_back_to_first_connected() {
        // "gamepad" is hidden, so the explicit choice is ignored.
        let mut cfg = cfg_with_primary(Some("gamepad"));
        cfg.hidden_devices = vec!["gamepad".to_string()];
        let state = make_state(vec![
            (make_info("mouse"), Some(make_reading(80))),
            (make_info("keyboard"), Some(make_reading(50))),
            (make_info("gamepad"), Some(make_reading(30))),
        ]);
        // "gamepad" is not shown, so falls back to first connected shown: "mouse"
        assert_eq!(
            featured_name(&state, &cfg, Instant::now()),
            Some("mouse".to_string())
        );
    }

    #[test]
    fn featured_name_no_explicit_returns_first_connected_shown() {
        let state = make_state(vec![
            (make_info("mouse"), None),
            (make_info("keyboard"), Some(make_reading(60))),
        ]);
        let cfg = cfg_with_primary(None);
        assert_eq!(
            featured_name(&state, &cfg, Instant::now()),
            Some("keyboard".to_string())
        );
    }

    #[test]
    fn featured_name_no_connected_returns_first_shown() {
        let state = TrayState {
            devices: vec![
                retained("mouse", 80, Duration::from_secs(60)),
                retained("keyboard", 40, Duration::from_secs(60)),
            ],
        };
        let cfg = cfg_with_primary(None);
        // Nothing online; falls back to the first device still worth showing.
        assert_eq!(
            featured_name(&state, &cfg, Instant::now()),
            Some("mouse".to_string())
        );
    }

    /// Devices that have never answered — a dongle enumerated while its mouse
    /// is switched off — have nothing to feature.
    #[test]
    fn featured_name_ignores_devices_that_never_answered() {
        let state = make_state(vec![
            (make_info("mouse"), None),
            (make_info("keyboard"), None),
        ]);
        let cfg = cfg_with_primary(None);
        assert_eq!(featured_name(&state, &cfg, Instant::now()), None);
    }

    /// A reading old enough to be a fact about last week is not a battery
    /// level, and stops counting as one.
    #[test]
    fn featured_name_ignores_a_reading_past_its_shelf_life() {
        let state = TrayState {
            devices: vec![retained(
                "mouse",
                80,
                RETAINED_ICON_MAX_AGE + Duration::from_secs(60),
            )],
        };
        let cfg = cfg_with_primary(None);
        assert_eq!(featured_name(&state, &cfg, Instant::now()), None);
    }

    #[test]
    fn featured_name_no_devices_returns_none() {
        let state = make_state(vec![]);
        let cfg = cfg_with_primary(None);
        assert_eq!(featured_name(&state, &cfg, Instant::now()), None);
    }

    #[test]
    fn featured_name_all_hidden_returns_none() {
        let mut cfg = cfg_with_primary(None);
        cfg.hidden_devices = vec!["mouse".to_string(), "keyboard".to_string()];
        let state = make_state(vec![
            (make_info("mouse"), Some(make_reading(80))),
            (make_info("keyboard"), Some(make_reading(50))),
        ]);
        // Both present devices are hidden.
        assert_eq!(featured_name(&state, &cfg, Instant::now()), None);
    }

    // --- resolve_for ----------------------------------------------------------

    #[test]
    fn resolve_for_per_device_key_present_and_shown() {
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(80)))]);
        let cfg = cfg_with_primary(None);
        let resolved = resolve_for(Some("mouse"), &state, &cfg, Instant::now()).unwrap();
        assert_eq!(resolved.state.info.name, "mouse");
        assert_eq!(resolved.state.last_reading, Some(make_reading(80)));
    }

    #[test]
    fn resolve_for_per_device_key_hidden_by_hidden_devices_returns_none() {
        let mut cfg = cfg_with_primary(None);
        cfg.hidden_devices = vec!["mouse".to_string()];
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(80)))]);
        assert!(resolve_for(Some("mouse"), &state, &cfg, Instant::now()).is_none());
    }

    #[test]
    fn resolve_for_per_device_key_absent_from_state_returns_none() {
        let state = make_state(vec![(make_info("keyboard"), Some(make_reading(50)))]);
        let cfg = cfg_with_primary(None);
        assert!(resolve_for(Some("mouse"), &state, &cfg, Instant::now()).is_none());
    }

    #[test]
    fn resolve_for_aggregate_key_uses_explicit_primary_device() {
        let state = make_state(vec![
            (make_info("mouse"), Some(make_reading(80))),
            (make_info("keyboard"), Some(make_reading(50))),
        ]);
        let cfg = cfg_with_primary(Some("keyboard"));
        let resolved = resolve_for(None, &state, &cfg, Instant::now()).unwrap();
        assert_eq!(resolved.state.info.name, "keyboard");
    }

    #[test]
    fn resolve_for_aggregate_key_no_devices_returns_none() {
        let state = make_state(vec![]);
        let cfg = cfg_with_primary(None);
        assert!(resolve_for(None, &state, &cfg, Instant::now()).is_none());
    }

    #[test]
    fn resolve_for_status_uses_per_device_low_threshold_override() {
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(15)))]);
        let mut cfg = cfg_with_primary(None);

        // Global threshold (20) would already flag 15% as Low; override it down
        // so the global default alone would report Ok, isolating the override.
        cfg.low_threshold = 5;
        let resolved = resolve_for(Some("mouse"), &state, &cfg, Instant::now()).unwrap();
        assert!(matches!(resolved.status, PrimaryStatus::Ok { .. }));

        cfg.device_overrides.insert(
            "mouse".to_string(),
            crate::config::DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(20),
            },
        );
        let resolved = resolve_for(Some("mouse"), &state, &cfg, Instant::now()).unwrap();
        assert!(matches!(resolved.status, PrimaryStatus::Low { .. }));
    }

    #[test]
    fn resolve_for_unreachable_device_with_retained_reading_classifies_and_marks_stale() {
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![DeviceState {
                info: make_info("mouse"),
                last_reading: Some(make_reading(5)), // classifies as Low
                last_seen: Some(Instant::now()),
                presence: Presence::Unreachable,
                estimate: crate::domain::Estimate::Unknown,
            }],
        };
        let resolved = resolve_for(Some("mouse"), &state, &cfg, Instant::now()).unwrap();
        assert!(matches!(resolved.status, PrimaryStatus::Low { .. }));
        assert!(resolved.stale);
    }

    #[test]
    /// The icon this removed: a wireless dongle stays enumerated while its
    /// mouse is off, so the device is discovered and polled and never answers.
    /// An empty battery outline that has never meant anything is worse than no
    /// icon — the user reported one sitting in the tray for days.
    fn resolve_for_unreachable_device_with_no_reading_shows_nothing() {
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![DeviceState {
                info: make_info("mouse"),
                last_reading: None,
                last_seen: None,
                presence: Presence::Unreachable,
                estimate: crate::domain::Estimate::Unknown,
            }],
        };
        assert!(resolve_for(Some("mouse"), &state, &cfg, Instant::now()).is_none());
    }

    #[test]
    fn resolve_for_keeps_a_recent_retained_reading() {
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![retained("mouse", 88, Duration::from_secs(3600))],
        };
        let resolved = resolve_for(Some("mouse"), &state, &cfg, Instant::now()).unwrap();
        assert_eq!(resolved.status, PrimaryStatus::Ok { percent: 88 });
        assert!(resolved.stale);
    }

    #[test]
    fn resolve_for_drops_a_retained_reading_past_its_shelf_life() {
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![retained(
                "mouse",
                88,
                RETAINED_ICON_MAX_AGE + Duration::from_secs(1),
            )],
        };
        assert!(resolve_for(Some("mouse"), &state, &cfg, Instant::now()).is_none());
    }

    #[test]
    fn resolve_for_unreachable_stale_charging_never_shows_charging() {
        // Regression test for the reported bug: a mouse retained at 80%
        // Charging, then gone unreachable, must not render green.
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![DeviceState {
                info: make_info("mouse"),
                last_reading: Some(BatteryReading::new(80, ChargeState::Charging)),
                last_seen: Some(Instant::now()),
                presence: Presence::Unreachable,
                estimate: crate::domain::Estimate::Unknown,
            }],
        };
        let resolved = resolve_for(Some("mouse"), &state, &cfg, Instant::now()).unwrap();
        assert_eq!(resolved.status, PrimaryStatus::Ok { percent: 80 });
        assert!(resolved.stale);
    }

    #[test]
    fn resolve_for_unreachable_stale_charging_below_threshold_is_low_not_hidden() {
        // A stale Charging flag must not outrank and hide a low battery.
        let mut cfg = cfg_with_primary(None);
        cfg.low_threshold = 20;
        let state = TrayState {
            devices: vec![DeviceState {
                info: make_info("mouse"),
                last_reading: Some(BatteryReading::new(15, ChargeState::Charging)),
                last_seen: Some(Instant::now()),
                presence: Presence::Unreachable,
                estimate: crate::domain::Estimate::Unknown,
            }],
        };
        let resolved = resolve_for(Some("mouse"), &state, &cfg, Instant::now()).unwrap();
        assert_eq!(resolved.status, PrimaryStatus::Low { percent: 15 });
        assert!(resolved.stale);
    }

    #[test]
    fn resolve_for_online_device_is_never_stale() {
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(80)))]);
        let cfg = cfg_with_primary(None);
        let resolved = resolve_for(Some("mouse"), &state, &cfg, Instant::now()).unwrap();
        assert!(!resolved.stale);
    }
}
