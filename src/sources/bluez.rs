/// BlueZ Battery1 backend — reads the charge level of Bluetooth devices via D-Bus (zbus).
///
/// Protocol:
/// - Service: `org.bluez`, system bus.
/// - ObjectManager at `/` → `GetManagedObjects` → all objects.
/// - An object is of interest if it has the `org.bluez.Device1` interface with `Connected == true`
///   and the `org.bluez.Battery1` interface is present.
/// - Name: `Device1.Alias` → `Device1.Name` → address from the object path.
/// - Charge: `Battery1.Percentage` (u8).
/// - No charging info in Battery1 → always `ChargeState::Discharging`.
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures_util::StreamExt as _;
use tokio::time::Instant;
use zbus::zvariant::OwnedObjectPath;

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, Transport, guess_kind};
use crate::refresh::RefreshSignal;

use super::supervise::supervise;
use super::{BatteryBackend, BatterySource, Context};

/// `refresh.trigger()` wakes every source task, including the SteelSeries
/// hidraw task, which opens the device and blocks a thread.
/// BlueZ can emit a burst of signals for one physical event (several
/// interfaces added at once when a device connects, `Percentage` updates
/// from several devices in the same second); coalescing avoids hammering
/// unrelated USB HID devices for a reason that has nothing to do with them.
const SIGNAL_DEBOUNCE: Duration = Duration::from_secs(5);

pub struct BluezBackend;

struct BluezSource {
    info: DeviceInfo,
    path: OwnedObjectPath,
    conn: zbus::Connection,
}

/// Selects the device name: Alias → Name → address from the object path.
fn device_name(alias: Option<&str>, name: Option<&str>, addr: &str) -> String {
    alias
        .filter(|s| !s.is_empty())
        .or_else(|| name.filter(|s| !s.is_empty()))
        .unwrap_or(addr)
        .to_owned()
}

#[async_trait::async_trait]
impl BatteryBackend for BluezBackend {
    fn name(&self) -> &'static str {
        "bluez"
    }

    async fn discover(&self, ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        discover_inner(ctx).await
    }
}

async fn discover_inner(ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
    let conn = ctx.system_bus().await?;

    let om = zbus::fdo::ObjectManagerProxy::builder(&conn)
        .destination("org.bluez")
        .context("setting destination")?
        .path("/")
        .context("setting path")?
        .build()
        .await
        .context("building ObjectManagerProxy")?;

    let objects = om
        .get_managed_objects()
        .await
        .context("GetManagedObjects on org.bluez")?;

    let mut sources: Vec<Box<dyn BatterySource>> = Vec::new();

    for (path, interfaces) in objects {
        let Some(device_iface) = interfaces.get("org.bluez.Device1") else {
            continue;
        };

        if !interfaces.contains_key("org.bluez.Battery1") {
            continue;
        }

        // Check Connected property
        let connected = device_iface
            .get("Connected")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);

        if !connected {
            continue;
        }

        let alias = device_iface
            .get("Alias")
            .and_then(|v| String::try_from(v.clone()).ok());

        let name_prop = device_iface
            .get("Name")
            .and_then(|v| String::try_from(v.clone()).ok());

        // Address as a fallback name: the last path component
        // BlueZ encodes the address in the path: /org/bluez/hciX/dev_XX_XX_XX_XX_XX_XX
        let addr_fallback = path
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or("")
            .replace('_', ":");

        // The bare MAC, greppable in bluetoothctl: strip the `dev:` prefix left
        // over from the object-path component `dev_XX_XX_…`. It is both the
        // locator and the name fallback — a device with neither Alias nor Name
        // used to read as `dev:AA:BB:…` in the tray and the device table.
        let locator = addr_fallback
            .strip_prefix("dev:")
            .unwrap_or(&addr_fallback)
            .to_owned();

        let display_name = device_name(alias.as_deref(), name_prop.as_deref(), &locator);

        let info = DeviceInfo {
            kind: guess_kind(&display_name),
            name: display_name,
            transport: Transport::Bluetooth,
            locator: Some(locator),
        };

        sources.push(Box::new(BluezSource {
            info,
            path: path.clone(),
            conn: conn.clone(),
        }));
    }

    Ok(sources)
}

/// Subscribes to BlueZ D-Bus signals and triggers `refresh` when a device is
/// added, removed, or reports a new battery level. Runs under a supervising
/// retry loop with exponential backoff (`sources::supervise`): a lost system
/// bus, a BlueZ that leaves in a way `NameOwnerChanged` cannot recover from,
/// or any other stream ending is not fatal — the loop re-dials through
/// `ctx.system_bus()` (which re-connects a closed one) and resubscribes.
/// Still an optimisation, never a dependency: the supervisor's periodic
/// discovery sweep is the safety net while a watcher is down.
pub fn watch_events(refresh: RefreshSignal, ctx: Arc<Context>) {
    supervise("bluez event watcher", ctx, move |conn| {
        watch_events_inner(refresh.clone(), conn, SIGNAL_DEBOUNCE)
    });
}

/// The live ObjectManager and PropertiesChanged subscriptions. Rebuilt by
/// `subscribe` every time BlueZ reappears under a new unique name, so a
/// restart does not leave the watcher listening for a vanished sender.
struct Subscriptions {
    interfaces_added: zbus::fdo::InterfacesAddedStream,
    interfaces_removed: zbus::fdo::InterfacesRemovedStream,
    properties_changed: zbus::MessageStream,
}

/// Builds the ObjectManager and PropertiesChanged subscriptions against the
/// current BlueZ. Called once at startup and again whenever `NameOwnerChanged`
/// reports a new owner for `org.bluez`.
async fn subscribe(conn: &zbus::Connection) -> anyhow::Result<Subscriptions> {
    let om = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination("org.bluez")
        .context("setting destination")?
        .path("/")
        .context("setting path")?
        .build()
        .await
        .context("building ObjectManagerProxy")?;

    let interfaces_added = om
        .receive_interfaces_added()
        .await
        .context("subscribing to InterfacesAdded")?;
    let interfaces_removed = om
        .receive_interfaces_removed()
        .await
        .context("subscribing to InterfacesRemoved")?;

    // `PropertiesChanged` is emitted per object, at that object's own path
    // (e.g. `/org/bluez/hci0/dev_XX_.../batteryN`), not at a fixed path like
    // ObjectManager's `/`. A typed `PropertiesProxy` must be bound to one
    // path, so watching every device without pre-enumerating them needs a
    // raw match rule instead.
    //
    // The rule filters on the well-known name `org.bluez`, not a resolved
    // unique name: BlueZ restarting (crash, package upgrade, `systemctl
    // restart bluetooth`) gets a new unique name on the bus, and a rule
    // pinned to the old one would go silently quiet with no error. The D-Bus
    // spec does not explicitly promise that a well-known `sender` filter
    // tracks ownership changes, which is why `watch_events_inner` also
    // subscribes to `NameOwnerChanged` and calls `subscribe` again when
    // BlueZ's owner changes, rather than relying on this filter alone.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")
        .context("setting sender filter")?
        .interface("org.freedesktop.DBus.Properties")
        .context("setting interface filter")?
        .member("PropertiesChanged")
        .context("setting member filter")?
        .build();

    let properties_changed = zbus::MessageStream::for_match_rule(rule, conn, None)
        .await
        .context("subscribing to PropertiesChanged")?;

    Ok(Subscriptions {
        interfaces_added,
        interfaces_removed,
        properties_changed,
    })
}

async fn watch_events_inner(
    refresh: RefreshSignal,
    conn: zbus::Connection,
    debounce: Duration,
) -> anyhow::Result<()> {
    let dbus = zbus::fdo::DBusProxy::new(&conn)
        .await
        .context("building DBusProxy")?;
    let mut name_owner_changed = dbus
        .receive_name_owner_changed_with_args(&[(0, "org.bluez")])
        .await
        .context("subscribing to NameOwnerChanged for org.bluez")?;

    let mut subs = subscribe(&conn).await?;

    let mut debouncer = Debouncer::new(debounce);
    let mut deferred: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        tokio::select! {
            _ = async {
                match deferred.as_mut() {
                    Some(sleep) => sleep.await,
                    None => std::future::pending().await,
                }
            } => {
                deferred = None;
                // Only fire if the window really has elapsed. A signal landing
                // right at the boundary takes the immediate path, and this arm
                // then fired a second time for the same burst.
                if debouncer.should_fire(Instant::now()) {
                    refresh.trigger();
                }
            }
            item = subs.interfaces_added.next() => {
                if item.is_none() {
                    tracing::debug!("bluez InterfacesAdded stream ended; watcher will retry");
                    return Ok(());
                }
                fire_or_defer(&refresh, &mut debouncer, &mut deferred);
            }
            item = subs.interfaces_removed.next() => {
                if item.is_none() {
                    tracing::debug!("bluez InterfacesRemoved stream ended; watcher will retry");
                    return Ok(());
                }
                fire_or_defer(&refresh, &mut debouncer, &mut deferred);
            }
            item = subs.properties_changed.next() => {
                let Some(Ok(msg)) = item else {
                    tracing::debug!(
                        "bluez PropertiesChanged stream ended or errored; watcher will retry"
                    );
                    return Ok(());
                };
                if let Some(signal) = zbus::fdo::PropertiesChanged::from_message(msg)
                    && let Ok(args) = signal.args()
                {
                    let changed: Vec<&str> = args.changed_properties().keys().copied().collect();
                    if is_relevant_properties_change(args.interface_name().as_str(), &changed) {
                        fire_or_defer(&refresh, &mut debouncer, &mut deferred);
                    }
                }
            }
            item = name_owner_changed.next() => {
                let Some(signal) = item else {
                    tracing::debug!("bluez NameOwnerChanged stream ended; watcher will retry");
                    return Ok(());
                };
                let Ok(args) = signal.args() else {
                    continue;
                };
                if args.new_owner().as_ref().is_some() {
                    tracing::info!("BlueZ restarted on the bus, resubscribing");
                    match subscribe(&conn).await {
                        Ok(new_subs) => subs = new_subs,
                        Err(e) => {
                            tracing::warn!(
                                "bluez resubscription failed after BlueZ restart: {e:#}; \
                                 keeping stale subscriptions until the next NameOwnerChanged"
                            );
                        }
                    }
                    // Devices may have connected or changed battery level
                    // while BlueZ was down; the safety net (30s sweep) would
                    // eventually catch it, but a refresh now closes the gap.
                    refresh.trigger();
                } else {
                    tracing::warn!(
                        "BlueZ left the bus; Bluetooth updates fall back to periodic discovery"
                    );
                }
            }
        }
    }
}

fn fire_or_defer(
    refresh: &RefreshSignal,
    debouncer: &mut Debouncer,
    deferred: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
) {
    let now = Instant::now();
    if debouncer.should_fire(now) {
        // Firing now covers whatever the pending sleep was deferring.
        *deferred = None;
        refresh.trigger();
    } else if deferred.is_none()
        && let Some(deadline) = debouncer.deadline()
    {
        *deferred = Some(Box::pin(tokio::time::sleep_until(deadline)));
    }
}

/// `org.bluez.Battery1` PropertiesChanged is always battery-relevant (its
/// only meaningful property is `Percentage`). `org.bluez.Device1` also fires
/// on `RSSI`, `ServicesResolved`, `TxPower` and other radio-link chatter
/// that has nothing to do with battery state; only `Connected` (attach or
/// detach) should trigger a re-poll.
fn is_relevant_properties_change(interface: &str, changed_properties: &[&str]) -> bool {
    match interface {
        "org.bluez.Battery1" => true,
        "org.bluez.Device1" => changed_properties.contains(&"Connected"),
        _ => false,
    }
}

/// Coalesces refresh triggers within a fixed window so a burst of D-Bus
/// signals produces at most one immediate fire per window; a signal that
/// arrives during suppression is not dropped, it is expected to result in
/// exactly one deferred fire once the window elapses (see `deadline`).
struct Debouncer {
    last_fired: Option<Instant>,
    window: Duration,
}

impl Debouncer {
    fn new(window: Duration) -> Self {
        Self {
            last_fired: None,
            window,
        }
    }

    /// Returns true if the caller should fire now, starting a new window.
    /// Returns false if the window from the last fire has not elapsed yet;
    /// the caller should arrange exactly one deferred fire at `deadline()`.
    fn should_fire(&mut self, now: Instant) -> bool {
        let ready = match self.last_fired {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.window,
        };
        if ready {
            self.last_fired = Some(now);
        }
        ready
    }

    /// The instant at which a deferred fire (postponed by `should_fire`
    /// returning false) becomes due. `None` before the first fire.
    fn deadline(&self) -> Option<Instant> {
        self.last_fired.map(|last| last + self.window)
    }
}

#[async_trait::async_trait]
impl BatterySource for BluezSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        // Check Connected property
        let props_device = zbus::fdo::PropertiesProxy::builder(&self.conn)
            .destination("org.bluez")
            .context("setting destination for Device1")?
            .path(self.path.as_ref())
            .context("setting path for Device1")?
            .build()
            .await
            .context("building PropertiesProxy for Device1")?;

        let device_iface = zbus::names::InterfaceName::try_from("org.bluez.Device1")
            .context("invalid interface name")?;
        let battery_iface = zbus::names::InterfaceName::try_from("org.bluez.Battery1")
            .context("invalid interface name")?;

        let connected_val = props_device
            .get(device_iface, "Connected")
            .await
            .context("reading Device1.Connected")?;

        let connected =
            bool::try_from(connected_val).context("parsing Device1.Connected as bool")?;

        if !connected {
            anyhow::bail!("device {} is not connected", self.info.name);
        }

        // Read the charge (same PropertiesProxy, it's at the same path)
        let pct_val = props_device
            .get(battery_iface, "Percentage")
            .await
            .context("reading Battery1.Percentage")?;

        let reading = percentage_reading(pct_val)?;

        tracing::debug!(device = %self.info.name, percent = reading.percent, "bluez poll");
        Ok(reading)
    }
}

/// `Battery1.Percentage` is a D-Bus byte (`y`); a value of any other type is
/// rejected, not coerced.
fn percentage_reading(value: zbus::zvariant::OwnedValue) -> anyhow::Result<BatteryReading> {
    let percent = u8::try_from(value).context("parsing Battery1.Percentage as u8")?;
    Ok(BatteryReading::new(percent, ChargeState::Discharging))
}

#[cfg(test)]
mod tests {
    use super::{Debouncer, SIGNAL_DEBOUNCE, device_name, is_relevant_properties_change};
    use std::time::Duration;
    use tokio::time::Instant;

    #[test]
    fn debouncer_first_call_fires() {
        let mut d = Debouncer::new(SIGNAL_DEBOUNCE);
        assert!(d.should_fire(Instant::now()));
    }

    #[test]
    fn debouncer_second_call_inside_window_does_not_fire() {
        let mut d = Debouncer::new(SIGNAL_DEBOUNCE);
        let t0 = Instant::now();
        assert!(d.should_fire(t0));
        assert!(!d.should_fire(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn debouncer_call_after_window_fires() {
        let mut d = Debouncer::new(SIGNAL_DEBOUNCE);
        let t0 = Instant::now();
        assert!(d.should_fire(t0));
        assert!(d.should_fire(t0 + SIGNAL_DEBOUNCE + Duration::from_millis(1)));
    }

    #[test]
    fn debouncer_burst_inside_window_shares_one_deadline() {
        let mut d = Debouncer::new(SIGNAL_DEBOUNCE);
        let t0 = Instant::now();
        assert!(d.should_fire(t0));
        let deadline = d.deadline();
        assert!(!d.should_fire(t0 + Duration::from_millis(500)));
        assert!(!d.should_fire(t0 + Duration::from_millis(900)));
        assert!(!d.should_fire(t0 + Duration::from_secs(4)));
        // The window did not restart: every suppressed call during the burst
        // still resolves into the same single deferred fire.
        assert_eq!(d.deadline(), deadline);
    }

    #[test]
    fn device1_connected_change_is_relevant() {
        assert!(is_relevant_properties_change(
            "org.bluez.Device1",
            &["Connected"]
        ));
    }

    #[test]
    fn device1_rssi_only_change_is_not_relevant() {
        assert!(!is_relevant_properties_change(
            "org.bluez.Device1",
            &["RSSI"]
        ));
    }

    #[test]
    fn battery1_change_is_always_relevant() {
        assert!(is_relevant_properties_change(
            "org.bluez.Battery1",
            &["Percentage"]
        ));
    }

    #[test]
    fn unrelated_interface_change_is_not_relevant() {
        assert!(!is_relevant_properties_change(
            "org.bluez.MediaControl1",
            &["Connected"]
        ));
    }

    #[test]
    fn device_name_uses_alias_first() {
        assert_eq!(
            device_name(Some("My Keyboard"), Some("KB123"), "AA:BB:CC:DD:EE:FF"),
            "My Keyboard"
        );
    }

    #[test]
    fn device_name_falls_back_to_name() {
        assert_eq!(
            device_name(None, Some("KB123"), "AA:BB:CC:DD:EE:FF"),
            "KB123"
        );
    }

    #[test]
    fn device_name_falls_back_to_addr() {
        assert_eq!(
            device_name(None, None, "AA:BB:CC:DD:EE:FF"),
            "AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn device_name_empty_alias_falls_back_to_name() {
        assert_eq!(
            device_name(Some(""), Some("KB123"), "AA:BB:CC:DD:EE:FF"),
            "KB123"
        );
    }

    #[test]
    fn device_name_empty_alias_and_name_falls_back_to_addr() {
        assert_eq!(
            device_name(Some(""), Some(""), "AA:BB:CC:DD:EE:FF"),
            "AA:BB:CC:DD:EE:FF"
        );
    }

    mod props {
        use super::super::percentage_reading;
        use crate::domain::ChargeState;
        use proptest::prelude::*;
        use zbus::zvariant::{OwnedValue, Str};

        fn non_byte_value() -> impl Strategy<Value = OwnedValue> {
            prop_oneof![
                any::<bool>().prop_map(OwnedValue::from),
                any::<u16>().prop_map(OwnedValue::from),
                any::<i32>().prop_map(OwnedValue::from),
                any::<u32>().prop_map(OwnedValue::from),
                any::<f64>().prop_map(OwnedValue::from),
                any::<String>().prop_map(|s| OwnedValue::from(Str::from(s))),
            ]
        }

        proptest! {
            #[test]
            fn any_byte_reads_in_range(percent in any::<u8>()) {
                let reading = percentage_reading(OwnedValue::from(percent)).unwrap();
                prop_assert_eq!(reading.percent, percent.min(100));
                prop_assert_eq!(reading.state, ChargeState::Discharging);
            }

            #[test]
            fn a_value_of_another_type_is_rejected(value in non_byte_value()) {
                prop_assert!(percentage_reading(value).is_err());
            }
        }
    }
}

#[cfg(test)]
mod bus_tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use tokio::time::timeout;
    use zbus::zvariant::Value;

    use super::watch_events_inner;
    use crate::bus_test::isolated;
    use crate::refresh::{RefreshSignal, RefreshWaiter};
    use crate::sources::Context;

    const DEBOUNCE: Duration = Duration::from_millis(300);
    /// Room for a signal to cross the bus and the watcher to act on it.
    const SLACK: Duration = Duration::from_millis(150);
    const DEVICE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";

    async fn fake_bluez() -> zbus::Connection {
        zbus::connection::Builder::system()
            .expect("private bus address")
            .name("org.bluez")
            .expect("well-known name")
            .build()
            .await
            .expect("fake BlueZ")
    }

    async fn properties_changed(bluez: &zbus::Connection, interface: &str, property: &str) {
        let changed = HashMap::from([(property, Value::U8(42))]);
        bluez
            .emit_signal(
                None::<()>,
                DEVICE,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(interface, changed, Vec::<&str>::new()),
            )
            .await
            .expect("emit PropertiesChanged");
    }

    async fn fired(refreshed: &mut RefreshWaiter, within: Duration) -> bool {
        timeout(within, refreshed.wait()).await.is_ok()
    }

    #[tokio::test]
    async fn bluez_signals_trigger_a_debounced_refresh() {
        if !isolated(module_path!(), "bluez_signals_trigger_a_debounced_refresh") {
            return;
        }
        let bluez = fake_bluez().await;
        let refresh = RefreshSignal::new();
        let mut refreshed = refresh.waiter();
        let conn = Context::new()
            .system_bus()
            .await
            .expect("private system bus");
        let watcher = tokio::spawn(watch_events_inner(refresh, conn, DEBOUNCE));

        // Signals sent before the watcher's match rules exist are lost; repeat until one lands.
        let mut attempts = 0;
        while !fired(&mut refreshed, Duration::from_millis(50)).await {
            attempts += 1;
            assert!(attempts < 100, "the watcher never reacted");
            properties_changed(&bluez, "org.bluez.Battery1", "Percentage").await;
        }
        while fired(&mut refreshed, DEBOUNCE + SLACK).await {}

        for _ in 0..5 {
            properties_changed(&bluez, "org.bluez.Battery1", "Percentage").await;
        }
        assert!(
            fired(&mut refreshed, SLACK).await,
            "the first of a burst fires at once"
        );
        assert!(
            !fired(&mut refreshed, DEBOUNCE / 2).await,
            "the rest of the burst waits out the window"
        );
        assert!(
            fired(&mut refreshed, DEBOUNCE).await,
            "the burst's tail fires once the window closes"
        );
        assert!(
            !fired(&mut refreshed, DEBOUNCE + SLACK).await,
            "and only once"
        );

        properties_changed(&bluez, "org.bluez.Device1", "RSSI").await;
        assert!(
            !fired(&mut refreshed, DEBOUNCE + SLACK).await,
            "RSSI is noise"
        );

        let dbus = zbus::fdo::DBusProxy::new(&bluez).await.expect("DBus proxy");
        dbus.release_name("org.bluez".try_into().expect("name"))
            .await
            .expect("release org.bluez");
        let restarted = fake_bluez().await;
        assert!(
            fired(&mut refreshed, DEBOUNCE).await,
            "a restarted BlueZ re-polls"
        );
        properties_changed(&bluez, "org.bluez.Battery1", "Percentage").await;
        assert!(
            !fired(&mut refreshed, SLACK).await,
            "the old owner is no longer BlueZ"
        );
        properties_changed(&restarted, "org.bluez.Battery1", "Percentage").await;
        assert!(fired(&mut refreshed, SLACK).await, "the new owner is heard");
        assert!(!watcher.is_finished());
    }
}
