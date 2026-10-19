use std::collections::HashMap;

use ksni::TrayMethods;
use tokio::sync::watch;

use super::item::{RigbatTray, SaveConfig, View};
use super::resolve::visible;
use crate::appearance::ColorScheme;
use crate::config::Config;
use crate::domain::{BootTime, DeviceId, TrayMode, TrayState};
use crate::icon::{IconCache, TinySkiaRenderer};
use crate::refresh::RefreshSignal;

/// Returns the list of tray icon keys for the given mode and shown devices.
/// `None` is the aggregate/primary icon; `Some(id)` is a per-device icon.
fn desired_keys(mode: TrayMode, shown: &[DeviceId]) -> Vec<Option<DeviceId>> {
    match (mode, shown.is_empty()) {
        // No devices → always one "No devices" aggregate icon.
        (_, true) => vec![None],
        (TrayMode::PrimaryOnly, _) => vec![None],
        (TrayMode::PerDevice, _) => shown.iter().map(|n| Some(n.clone())).collect(),
    }
}

/// The tray-visible devices, in roster order, one entry per `DeviceId`.
fn shown_ids(state: &TrayState, cfg: &Config, now: BootTime) -> Vec<DeviceId> {
    let mut shown: Vec<DeviceId> = Vec::new();
    for d in visible(state, cfg, now).devices() {
        let id = d.info.id();
        if !shown.contains(&id) {
            shown.push(id);
        }
    }
    shown
}

struct Item {
    handle: ksni::Handle<RigbatTray>,
    view: View,
}

struct Items {
    live: HashMap<Option<DeviceId>, Item>,
    icons: IconCache,
}

impl Items {
    fn new() -> Self {
        Self {
            live: HashMap::new(),
            icons: IconCache::new(Box::new(TinySkiaRenderer::default())),
        }
    }
}

/// Spawns, retires and updates SNI items so they match the current inputs.
/// An item whose view is unchanged is not touched. Returns how many items
/// were spawned or updated.
async fn reconcile(
    items: &mut Items,
    rx: &watch::Receiver<TrayState>,
    theme_rx: &watch::Receiver<ColorScheme>,
    config: &watch::Sender<Config>,
    save_config: SaveConfig,
    refresh: &RefreshSignal,
) -> usize {
    let now = crate::clock::now();
    // Computed without holding any watch::Ref across an await.
    let desired: Vec<(Option<DeviceId>, View)> = {
        let state = rx.borrow();
        let cfg = config.borrow();
        let scheme = *theme_rx.borrow();
        desired_keys(cfg.tray_mode, &shown_ids(&state, &cfg, now))
            .into_iter()
            .map(|key| {
                let view = View::new(key.as_ref(), &state, &cfg, scheme, now);
                (key, view)
            })
            .collect()
    };
    let prev_len = items.live.len();
    retire_missing(items, &desired).await;
    let mut touched = 0;
    for (key, view) in desired {
        if let Some(item) = items.live.get_mut(&key) {
            match try_update(item, &key, &view, &mut items.icons).await {
                Update::Unchanged => continue,
                Update::Updated => {
                    touched += 1;
                    continue;
                }
                Update::Respawn => {
                    items.live.remove(&key);
                }
            }
        }
        if spawn_item(items, key, view, config, save_config, refresh).await {
            touched += 1;
        }
    }

    let live = &items.live;
    items
        .icons
        .retain(|icon| live.values().any(|item| item.view.icon == *icon));

    if items.live.len() != prev_len {
        tracing::info!("showing {} tray icon(s)", items.live.len());
    }
    touched
}

async fn retire_missing(items: &mut Items, desired: &[(Option<DeviceId>, View)]) {
    let to_remove: Vec<Option<DeviceId>> = items
        .live
        .keys()
        .filter(|k| !desired.iter().any(|(key, _)| key == *k))
        .cloned()
        .collect();
    for key in to_remove {
        if let Some(item) = items.live.remove(&key) {
            // ksni 0.3.4 keeps the SNI item alive on drop; shutdown() unregisters it.
            item.handle.shutdown().await;
        }
    }
}

enum Update {
    Unchanged,
    Updated,
    Respawn,
}

async fn try_update(
    item: &mut Item,
    key: &Option<DeviceId>,
    view: &View,
    icons: &mut IconCache,
) -> Update {
    if item.handle.is_closed() {
        tracing::warn!("tray icon for {key:?} stopped serving; recreating it");
        return Update::Respawn;
    }
    if item.view == *view {
        return Update::Unchanged;
    }
    let icon = icons.icons(&view.icon);
    let next = view.clone();
    let updated = item
        .handle
        .update(move |tray| {
            tray.view = next;
            tray.icon = icon;
        })
        .await;
    if updated.is_none() {
        tracing::warn!("tray icon for {key:?} did not take an update; recreating it");
        return Update::Respawn;
    }
    item.view = view.clone();
    Update::Updated
}

async fn spawn_item(
    items: &mut Items,
    key: Option<DeviceId>,
    view: View,
    config: &watch::Sender<Config>,
    save_config: SaveConfig,
    refresh: &RefreshSignal,
) -> bool {
    let tray = RigbatTray {
        key: key.clone(),
        icon: items.icons.icons(&view.icon),
        view: view.clone(),
        config: config.clone(),
        save_config,
        refresh: refresh.clone(),
    };
    match tray.assume_sni_available(true).spawn().await {
        Ok(handle) => {
            items.live.insert(key, Item { handle, view });
            true
        }
        Err(e) => {
            tracing::error!("failed to spawn tray icon for {key:?}: {e}");
            false
        }
    }
}

pub async fn run(
    mut rx: watch::Receiver<TrayState>,
    mut theme_rx: watch::Receiver<ColorScheme>,
    config: watch::Sender<Config>,
    save_config: SaveConfig,
    refresh: RefreshSignal,
) {
    let mut items = Items::new();
    let mut config_rx = config.subscribe();
    // Views change with time alone: text that ages ("2h ago") and the offline shelf life.
    let mut age_tick = crate::clock::age_ticker();

    loop {
        reconcile(&mut items, &rx, &theme_rx, &config, save_config, &refresh).await;

        tokio::select! {
            r = rx.changed() => if r.is_err() { break; },
            r = theme_rx.changed() => if r.is_err() { break; },
            r = config_rx.changed() => if r.is_err() { break; },
            _ = age_tick.tick() => {}
        }
    }
}

#[cfg(test)]
mod tests {

    use super::{desired_keys, shown_ids};
    use crate::config::Config;
    use crate::domain::{DeviceInfo, Presence, Transport, TrayMode};
    use crate::tray::fixtures::{key, retained, same_name_two_transports};
    use crate::tray::item::sni_id;
    use crate::tray::resolve::{resolve_for, visible};

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

    #[test]
    fn an_offline_device_keeps_its_icon_for_the_configured_period() {
        let state = crate::domain::TrayState {
            devices: vec![retained(
                "keys",
                88,
                std::time::Duration::from_secs(3 * 3600),
            )],
        };
        let now = crate::clock::now();
        assert!(shown_ids(&state, &Config::default(), now).is_empty());
        let six_hours = Config {
            hide_offline_after_mins: 360,
            ..Config::default()
        };
        assert_eq!(shown_ids(&state, &six_hours, now), [key("keys")]);
    }

    #[test]
    fn same_name_over_two_transports_gets_two_icons_each_resolving_to_its_own_reading() {
        let state = same_name_two_transports(Presence::Online);
        let cfg = Config {
            tray_mode: TrayMode::PerDevice,
            ..Config::default()
        };
        let now = crate::clock::now();
        let keys = desired_keys(cfg.tray_mode, &shown_ids(&state, &cfg, now));
        assert_eq!(keys.len(), 2, "two icons, not one: {keys:?}");

        let percents: Vec<(Transport, Option<u8>)> = keys
            .iter()
            .map(|k| {
                let r = resolve_for(k.as_ref(), &visible(&state, &cfg, now), &cfg).unwrap();
                (
                    r.device.info.transport,
                    r.device.last_reading.map(|r| r.percent),
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

    mod bus {
        use std::collections::{BTreeSet, HashMap};
        use std::time::Duration;

        use zbus::zvariant::{OwnedValue, Value};

        use tokio::sync::watch;

        use super::super::{Items, reconcile, run};
        use super::{Config, DeviceInfo, Transport, TrayMode, sni_id};
        use crate::appearance::ColorScheme;
        use crate::bus_test::{eventually, isolated};
        use crate::refresh::RefreshSignal;
        use crate::tray::fixtures::{make_reading, make_state, saved};

        const TIMEOUT: Duration = Duration::from_secs(5);

        async fn serve_fake_watcher() -> zbus::Connection {
            let (registered, _) = tokio::sync::mpsc::unbounded_channel();
            zbus::connection::Builder::session()
                .expect("private bus")
                .name("org.kde.StatusNotifierWatcher")
                .expect("name")
                .serve_at("/StatusNotifierWatcher", RecordingWatcher(registered))
                .expect("path")
                .build()
                .await
                .expect("fake watcher")
        }

        struct RecordingWatcher(tokio::sync::mpsc::UnboundedSender<String>);

        #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
        impl RecordingWatcher {
            fn register_status_notifier_item(&self, service: &str) {
                let _ = self.0.send(service.to_owned());
            }

            #[zbus(property)]
            fn is_status_notifier_host_registered(&self) -> bool {
                true
            }
        }

        /// At login the tray often starts before the panel's watcher: the
        /// icon must register as soon as the watcher appears, not on the
        /// next state change or age tick.
        #[tokio::test]
        async fn an_icon_spawned_before_the_watcher_registers_when_it_appears() {
            if !isolated(
                module_path!(),
                "an_icon_spawned_before_the_watcher_registers_when_it_appears",
            ) {
                return;
            }
            let (_state_tx, state_rx) = watch::channel(make_state(vec![(
                DeviceInfo::test("mouse"),
                Some(make_reading(80)),
            )]));
            let (_theme_tx, theme_rx) = watch::channel(ColorScheme::Dark);
            let (config_tx, _config_rx) = watch::channel(Config::default());
            tokio::spawn(run(
                state_rx,
                theme_rx,
                config_tx,
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

            let (registered_tx, mut registered) = tokio::sync::mpsc::unbounded_channel();
            let _watcher = zbus::connection::Builder::session()
                .expect("private bus")
                .serve_at("/StatusNotifierWatcher", RecordingWatcher(registered_tx))
                .expect("path")
                .name("org.kde.StatusNotifierWatcher")
                .expect("name")
                .build()
                .await
                .expect("late watcher");
            let service = tokio::time::timeout(TIMEOUT, registered.recv())
                .await
                .expect("the icon never registered with the late watcher")
                .expect("watcher alive");
            assert_eq!(service, item);
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

        async fn items(conn: &zbus::Connection) -> Vec<(String, Option<String>)> {
            let dbus = zbus::fdo::DBusProxy::new(conn).await.expect("DBus proxy");
            let mut items = Vec::new();
            for name in dbus.list_names().await.expect("ListNames") {
                if name.starts_with("org.kde.StatusNotifierItem-") {
                    let id = item_property(conn, name.as_str(), "Id").await;
                    items.push((name.to_string(), id));
                }
            }
            items
        }

        /// The bus name of the item whose SNI `Id` is `id`.
        async fn item_named(conn: &zbus::Connection, id: &str) -> Option<String> {
            items(conn)
                .await
                .into_iter()
                .find(|(_, item_id)| item_id.as_deref() == Some(id))
                .map(|(name, _)| name)
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
            items(conn)
                .await
                .into_iter()
                .filter_map(|(_, id)| id)
                .collect()
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

            let sysfs = DeviceInfo::test("mouse");
            let mut bluetooth = DeviceInfo::test("mouse");
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

            let mut keyboard = DeviceInfo::test("keyboard");
            keyboard.kind = crate::domain::DeviceKind::Keyboard;
            let (_state_tx, state_rx) = watch::channel(make_state(vec![
                (DeviceInfo::test("mouse"), Some(make_reading(80))),
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

        /// An item whose SNI service ended is spawned again, not left dead.
        #[tokio::test]
        async fn a_dead_item_is_recreated() {
            if !isolated(module_path!(), "a_dead_item_is_recreated") {
                return;
            }
            let _watcher = serve_fake_watcher().await;
            let client = zbus::Connection::session().await.expect("private bus");

            let mouse = DeviceInfo::test("mouse");
            let state = |percent| make_state(vec![(mouse.clone(), Some(make_reading(percent)))]);
            let (state_tx, state_rx) = watch::channel(state(80));
            let (_theme_tx, theme_rx) = watch::channel(ColorScheme::Dark);
            let (config_tx, _config_rx) = watch::channel(Config {
                tray_mode: TrayMode::PerDevice,
                ..Config::default()
            });
            let refresh = RefreshSignal::new();
            let mut items = Items::new();
            let mouse_id = sni_id(&mouse.id());

            reconcile(
                &mut items, &state_rx, &theme_rx, &config_tx, saved, &refresh,
            )
            .await;
            until_ids(&client, &[&mouse_id]).await;

            items.live[&Some(mouse.id())].handle.shutdown().await;
            until_ids(&client, &[]).await;

            state_tx.send_replace(state(79));
            reconcile(
                &mut items, &state_rx, &theme_rx, &config_tx, saved, &refresh,
            )
            .await;
            until_ids(&client, &[&mouse_id]).await;

            items.live[&Some(mouse.id())].handle.shutdown().await;
            until_ids(&client, &[]).await;
            reconcile(
                &mut items, &state_rx, &theme_rx, &config_tx, saved, &refresh,
            )
            .await;
            until_ids(&client, &[&mouse_id]).await;
        }

        /// A republished but unchanged state touches no item; a change
        /// updates exactly the items that show it.
        #[tokio::test]
        async fn only_items_whose_view_changed_are_updated() {
            if !isolated(module_path!(), "only_items_whose_view_changed_are_updated") {
                return;
            }
            let _watcher = serve_fake_watcher().await;

            let (mouse, pad) = (DeviceInfo::test("mouse"), DeviceInfo::test("pad"));
            let state = |mouse_percent| {
                make_state(vec![
                    (mouse.clone(), Some(make_reading(mouse_percent))),
                    (pad.clone(), Some(make_reading(50))),
                ])
            };
            let (state_tx, state_rx) = watch::channel(state(80));
            let (theme_tx, theme_rx) = watch::channel(ColorScheme::Dark);
            let (config_tx, _config_rx) = watch::channel(Config {
                tray_mode: TrayMode::PerDevice,
                language: Some("en".to_owned()),
                ..Config::default()
            });
            let refresh = RefreshSignal::new();
            let mut items = Items::new();
            let mut sync = async || {
                reconcile(
                    &mut items, &state_rx, &theme_rx, &config_tx, saved, &refresh,
                )
                .await
            };

            assert_eq!(sync().await, 2, "both icons spawn");
            assert_eq!(sync().await, 0, "nothing changed");

            // A fresh poll with the same percent: only `last_seen` moves.
            state_tx.send_replace(state(80));
            assert_eq!(sync().await, 0, "a new timestamp renders nothing new");

            state_tx.send_replace(state(79));
            assert_eq!(sync().await, 2, "both menus list the mouse");

            theme_tx.send_replace(ColorScheme::Light);
            assert_eq!(sync().await, 2);
            assert_eq!(sync().await, 0);
        }
    }
}
