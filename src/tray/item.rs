use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{MenuItem, ToolTip, Tray};
use tokio::sync::watch;

use super::launch::launch;
use super::resolve::{featured_id, resolve_for, visible};
use crate::appearance::ColorScheme;
use crate::config::Config;
use crate::domain::{
    BootTime, DeviceId, PrimaryStatus, TrayMode, TrayState, device_line, device_status,
    freedesktop_icon_name,
};
use crate::i18n::{Lang, fl, loader};
use crate::icon::{IconKey, Theme};
use crate::refresh::RefreshSignal;

// ---------------------------------------------------------------------------
// sni_id: a stable, unique SNI item id per device
// ---------------------------------------------------------------------------

/// Builds the SNI item id for a per-device icon.
///
/// The readable stem (name, transport) flattens every character outside
/// `[A-Za-z0-9]` to `-` and omits the locator, so it collides on its own; the
/// suffix hashes the whole `DeviceId`. Hosts key remembered position on this
/// id, and every `DeviceId` field survives a restart, so the id does too.
pub(super) fn sni_id(id: &DeviceId) -> String {
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

// ---------------------------------------------------------------------------
// RigbatTray — unified SNI item for both aggregate and per-device icons
// ---------------------------------------------------------------------------

/// Writes `config.json`; a parameter so tests never touch the real file.
pub type SaveConfig = fn(&Config) -> anyhow::Result<()>;

/// Serves a `View` the manager computed; ksni reads it on every `update`.
pub struct RigbatTray {
    /// `Some(id)` = per-device icon; `None` = aggregate/primary icon.
    pub key: Option<DeviceId>,
    pub view: View,
    /// `view.icon`, rendered.
    pub icon: Vec<ksni::Icon>,
    pub config: watch::Sender<Config>,
    pub save_config: SaveConfig,
    pub refresh: RefreshSignal,
}

/// Everything one icon shows. Text that ages ("2h ago") is part of it, so a
/// view computed later can differ from an earlier one for the same state.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    title: String,
    pub icon: IconKey,
    tool_tip: String,
    rows: Vec<MenuRow>,
    mode: TrayMode,
    automatic: bool,
    lang: Lang,
}

#[derive(Debug, Clone, PartialEq)]
struct MenuRow {
    name: String,
    label: String,
    icon_name: String,
    pinned: bool,
}

impl View {
    pub fn new(
        key: Option<&DeviceId>,
        state: &TrayState,
        cfg: &Config,
        scheme: ColorScheme,
        now: BootTime,
    ) -> Self {
        let lang = cfg.lang();
        let l = loader(lang);
        // `None` when the icon has nothing to show: no devices, or the keyed one is gone or hidden.
        let resolved = resolve_for(key, state, cfg, now);
        let title = match (key, &resolved) {
            (Some(k), _) => k.name.clone(),
            (None, Some(r)) => r.state.info.name.clone(),
            (None, None) => "rigbat".to_owned(),
        };
        let tool_tip = match (&resolved, key) {
            (Some(r), _) => device_line(&r.state, r.status, now, lang),
            (None, Some(id)) => fl!(l, "entry-offline", name = id.name.as_str()),
            (None, None) => fl!(l, "tray-no-devices"),
        };
        let icon = IconKey {
            status: resolved
                .as_ref()
                .map_or(PrimaryStatus::Offline, |r| r.status),
            kind: resolved.as_ref().map(|r| r.state.info.kind),
            theme: Theme::new(cfg.palette, scheme),
            mode: cfg.display_mode,
            stale: resolved.as_ref().is_some_and(|r| r.stale),
        };
        let pinned = featured_id(state, cfg, now)
            .filter(|id| cfg.primary_device.as_deref() == Some(id.name.as_str()));
        let rows = visible(state, cfg, now)
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
        Self {
            title,
            icon,
            tool_tip,
            rows,
            mode: cfg.tray_mode,
            automatic: cfg.primary_device.is_none(),
            lang,
        }
    }
}

impl RigbatTray {
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
        self.view.title.clone()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.view.tool_tip.clone(),
            ..ToolTip::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let View {
            rows,
            mode,
            automatic,
            lang,
            ..
        } = self.view.clone();
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{MenuItem, RigbatTray, SaveConfig, Tray as _, View, sni_id, watch};
    use crate::appearance::ColorScheme;
    use crate::config::Config;
    use crate::domain::{
        BatteryReading, BootTime, ChargeState, DeviceId, DeviceInfo, DeviceKind, DeviceState,
        Palette, Presence, Transport, TrayMode, TrayState,
    };
    use crate::refresh::RefreshSignal;
    use crate::tray::fixtures::{
        cfg_with_primary, id, key, make_info, make_reading, make_state, no_access, retained,
        same_name_two_transports, saved,
    };

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
            view: View::new(
                key.as_ref(),
                &state,
                &cfg,
                ColorScheme::Dark,
                crate::clock::now(),
            ),
            key,
            icon: Vec::new(),
            config: watch::channel(cfg).0,
            save_config,
            refresh: RefreshSignal::new(),
        }
    }

    /// What the manager does on the config change a click publishes.
    fn rerender(tray: &mut RigbatTray, state: &TrayState) {
        tray.view = View::new(
            tray.key.as_ref(),
            state,
            &tray.config.borrow(),
            ColorScheme::Dark,
            crate::clock::now(),
        );
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
            last_seen: Some(crate::clock::now()),
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
                    Estimate::Unknown,
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

    /// The icon falls back to the automatic choice, but the user did
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
    fn the_icon_key_follows_the_palette() {
        let state = every_row_shape();
        let key = |palette| {
            let cfg = Config {
                palette,
                ..Config::default()
            };
            tray_with(None, state.clone(), cfg, saved).view.icon
        };
        let catppuccin = key(Palette::Catppuccin);
        let nord = key(Palette::Nord);
        assert_ne!(catppuccin, nord);
        assert_eq!(
            catppuccin.theme,
            crate::icon::Theme::new(Palette::Catppuccin, ColorScheme::Dark)
        );
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
        let state = every_row_shape();
        let mut tray = tray_for(None, state.clone());

        click(&mut tray, "Ear: ⚡ 40%");
        assert_eq!(tray.config.borrow().primary_device.as_deref(), Some("Ear"));
        rerender(&mut tray, &state);
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

    /// Unchanged state still renders new text once an age crosses a unit;
    /// a live reading's line does not age.
    #[test]
    fn a_view_changes_with_time_only_where_text_ages() {
        let state = TrayState {
            devices: vec![
                retained("NuPhy", 88, Duration::from_secs(30 * 60)),
                device(
                    "Pad",
                    DeviceKind::Controller,
                    make_reading(70),
                    crate::domain::Estimate::Unknown,
                ),
            ],
        };
        let cfg = Config {
            language: Some("en".to_owned()),
            tray_mode: TrayMode::PerDevice,
            ..Config::default()
        };
        let now = crate::clock::now();
        let since_boot = now.saturating_duration_since(BootTime::from_boot(Duration::ZERO));
        let later = BootTime::from_boot(since_boot + Duration::from_secs(3600));
        let key_of = |name: &str| {
            state
                .devices
                .iter()
                .find(|d| d.info.name == name)
                .expect("device")
                .info
                .id()
        };
        let view =
            |name: &str, at| View::new(Some(&key_of(name)), &state, &cfg, ColorScheme::Dark, at);

        assert_eq!(view("NuPhy", now), view("NuPhy", now));
        assert_ne!(view("NuPhy", now), view("NuPhy", later));
        let (pad_now, pad_later) = (view("Pad", now), view("Pad", later));
        assert_eq!(pad_now.icon, pad_later.icon);
        assert_eq!(pad_now.tool_tip, pad_later.tool_tip);
    }
}
