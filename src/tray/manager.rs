use std::collections::HashMap;
use std::time::Instant;

use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{MenuItem, ToolTip, Tray, TrayMethods};
use tokio::sync::watch;

use crate::app::refresh::RefreshSignal;
use crate::app::supervisor::TrayState;
use crate::appearance::ColorScheme;
use crate::config::{Config, TrayMode};
use crate::domain::{
    DeviceId, DeviceState, PrimaryStatus, Roster, device_line, device_status,
    freedesktop_icon_name, is_visible,
};
use crate::i18n::{Lang, fl, loader};
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

/// The devices the tray shows, in roster order.
fn visible<'a>(state: &'a TrayState, cfg: &Config, now: Instant) -> Roster<'a> {
    Roster::visible(&state.devices, |name| cfg.is_shown(name), now)
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
    for d in visible(state, cfg, now).devices() {
        let id = d.info.id();
        if !shown.contains(&id) {
            shown.push(id);
        }
    }
    shown
}

/// The device the single (aggregate) icon represents: `Roster::featured`.
pub(super) fn featured_id(state: &TrayState, cfg: &Config, now: Instant) -> Option<DeviceId> {
    visible(state, cfg, now)
        .featured(cfg.primary_device.as_deref())
        .map(|d| d.info.id())
}

// ---------------------------------------------------------------------------
// resolve — the single answer to "which device does this icon represent?"
// ---------------------------------------------------------------------------

/// The device a tray icon stands for, resolved against the current state and config.
pub(crate) struct Resolved {
    pub(crate) state: DeviceState,
    pub(crate) status: PrimaryStatus,
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
pub(crate) fn resolve_for(
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
        .filter(|d| is_visible(d, |name| cfg.is_shown(name), now))?;
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

/// Writes `config.json`; a parameter so tests never touch the real file.
pub type SaveConfig = fn(&Config) -> anyhow::Result<()>;

pub struct RigbatTray {
    /// `Some(id)` = per-device icon; `None` = aggregate/primary icon.
    pub key: Option<DeviceId>,
    pub rx: watch::Receiver<TrayState>,
    pub theme_rx: watch::Receiver<ColorScheme>,
    pub config: watch::Sender<Config>,
    pub save_config: SaveConfig,
    pub renderer: Box<dyn IconRenderer>,
    pub refresh: RefreshSignal,
}

/// A device row of the menu, read under one pair of borrows.
struct MenuRow {
    name: String,
    label: String,
    icon_name: String,
    pinned: bool,
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

    /// Saves `primary_device` and publishes it on the config channel, whose
    /// change makes the manager loop re-publish every icon: ksni does not
    /// re-publish an icon after a menu event.
    fn pin(&self, device: Option<String>) {
        let mut cfg = self.config.borrow().clone();
        if cfg.primary_device == device {
            return;
        }
        cfg.primary_device = device;
        match (self.save_config)(&cfg) {
            Ok(()) => {
                self.config.send_replace(cfg);
            }
            Err(e) => tracing::error!("failed to save the tray device choice: {e:#}"),
        }
    }

    fn menu_rows(&self, now: Instant) -> (Vec<MenuRow>, TrayMode, bool, Lang) {
        let state = self.rx.borrow();
        let cfg = self.config.borrow();
        let lang = cfg.lang();
        let pinned = featured_id(&state, &cfg, now)
            .filter(|id| cfg.primary_device.as_deref() == Some(id.name.as_str()));
        let rows = visible(&state, &cfg, now)
            .devices()
            .iter()
            .map(|d| {
                let (status, _) = device_status(d, cfg.effective_low_threshold(&d.info.name));
                MenuRow {
                    name: d.info.name.clone(),
                    label: mnemonic_escape(&device_line(d, status, now, lang)),
                    icon_name: freedesktop_icon_name(d.info.kind).to_owned(),
                    pinned: pinned.as_ref() == Some(&d.info.id()),
                }
            })
            .collect();
        (rows, cfg.tray_mode, cfg.primary_device.is_none(), lang)
    }
}

/// DBusMenu labels swallow a single `_` as a mnemonic marker; `__` shows one.
fn mnemonic_escape(label: &str) -> String {
    label.replace('_', "__")
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
            .map(|r| device_line(&r.state, r.status, Instant::now(), lang))
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
        let (rows, mode, automatic, lang) = self.menu_rows(Instant::now());
        let l = loader(lang);

        let mut items: Vec<MenuItem<Self>> = Vec::new();
        if rows.is_empty() {
            items.push(
                StandardItem {
                    label: fl!(l, "tray-no-devices"),
                    enabled: false,
                    ..StandardItem::default()
                }
                .into(),
            );
        } else if mode == TrayMode::PrimaryOnly {
            items.push(
                CheckmarkItem {
                    label: fl!(l, "tray-automatic"),
                    checked: automatic,
                    activate: Box::new(|tray: &mut Self| tray.pin(None)),
                    ..CheckmarkItem::default()
                }
                .into(),
            );
            for row in rows {
                let name = row.name;
                items.push(
                    CheckmarkItem {
                        label: row.label,
                        icon_name: row.icon_name,
                        checked: row.pinned,
                        activate: Box::new(move |tray: &mut Self| tray.pin(Some(name.clone()))),
                        ..CheckmarkItem::default()
                    }
                    .into(),
                );
            }
        } else {
            for row in rows {
                items.push(
                    StandardItem {
                        label: row.label,
                        icon_name: row.icon_name,
                        activate: Box::new(|_: &mut Self| launch("dashboard")),
                        ..StandardItem::default()
                    }
                    .into(),
                );
            }
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: fl!(l, "tray-dashboard"),
                activate: Box::new(|_: &mut Self| launch("dashboard")),
                ..StandardItem::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: fl!(l, "tray-refresh"),
                activate: Box::new(|tray: &mut Self| tray.refresh.trigger()),
                ..StandardItem::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: fl!(l, "tray-settings"),
                activate: Box::new(|_: &mut Self| launch("settings")),
                ..StandardItem::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: fl!(l, "tray-quit"),
                activate: Box::new(|_| std::process::exit(0)),
                ..StandardItem::default()
            }
            .into(),
        );
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
    config: &watch::Sender<Config>,
    save_config: SaveConfig,
    refresh: &RefreshSignal,
) {
    // Compute desired key list without holding any watch::Ref across an await.
    let desired: Vec<Option<DeviceId>> = {
        let state = rx.borrow();
        let cfg = config.borrow();
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
                config: config.clone(),
                save_config,
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
    config: watch::Sender<Config>,
    save_config: SaveConfig,
    refresh: RefreshSignal,
) {
    let mut items: HashMap<Option<DeviceId>, ksni::Handle<RigbatTray>> = HashMap::new();
    let mut config_rx = config.subscribe();

    loop {
        reconcile(&mut items, &rx, &theme_rx, &config, save_config, &refresh).await;

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
        ColorScheme, MenuItem, RefreshSignal, RigbatTray, SaveConfig, TinySkiaRenderer, Tray as _,
        desired_keys, featured_id, resolve_for, shown_ids, sni_id, watch,
    };
    use crate::app::supervisor::TrayState;
    use crate::config::{Config, TrayMode};
    use crate::domain::{
        BatteryReading, ChargeState, DeviceId, DeviceInfo, DeviceKind, DeviceState, Presence,
        PrimaryStatus, RETAINED_ICON_MAX_AGE, Transport,
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
                (Transport::Bluetooth, Some(40)),
                (Transport::Sysfs, Some(70))
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
    fn featured_name_explicit_hidden_falls_back_to_first_connected_by_name() {
        // "gamepad" is hidden, so the explicit choice is ignored.
        let mut cfg = cfg_with_primary(Some("gamepad"));
        cfg.hidden_devices = vec!["gamepad".to_string()];
        let state = make_state(vec![
            (make_info("mouse"), Some(make_reading(80))),
            (make_info("keyboard"), Some(make_reading(50))),
            (make_info("gamepad"), Some(make_reading(30))),
        ]);
        assert_eq!(
            featured_name(&state, &cfg, Instant::now()),
            Some("keyboard".to_string())
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
            Some("keyboard".to_string())
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

    fn saved(_: &Config) -> anyhow::Result<()> {
        Ok(())
    }

    fn unsavable(_: &Config) -> anyhow::Result<()> {
        anyhow::bail!("read-only file system")
    }

    fn tray_with(
        key: Option<DeviceId>,
        state: TrayState,
        cfg: Config,
        save_config: SaveConfig,
    ) -> RigbatTray {
        let mut cfg = cfg;
        cfg.language.get_or_insert_with(|| "en".to_owned());
        RigbatTray {
            key,
            rx: watch::channel(state).1,
            theme_rx: watch::channel(ColorScheme::Dark).1,
            config: watch::channel(cfg).0,
            save_config,
            renderer: Box::new(TinySkiaRenderer::default()),
            refresh: RefreshSignal::new(),
        }
    }

    fn tray_for(key: Option<DeviceId>, state: TrayState) -> RigbatTray {
        tray_with(key, state, Config::default(), saved)
    }

    fn device(
        name: &str,
        kind: DeviceKind,
        reading: BatteryReading,
        estimate: crate::domain::Estimate,
    ) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                kind,
                ..make_info(name)
            },
            last_reading: Some(reading),
            last_seen: Some(Instant::now()),
            presence: Presence::Online,
            estimate,
        }
    }

    /// One device per shape a row takes.
    fn every_row_shape() -> TrayState {
        use crate::domain::Estimate;
        let mut keyboard = retained("NuPhy", 88, Duration::from_secs(2 * 3600));
        keyboard.info.kind = DeviceKind::Keyboard;
        TrayState {
            devices: vec![
                device(
                    "MX_Master",
                    DeviceKind::Mouse,
                    make_reading(62),
                    Estimate::Remaining(Duration::from_secs(3 * 3600)),
                ),
                device(
                    "Ear",
                    DeviceKind::Headset,
                    BatteryReading::new(40, ChargeState::Charging),
                    Estimate::Charging,
                ),
                device(
                    "Pad",
                    DeviceKind::Controller,
                    BatteryReading::new(100, ChargeState::Full),
                    Estimate::Unknown,
                ),
                device(
                    "Aerox",
                    DeviceKind::Mouse,
                    make_reading(15),
                    Estimate::Unknown,
                ),
                keyboard,
                no_access("mouse"),
            ],
        }
    }

    /// The menu as the host receives it: `[x]`/`[ ]` a checkmark item and its
    /// state, `#` the icon name, `---` a separator.
    fn describe(tray: &RigbatTray) -> Vec<String> {
        let line = |mark: String, label: String, enabled: bool, icon: String| {
            let mut line = format!("{mark}{label}");
            if !enabled {
                line.insert_str(0, "(disabled) ");
            }
            if !icon.is_empty() {
                line.push_str(&format!(" #{icon}"));
            }
            line
        };
        tray.menu()
            .into_iter()
            .map(|item| match item {
                MenuItem::Standard(i) => line(String::new(), i.label, i.enabled, i.icon_name),
                MenuItem::Checkmark(i) => {
                    let mark = if i.checked { "[x] " } else { "[ ] " };
                    line(mark.to_owned(), i.label, i.enabled, i.icon_name)
                }
                MenuItem::Separator => "---".to_owned(),
                _ => "unexpected item kind".to_owned(),
            })
            .collect()
    }

    fn click(tray: &mut RigbatTray, label: &str) {
        let activate = tray
            .menu()
            .into_iter()
            .find_map(|item| match item {
                MenuItem::Checkmark(i) if i.label == label => Some(i.activate),
                _ => None,
            })
            .expect("no checkmark item with that label");
        activate(tray);
    }

    const TAIL: [&str; 6] = [
        "---",
        "Device overview…",
        "Refresh",
        "Settings…",
        "---",
        "Quit",
    ];

    fn with_tail(rows: &[&str]) -> Vec<String> {
        rows.iter().chain(&TAIL).map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn single_icon_menu_offers_automatic_then_every_device_as_a_checkmark() {
        let tray = tray_for(None, every_row_shape());
        assert_eq!(
            describe(&tray),
            with_tail(&[
                "[x] Automatic",
                "[ ] Aerox: ⚠ 15% #input-mouse",
                "[ ] Ear: ⚡ 40% #audio-headset",
                "[ ] MX__Master: 62% · ~3h left #input-mouse",
                "[ ] Pad: 100% · full #input-gaming",
                "[ ] mouse: No access · run rigbat doctor #input-mouse",
                "[ ] NuPhy: Unreachable · last reading 2h ago #input-keyboard",
            ])
        );
    }

    #[test]
    fn single_icon_menu_checks_the_pinned_device_not_automatic() {
        let cfg = cfg_with_primary(Some("Ear"));
        let tray = tray_with(None, every_row_shape(), cfg, saved);
        let checked: Vec<String> = describe(&tray)
            .into_iter()
            .filter(|l| l.starts_with("[x]"))
            .collect();
        assert_eq!(checked, ["[x] Ear: ⚡ 40% #audio-headset"]);
        assert!(describe(&tray).contains(&"[ ] Automatic".to_owned()));
    }

    /// The icon falls back to the first connected device, but the user did
    /// not choose it, so nothing claims the choice.
    #[test]
    fn a_pin_to_a_device_that_is_not_shown_checks_nothing() {
        let cfg = cfg_with_primary(Some("gone"));
        let tray = tray_with(None, every_row_shape(), cfg, saved);
        assert!(!describe(&tray).iter().any(|l| l.starts_with("[x]")));
    }

    #[test]
    fn a_pin_shared_by_two_transports_checks_the_one_the_icon_shows() {
        let cfg = cfg_with_primary(Some("MX"));
        let tray = tray_with(None, same_name_two_transports(Presence::Online), cfg, saved);
        assert_eq!(
            describe(&tray),
            with_tail(&[
                "[ ] Automatic",
                "[x] MX: 40% #input-mouse",
                "[ ] MX: Unreachable · last reading just now #input-mouse",
            ])
        );
    }

    #[test]
    fn per_device_menu_rows_are_enabled_plain_items() {
        let cfg = Config {
            tray_mode: TrayMode::PerDevice,
            ..Config::default()
        };
        let tray = tray_with(Some(key("Aerox")), every_row_shape(), cfg, saved);
        assert_eq!(
            describe(&tray),
            with_tail(&[
                "Aerox: ⚠ 15% #input-mouse",
                "Ear: ⚡ 40% #audio-headset",
                "MX__Master: 62% · ~3h left #input-mouse",
                "Pad: 100% · full #input-gaming",
                "mouse: No access · run rigbat doctor #input-mouse",
                "NuPhy: Unreachable · last reading 2h ago #input-keyboard",
            ])
        );
    }

    #[test]
    fn no_devices_menu_says_so_in_either_mode() {
        for tray_mode in [TrayMode::PrimaryOnly, TrayMode::PerDevice] {
            let cfg = Config {
                tray_mode,
                ..Config::default()
            };
            let tray = tray_with(None, make_state(vec![]), cfg, saved);
            assert_eq!(describe(&tray), with_tail(&["(disabled) No devices"]));
        }
    }

    #[test]
    fn menu_follows_the_configured_language() {
        let cfg = Config {
            language: Some("ru".to_owned()),
            ..Config::default()
        };
        let tray = tray_with(None, every_row_shape(), cfg, saved);
        let menu = describe(&tray);
        assert_eq!(menu[0], "[x] Автоматически");
        assert!(menu.contains(&"[ ] Pad: 100% · заряжено #input-gaming".to_owned()));
        assert!(menu.contains(&"Обзор устройств…".to_owned()));
    }

    #[test]
    fn clicking_a_device_pins_it_and_automatic_clears_the_pin() {
        let mut tray = tray_for(None, every_row_shape());

        click(&mut tray, "Ear: ⚡ 40%");
        assert_eq!(tray.config.borrow().primary_device.as_deref(), Some("Ear"));
        assert_eq!(tray.title(), "Ear");

        click(&mut tray, "MX__Master: 62% · ~3h left");
        assert_eq!(
            tray.config.borrow().primary_device.as_deref(),
            Some("MX_Master")
        );

        click(&mut tray, "Automatic");
        assert_eq!(tray.config.borrow().primary_device, None);
    }

    #[test]
    fn a_pin_that_cannot_be_saved_changes_nothing() {
        let mut tray = tray_with(None, every_row_shape(), Config::default(), unsavable);
        click(&mut tray, "Ear: ⚡ 40%");
        assert_eq!(tray.config.borrow().primary_device, None);
    }

    #[test]
    fn menu_and_tooltip_say_no_access_and_point_at_doctor() {
        let state = TrayState {
            devices: vec![no_access("mouse")],
        };
        let tray = tray_for(Some(key("mouse")), state);
        let expected = "mouse: No access · run rigbat doctor";

        assert_eq!(tray.tool_tip().title, expected);
        assert!(
            describe(&tray).contains(&format!("[ ] {expected} #input-mouse")),
            "{:?}",
            describe(&tray)
        );
    }

    mod bus {
        use std::collections::{BTreeSet, HashMap};
        use std::time::Duration;

        use zbus::zvariant::{OwnedValue, Value};

        use super::super::run;
        use super::{
            ColorScheme, Config, RefreshSignal, Transport, TrayMode, make_info, make_reading,
            make_state, saved, sni_id, watch,
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

        async fn serve_fake_watcher() -> zbus::Connection {
            zbus::connection::Builder::session()
                .expect("private bus")
                .name("org.kde.StatusNotifierWatcher")
                .expect("name")
                .serve_at("/StatusNotifierWatcher", FakeWatcher)
                .expect("path")
                .build()
                .await
                .expect("fake watcher")
        }

        async fn item_property(
            conn: &zbus::Connection,
            bus_name: &str,
            property: &str,
        ) -> Option<String> {
            let reply = conn
                .call_method(
                    Some(bus_name),
                    "/StatusNotifierItem",
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &("org.kde.StatusNotifierItem", property),
                )
                .await
                .ok()?;
            let value: OwnedValue = reply.body().deserialize().ok()?;
            String::try_from(value).ok()
        }

        /// The bus name of the item whose SNI `Id` is `id`.
        async fn item_named(conn: &zbus::Connection, id: &str) -> Option<String> {
            let dbus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
            for name in dbus.list_names().await.ok()? {
                if name.starts_with("org.kde.StatusNotifierItem-")
                    && item_property(conn, name.as_str(), "Id").await.as_deref() == Some(id)
                {
                    return Some(name.to_string());
                }
            }
            None
        }

        type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

        /// Top-level menu items as (id, label, toggle-state), read the way a host reads them.
        async fn menu(conn: &zbus::Connection, bus_name: &str) -> Vec<(i32, String, i32)> {
            let reply = conn
                .call_method(
                    Some(bus_name),
                    "/MenuBar",
                    Some("com.canonical.dbusmenu"),
                    "GetLayout",
                    &(0i32, -1i32, Vec::<String>::new()),
                )
                .await
                .expect("GetLayout");
            let (_revision, (_root, _props, children)): (u32, Layout) =
                reply.body().deserialize().expect("layout");
            children
                .into_iter()
                .map(|child| {
                    let (id, props, _): Layout = child.try_into().expect("menu item");
                    let text = |key: &str| {
                        props
                            .get(key)
                            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
                            .unwrap_or_default()
                    };
                    let toggle = props
                        .get("toggle-state")
                        .and_then(|v| i32::try_from(v).ok())
                        .unwrap_or(-1);
                    (id, text("label"), toggle)
                })
                .collect()
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
            let _watcher = serve_fake_watcher().await;

            let sysfs = make_info("mouse");
            let mut bluetooth = make_info("mouse");
            bluetooth.transport = Transport::Bluetooth;
            let (state_tx, state_rx) = watch::channel(make_state(vec![
                (sysfs.clone(), Some(make_reading(80))),
                (bluetooth.clone(), Some(make_reading(40))),
            ]));
            let (_theme_tx, theme_rx) = watch::channel(ColorScheme::Dark);
            let (config_tx, _config_rx) = watch::channel(Config {
                tray_mode: TrayMode::PerDevice,
                ..Config::default()
            });
            tokio::spawn(run(
                state_rx,
                theme_rx,
                config_tx.clone(),
                saved,
                RefreshSignal::new(),
            ));

            let client = zbus::Connection::session().await.expect("private bus");
            let sysfs_id = sni_id(&sysfs.id());
            let bluetooth_id = sni_id(&bluetooth.id());
            until_ids(&client, &[&sysfs_id, &bluetooth_id]).await;

            state_tx.send_replace(make_state(vec![(sysfs, Some(make_reading(80)))]));
            until_ids(&client, &[&sysfs_id]).await;

            config_tx.send_modify(|c| c.tray_mode = TrayMode::PrimaryOnly);
            until_ids(&client, &["rigbat"]).await;
        }

        /// A click on a device row, sent as a host sends it, pins the device:
        /// the config channel carries it and the aggregate icon re-publishes.
        #[tokio::test]
        async fn a_menu_click_pins_the_device_the_single_icon_shows() {
            if !isolated(
                module_path!(),
                "a_menu_click_pins_the_device_the_single_icon_shows",
            ) {
                return;
            }
            let _watcher = serve_fake_watcher().await;

            let mut keyboard = make_info("keyboard");
            keyboard.kind = crate::domain::DeviceKind::Keyboard;
            let (_state_tx, state_rx) = watch::channel(make_state(vec![
                (make_info("mouse"), Some(make_reading(80))),
                (keyboard, Some(make_reading(50))),
            ]));
            let (_theme_tx, theme_rx) = watch::channel(ColorScheme::Dark);
            let (config_tx, _config_rx) = watch::channel(Config {
                language: Some("en".to_owned()),
                ..Config::default()
            });
            tokio::spawn(run(
                state_rx,
                theme_rx,
                config_tx.clone(),
                saved,
                RefreshSignal::new(),
            ));

            let client = zbus::Connection::session().await.expect("private bus");
            let client = &client;
            let item = eventually(
                TIMEOUT,
                || async move { item_named(client, "rigbat").await },
            )
            .await;
            let item = item.as_str();
            let title = || async move { item_property(client, item, "Title").await };
            assert_eq!(title().await.as_deref(), Some("keyboard"));

            let rows = menu(client, item).await;
            let rows: Vec<(&str, i32)> = rows.iter().map(|(_, l, t)| (l.as_str(), *t)).collect();
            assert_eq!(
                &rows[..3],
                [("Automatic", 1), ("keyboard: 50%", 0), ("mouse: 80%", 0)]
            );

            let mouse_id = menu(client, item)
                .await
                .into_iter()
                .find(|(_, label, _)| label == "mouse: 80%")
                .map(|(id, _, _)| id)
                .expect("mouse row");
            client
                .call_method(
                    Some(item),
                    "/MenuBar",
                    Some("com.canonical.dbusmenu"),
                    "Event",
                    &(mouse_id, "clicked", Value::from(0i32), 0u32),
                )
                .await
                .expect("Event");

            eventually(TIMEOUT, || async move {
                (title().await.as_deref() == Some("mouse")).then_some(())
            })
            .await;
            assert_eq!(config_tx.borrow().primary_device.as_deref(), Some("mouse"));
            let rows = menu(client, item).await;
            let toggles: Vec<i32> = rows.iter().take(3).map(|(_, _, t)| *t).collect();
            assert_eq!(toggles, [0, 0, 1]);
        }
    }
}
