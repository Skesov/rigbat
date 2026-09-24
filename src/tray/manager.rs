use std::collections::HashMap;
use std::time::{Duration, Instant};

use ksni::{MenuItem, ToolTip, Tray, TrayMethods};
use tokio::sync::watch;

use crate::app::refresh::RefreshSignal;
use crate::app::supervisor::TrayState;
use crate::appearance::ColorScheme;
use crate::config::{Config, TrayMode};
use crate::domain::{
    DeviceId, DeviceState, Presence, PrimaryStatus, device_status, format_device_entry,
    freedesktop_icon_name, select_featured,
};
use crate::i18n::{fl, loader};
use crate::icon::{IconRenderer, Theme, TinySkiaRenderer};

// ---------------------------------------------------------------------------
// sni_id: a stable, unique SNI item id per device
// ---------------------------------------------------------------------------

/// Builds the SNI item id for a per-device icon.
///
/// The readable stem (name, transport) flattens every character outside
/// `[A-Za-z0-9]` to `-` and omits the locator, so it collides on its own; the
/// suffix hashes the whole `DeviceId`. Hosts key remembered position on this
/// id, and every `DeviceId` field survives a restart, so the id does too.
fn sni_id(id: &DeviceId) -> String {
    let stem: String = format!("{}-{}", id.name, id.transport.as_str())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("rigbat-{stem}-{:08x}", id_hash(id))
}

/// FNV-1a over every field; not `std::hash`, whose `RandomState` reseeds per process.
fn id_hash(id: &DeviceId) -> u32 {
    // Separators and a presence tag keep ("ab","c") from ("a","bc") and None from Some("").
    let locator: [&[u8]; 2] = match &id.locator {
        Some(l) => [b"\x1fS", l.as_bytes()],
        None => [b"\x1fN", b""],
    };
    let fields: [&[u8]; 3] = [
        id.name.as_bytes(),
        b"\x1f",
        id.transport.as_str().as_bytes(),
    ];
    let mut hash: u32 = 0x811c_9dc5;
    for byte in fields.iter().chain(&locator).flat_map(|f| f.iter()) {
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
pub(super) fn tray_visible(device: &DeviceState, cfg: &Config, now: Instant) -> bool {
    cfg.is_shown(&device.info.name) && device.is_currently_informative(now, RETAINED_ICON_MAX_AGE)
}

// ---------------------------------------------------------------------------
// desired_keys: compute the set of icon keys from mode and shown devices
// ---------------------------------------------------------------------------

/// Returns the list of tray icon keys for the given mode and shown devices.
/// `None` is the aggregate/primary icon; `Some(id)` is a per-device icon.
pub fn desired_keys(mode: TrayMode, shown: &[DeviceId]) -> Vec<Option<DeviceId>> {
    match (mode, shown.is_empty()) {
        // No devices → always one "No devices" aggregate icon.
        (_, true) => vec![None],
        // One aggregate icon.
        (TrayMode::PrimaryOnly, _) => vec![None],
        // One icon per shown device.
        (TrayMode::PerDevice, _) => shown.iter().map(|n| Some(n.clone())).collect(),
    }
}

/// The tray-visible devices, in roster order, one entry per `DeviceId`.
fn shown_ids(state: &TrayState, cfg: &Config, now: Instant) -> Vec<DeviceId> {
    let mut shown: Vec<DeviceId> = Vec::new();
    for d in state.devices.iter().filter(|d| tray_visible(d, cfg, now)) {
        let id = d.info.id();
        if !shown.contains(&id) {
            shown.push(id);
        }
    }
    shown
}

// ---------------------------------------------------------------------------
// featured_id — the device the aggregate icon represents
// ---------------------------------------------------------------------------

/// Returns the device the single (aggregate) icon represents.
///
/// Priority: explicit user choice (if still shown) → first connected shown
/// device → first shown device → None. The pin is name-keyed, so among
/// visible devices sharing the picked name the first online one wins.
pub(super) fn featured_id(state: &TrayState, cfg: &Config, now: Instant) -> Option<DeviceId> {
    let visible: Vec<&DeviceState> = state
        .devices
        .iter()
        .filter(|d| tray_visible(d, cfg, now))
        .collect();
    let shown: Vec<(&str, bool)> = visible
        .iter()
        .map(|d| (d.info.name.as_str(), d.presence == Presence::Online))
        .collect();
    let name = select_featured(&shown, cfg.primary_device.as_deref())?;

    let mut named = visible.iter().filter(|d| d.info.name == name);
    let first = named.clone().next();
    named
        .find(|d| d.presence == Presence::Online)
        .or(first)
        .map(|d| d.info.id())
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
/// Per-device icon (`key = Some(id)`): that device, if present and shown.
/// A device named in `hidden_devices` resolves to `None` even if `key` still
/// names it: `desired_keys` only creates per-device icons from the shown
/// list, so a keyed-but-hidden state is transient (config changed, reconcile
/// has not yet retired the icon) and showing it for one frame would be the
/// bug.
/// Aggregate icon (`key = None`): `featured_id`'s pick.
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
    key: Option<&DeviceId>,
    state: &TrayState,
    cfg: &Config,
    now: Instant,
) -> Option<Resolved> {
    let id = match key {
        Some(id) => id.clone(),
        None => featured_id(state, cfg, now)?,
    };
    let device = state
        .devices
        .iter()
        .find(|d| d.info.id() == id)
        .filter(|d| tray_visible(d, cfg, now))?;
    let (status, stale) = device_status(device, cfg.effective_low_threshold(&device.info.name));
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
    /// `Some(id)` = per-device icon; `None` = aggregate/primary icon.
    pub key: Option<DeviceId>,
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
        resolve_for(self.key.as_ref(), &state, &cfg, Instant::now())
        // `state` and `cfg` (watch::Ref) are dropped here, before any await.
    }
}

impl Tray for RigbatTray {
    // Left click opens the dashboard; the host shows the menu on right click.
    const MENU_ON_ACTIVATE: bool = false;

    fn activate(&mut self, _x: i32, _y: i32) {
        launch("dashboard");
    }

    fn id(&self) -> String {
        match &self.key {
            Some(k) => sni_id(k),
            None => "rigbat".into(),
        }
    }

    /// COSMIC shows no hover tooltip for tray icons, so for the aggregate
    /// icon this is the only textual channel naming the device it stands for.
    fn title(&self) -> String {
        match &self.key {
            Some(k) => k.name.clone(),
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
        let lang = self.config.borrow().lang();
        let l = loader(lang);
        let title = self
            .resolve()
            .map(|r| format_device_entry(&r.state, Instant::now(), lang))
            .unwrap_or_else(|| match &self.key {
                Some(id) => fl!(l, "entry-offline", name = id.name.as_str()),
                None => fl!(l, "tray-no-devices"),
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
        let now = Instant::now();

        let (rows, lang) = {
            let state = self.rx.borrow();
            let cfg = self.config.borrow();
            let lang = cfg.lang();

            // The bullet and the rows come from one pair of borrows. Resolving
            // the highlight separately re-borrowed the channels, so a state
            // change landing between the two could bullet a device the rows
            // below no longer described.
            let highlight: Option<DeviceId> =
                resolve_for(self.key.as_ref(), &state, &cfg, now).map(|r| r.state.info.id());

            // Collect (label, icon_name) for each shown device.
            let rows: Vec<(String, String)> = state
                .devices
                .iter()
                .filter(|d| tray_visible(d, &cfg, now))
                .map(|d| {
                    let prefix = if highlight.as_ref() == Some(&d.info.id()) {
                        "\u{25cf} " // "● "
                    } else {
                        "  "
                    };
                    let entry = format_device_entry(d, now, lang);
                    let label = format!("{prefix}{entry}");
                    let icon = freedesktop_icon_name(d.info.kind).to_owned();
                    (label, icon)
                })
                .collect();

            (rows, lang)
        };
        // All watch borrows are released here.
        let l = loader(lang);

        let mut items: Vec<MenuItem<Self>> = Vec::new();

        if rows.is_empty() {
            items.push(MenuItem::Standard(ksni::menu::StandardItem {
                label: fl!(l, "tray-no-devices"),
                enabled: false,
                ..ksni::menu::StandardItem::default()
            }));
        } else {
            // Device rows report status; they are not controls. Clicking one used
            // to write `primary_device`, which only the aggregate icon consumes —
            // in TrayMode::PerDevice there is no aggregate icon, so the click wrote
            // to disk and changed nothing a user could see.
            for (label, icon_name) in rows {
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
            label: fl!(l, "tray-dashboard"),
            activate: Box::new(|_: &mut Self| launch("dashboard")),
            ..ksni::menu::StandardItem::default()
        }));

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: fl!(l, "tray-refresh"),
            icon_name: "view-refresh".into(),
            activate: Box::new(|app: &mut Self| app.refresh.trigger()),
            ..ksni::menu::StandardItem::default()
        }));

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: fl!(l, "tray-settings"),
            activate: Box::new(|_: &mut Self| launch("settings")),
            ..ksni::menu::StandardItem::default()
        }));

        items.push(MenuItem::Separator);

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: fl!(l, "tray-quit"),
            activate: Box::new(|_| std::process::exit(0)),
            ..ksni::menu::StandardItem::default()
        }));

        items
    }
}

/// Starts `rigbat <subcommand>` as a window process of its own.
fn launch(subcommand: &'static str) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            tracing::error!("cannot find own executable: {e}");
            return;
        }
    };
    match std::process::Command::new(exe).arg(subcommand).spawn() {
        // `Child` does not reap on drop: without the wait, every closed window
        // stays a zombie for the tray's lifetime. The wait blocks, so it gets a
        // thread of its own rather than the tray's.
        Ok(mut child) => {
            std::thread::spawn(move || {
                if let Err(e) = child.wait() {
                    tracing::warn!("{subcommand} process could not be reaped: {e}");
                }
            });
        }
        Err(e) => tracing::error!("failed to launch rigbat {subcommand}: {e}"),
    }
}

// ---------------------------------------------------------------------------
// reconcile — sync live SNI items against the desired icon set
// ---------------------------------------------------------------------------

async fn reconcile(
    items: &mut HashMap<Option<DeviceId>, ksni::Handle<RigbatTray>>,
    rx: &watch::Receiver<TrayState>,
    theme_rx: &watch::Receiver<ColorScheme>,
    config_rx: &watch::Receiver<Config>,
    refresh: &RefreshSignal,
) {
    // Compute desired key list without holding any watch::Ref across an await.
    let desired: Vec<Option<DeviceId>> = {
        let state = rx.borrow();
        let cfg = config_rx.borrow();
        desired_keys(cfg.tray_mode, &shown_ids(&state, &cfg, Instant::now()))
    }; // borrows dropped here

    let prev_len = items.len();

    // Retire icons whose keys are no longer desired.
    let to_remove: Vec<Option<DeviceId>> = items
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
    let mut items: HashMap<Option<DeviceId>, ksni::Handle<RigbatTray>> = HashMap::new();

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

    use super::{
        ColorScheme, MenuItem, RETAINED_ICON_MAX_AGE, RefreshSignal, RigbatTray, TinySkiaRenderer,
        Tray as _, desired_keys, featured_id, resolve_for, shown_ids, sni_id, watch,
    };
    use crate::app::supervisor::TrayState;
    use crate::config::{Config, TrayMode};
    use crate::domain::{
        BatteryReading, ChargeState, DeviceId, DeviceInfo, DeviceKind, DeviceState, Presence,
        PrimaryStatus, Transport,
    };
    use std::time::Duration;

    fn id(name: &str, transport: Transport, locator: Option<&str>) -> DeviceId {
        DeviceId {
            name: name.to_owned(),
            transport,
            locator: locator.map(str::to_owned),
        }
    }

    /// The `DeviceId` of a `make_info` device.
    fn key(name: &str) -> DeviceId {
        make_info(name).id()
    }

    // --- sni_id -------------------------------------------------------------

    #[test]
    fn sni_id_keeps_a_readable_stem() {
        let mx = id("MX Master 3", Transport::Bluetooth, Some("AA:BB"));
        assert!(sni_id(&mx).starts_with("rigbat-MX-Master-3-bluetooth-"));
        let kbd = id("kbd/bt#1", Transport::Hidraw, None);
        assert!(sni_id(&kbd).starts_with("rigbat-kbd-bt-1-hidraw-"));
    }

    /// Pinned value: hosts remember icon positions by this id, so it must not
    /// depend on anything reseeded per process.
    #[test]
    fn sni_id_is_stable_across_processes() {
        let mouse = id("mouse", Transport::Sysfs, Some("hidpp_battery_0"));
        assert_eq!(sni_id(&mouse), "rigbat-mouse-sysfs-1619ed09");
    }

    /// Ids that differ only in what the stem flattens or omits must still
    /// get distinct SNI ids — hosts key per-item state on this string.
    #[test]
    fn sni_id_separates_ids_the_stem_cannot() {
        let s = |name| sni_id(&id(name, Transport::Sysfs, None));
        assert_ne!(s("Foo Bar"), s("Foo-Bar"));
        assert_ne!(s("héadset"), s("hèadset"));
        assert_ne!(s("Мышь"), s("Клава"));

        let mx = |t, l| sni_id(&id("MX", t, l));
        assert_ne!(
            mx(Transport::Sysfs, None),
            mx(Transport::Bluetooth, None),
            "same name, different transport"
        );
        assert_ne!(
            mx(Transport::Hidraw, Some("1-2")),
            mx(Transport::Hidraw, Some("1-3")),
            "same name and transport, different locator"
        );
        assert_ne!(mx(Transport::Hidraw, None), mx(Transport::Hidraw, Some("")));
    }

    #[test]
    fn sni_id_empty_name_still_yields_an_id() {
        assert!(!sni_id(&id("", Transport::Sysfs, None)).is_empty());
    }

    // --- desired_keys -------------------------------------------------------

    #[test]
    fn desired_keys_primary_only_returns_one_none() {
        let shown = vec![key("mouse"), key("keyboard")];
        assert_eq!(desired_keys(TrayMode::PrimaryOnly, &shown), vec![None]);
    }

    #[test]
    fn desired_keys_per_device_returns_some_per_device() {
        let shown = vec![key("mouse"), key("keyboard")];
        let result = desired_keys(TrayMode::PerDevice, &shown);
        assert_eq!(result, vec![Some(key("mouse")), Some(key("keyboard"))]);
    }

    #[test]
    fn desired_keys_empty_shown_returns_one_none_regardless_of_mode() {
        assert_eq!(desired_keys(TrayMode::PrimaryOnly, &[]), vec![None]);
        assert_eq!(desired_keys(TrayMode::PerDevice, &[]), vec![None]);
    }

    // --- one name, two transports ------------------------------------------

    /// One mouse seen over sysfs/HID++ and over Bluetooth: the project does
    /// not dedup, so these are two devices.
    fn same_name_two_transports(bt_presence: Presence) -> TrayState {
        let mx = |transport, locator: &str, percent, presence| DeviceState {
            info: DeviceInfo {
                name: "MX".to_owned(),
                kind: DeviceKind::Mouse,
                transport,
                locator: Some(locator.to_owned()),
            },
            last_reading: Some(make_reading(percent)),
            last_seen: Some(Instant::now()),
            presence,
            estimate: crate::domain::Estimate::Unknown,
        };
        TrayState {
            devices: vec![
                mx(Transport::Sysfs, "hidpp_0", 70, Presence::Unreachable),
                mx(Transport::Bluetooth, "AA:BB", 40, bt_presence),
            ],
        }
    }

    #[test]
    fn same_name_over_two_transports_gets_two_icons_each_resolving_to_its_own_reading() {
        let state = same_name_two_transports(Presence::Online);
        let cfg = Config {
            tray_mode: TrayMode::PerDevice,
            ..Config::default()
        };
        let now = Instant::now();
        let keys = desired_keys(cfg.tray_mode, &shown_ids(&state, &cfg, now));
        assert_eq!(keys.len(), 2, "two icons, not one: {keys:?}");

        let percents: Vec<(Transport, Option<u8>)> = keys
            .iter()
            .map(|k| {
                let r = resolve_for(k.as_ref(), &state, &cfg, now).unwrap();
                (
                    r.state.info.transport,
                    r.state.last_reading.map(|r| r.percent),
                )
            })
            .collect();
        assert_eq!(
            percents,
            [
                (Transport::Sysfs, Some(70)),
                (Transport::Bluetooth, Some(40))
            ]
        );

        let ids: Vec<String> = keys.iter().flatten().map(sni_id).collect();
        assert_ne!(ids[0], ids[1]);
    }

    /// The pin is by name; between two devices sharing it, the aggregate icon
    /// shows the one that is answering, not whichever the roster lists first.
    #[test]
    fn featured_id_prefers_the_online_device_among_same_named_ones() {
        let state = same_name_two_transports(Presence::Online);
        let cfg = cfg_with_primary(Some("MX"));
        let featured = featured_id(&state, &cfg, Instant::now()).unwrap();
        assert_eq!(featured.transport, Transport::Bluetooth);
        let resolved = resolve_for(None, &state, &cfg, Instant::now()).unwrap();
        assert_eq!(resolved.state.last_reading, Some(make_reading(40)));
    }

    #[test]
    fn featured_id_falls_back_to_the_first_same_named_device_when_none_is_online() {
        let state = same_name_two_transports(Presence::Unreachable);
        let cfg = cfg_with_primary(Some("MX"));
        let featured = featured_id(&state, &cfg, Instant::now()).unwrap();
        assert_eq!(featured.transport, Transport::Sysfs);
    }

    // --- featured_id --------------------------------------------------------

    fn featured_name(state: &TrayState, cfg: &Config, now: Instant) -> Option<String> {
        featured_id(state, cfg, now).map(|id| id.name)
    }

    fn make_info(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            kind: DeviceKind::Mouse,
            transport: Transport::Sysfs,
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
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).unwrap();
        assert_eq!(resolved.state.info.name, "mouse");
        assert_eq!(resolved.state.last_reading, Some(make_reading(80)));
    }

    #[test]
    fn resolve_for_per_device_key_hidden_by_hidden_devices_returns_none() {
        let mut cfg = cfg_with_primary(None);
        cfg.hidden_devices = vec!["mouse".to_string()];
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(80)))]);
        assert!(resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).is_none());
    }

    #[test]
    fn resolve_for_per_device_key_absent_from_state_returns_none() {
        let state = make_state(vec![(make_info("keyboard"), Some(make_reading(50)))]);
        let cfg = cfg_with_primary(None);
        assert!(resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).is_none());
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
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).unwrap();
        assert!(matches!(resolved.status, PrimaryStatus::Ok { .. }));

        cfg.device_overrides.insert(
            "mouse".to_string(),
            crate::config::DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(20),
            },
        );
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).unwrap();
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
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).unwrap();
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
        assert!(resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).is_none());
    }

    #[test]
    fn resolve_for_keeps_a_recent_retained_reading() {
        let cfg = cfg_with_primary(None);
        let state = TrayState {
            devices: vec![retained("mouse", 88, Duration::from_secs(3600))],
        };
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).unwrap();
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
        assert!(resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).is_none());
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
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).unwrap();
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
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).unwrap();
        assert_eq!(resolved.status, PrimaryStatus::Low { percent: 15 });
        assert!(resolved.stale);
    }

    #[test]
    fn resolve_for_online_device_is_never_stale() {
        let state = make_state(vec![(make_info("mouse"), Some(make_reading(80)))]);
        let cfg = cfg_with_primary(None);
        let resolved = resolve_for(Some(&key("mouse")), &state, &cfg, Instant::now()).unwrap();
        assert!(!resolved.stale);
    }

    // --- no access --------------------------------------------------------------

    fn no_access(name: &str) -> DeviceState {
        DeviceState {
            info: make_info(name),
            last_reading: None,
            last_seen: None,
            presence: Presence::NoAccess,
            estimate: crate::domain::Estimate::Unknown,
        }
    }

    #[test]
    fn featured_id_prefers_an_online_device_over_one_without_access() {
        let mut state = make_state(vec![(make_info("keyboard"), Some(make_reading(60)))]);
        state.devices.insert(0, no_access("mouse"));
        let cfg = cfg_with_primary(None);
        assert_eq!(
            featured_name(&state, &cfg, Instant::now()),
            Some("keyboard".to_string())
        );
    }

    fn tray_for(key: Option<DeviceId>, state: TrayState) -> RigbatTray {
        RigbatTray {
            key,
            rx: watch::channel(state).1,
            theme_rx: watch::channel(ColorScheme::Dark).1,
            config: watch::channel(Config::default()).1,
            renderer: Box::new(TinySkiaRenderer::default()),
            refresh: RefreshSignal::new(),
        }
    }

    #[test]
    fn menu_and_tooltip_say_no_access_and_point_at_doctor() {
        let state = TrayState {
            devices: vec![no_access("mouse")],
        };
        let tray = tray_for(Some(key("mouse")), state);
        let expected = "mouse: no access (run rigbat doctor)";

        assert_eq!(tray.tool_tip().title, expected);
        let labels: Vec<String> = tray
            .menu()
            .into_iter()
            .filter_map(|item| match item {
                MenuItem::Standard(item) => Some(item.label),
                _ => None,
            })
            .collect();
        assert!(
            labels.iter().any(|label| label.ends_with(expected)),
            "{labels:?}"
        );
    }

    mod bus {
        use std::collections::BTreeSet;
        use std::time::Duration;

        use super::super::run;
        use super::{
            ColorScheme, Config, RefreshSignal, Transport, TrayMode, make_info, make_reading,
            make_state, sni_id, watch,
        };
        use crate::bus_test::{eventually, isolated};

        const TIMEOUT: Duration = Duration::from_secs(5);

        struct FakeWatcher;

        #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
        impl FakeWatcher {
            fn register_status_notifier_item(&self, _service: &str) {}

            #[zbus(property)]
            fn is_status_notifier_host_registered(&self) -> bool {
                true
            }
        }

        async fn item_ids(conn: &zbus::Connection) -> BTreeSet<String> {
            let dbus = zbus::fdo::DBusProxy::new(conn).await.expect("DBus proxy");
            let mut ids = BTreeSet::new();
            for name in dbus.list_names().await.expect("ListNames") {
                if !name.starts_with("org.kde.StatusNotifierItem-") {
                    continue;
                }
                let reply = conn
                    .call_method(
                        Some(name.as_str()),
                        "/StatusNotifierItem",
                        Some("org.freedesktop.DBus.Properties"),
                        "Get",
                        &("org.kde.StatusNotifierItem", "Id"),
                    )
                    .await;
                let id = reply.ok().and_then(|reply| {
                    let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
                    String::try_from(value).ok()
                });
                ids.extend(id);
            }
            ids
        }

        async fn until_ids(conn: &zbus::Connection, want: &[&str]) {
            let want: BTreeSet<String> = want.iter().map(|id| (*id).to_owned()).collect();
            let want = &want;
            eventually(TIMEOUT, || async move {
                (item_ids(conn).await == *want).then_some(())
            })
            .await;
        }

        #[tokio::test]
        async fn one_icon_per_device_id_follows_the_roster() {
            if !isolated(module_path!(), "one_icon_per_device_id_follows_the_roster") {
                return;
            }
            let _watcher = zbus::connection::Builder::session()
                .expect("private bus")
                .name("org.kde.StatusNotifierWatcher")
                .expect("name")
                .serve_at("/StatusNotifierWatcher", FakeWatcher)
                .expect("path")
                .build()
                .await
                .expect("fake watcher");

            let sysfs = make_info("mouse");
            let mut bluetooth = make_info("mouse");
            bluetooth.transport = Transport::Bluetooth;
            let (state_tx, state_rx) = watch::channel(make_state(vec![
                (sysfs.clone(), Some(make_reading(80))),
                (bluetooth.clone(), Some(make_reading(40))),
            ]));
            let (_theme_tx, theme_rx) = watch::channel(ColorScheme::Dark);
            let (config_tx, config_rx) = watch::channel(Config {
                tray_mode: TrayMode::PerDevice,
                ..Config::default()
            });
            tokio::spawn(run(state_rx, theme_rx, config_rx, RefreshSignal::new()));

            let client = zbus::Connection::session().await.expect("private bus");
            let sysfs_id = sni_id(&sysfs.id());
            let bluetooth_id = sni_id(&bluetooth.id());
            until_ids(&client, &[&sysfs_id, &bluetooth_id]).await;

            state_tx.send_replace(make_state(vec![(sysfs, Some(make_reading(80)))]));
            until_ids(&client, &[&sysfs_id]).await;

            config_tx.send_modify(|c| c.tray_mode = TrayMode::PrimaryOnly);
            until_ids(&client, &["rigbat"]).await;
        }
    }
}
