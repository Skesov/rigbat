//! BlueZ `Battery1` backend: the charge of connected Bluetooth devices over D-Bus.
//! `Battery1` carries no charging state, so a reading is always `Discharging`.
//! A device is named `Alias`, else `Name`, else its address.

use anyhow::Context as _;
use futures_util::StreamExt as _;
use std::collections::HashMap;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::{BatteryBackend, BatterySource, Context};
use crate::domain::{BatteryReading, ChargeState, DeviceInfo, Transport, guess_kind};

const BLUEZ: &str = "org.bluez";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const BATTERY_IFACE: &str = "org.bluez.Battery1";

mod watch;

pub use watch::watch_events;

pub struct BluezBackend;

struct BluezSource {
    info: DeviceInfo,
    path: OwnedObjectPath,
    /// Asked for the connection on every use: a reconnect replaces it there.
    ctx: Context,
    /// `Battery1` changes at `path`, subscribed on the first `pushed` call.
    changes: Option<zbus::MessageStream>,
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
        let conn = ctx.system_bus().await?;
        let objects = object_manager(&conn)
            .await?
            .get_managed_objects()
            .await
            .context("GetManagedObjects on org.bluez")?;
        Ok(objects
            .into_iter()
            .filter_map(|(path, interfaces)| {
                let info = device_from_object(path.as_str(), &interfaces)?;
                Some(Box::new(BluezSource {
                    info,
                    path,
                    ctx: ctx.clone(),
                    changes: None,
                }) as Box<dyn BatterySource>)
            })
            .collect())
    }
}

async fn object_manager(
    conn: &zbus::Connection,
) -> anyhow::Result<zbus::fdo::ObjectManagerProxy<'static>> {
    zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination(BLUEZ)
        .context("setting destination")?
        .path("/")
        .context("setting path")?
        .build()
        .await
        .context("building ObjectManagerProxy")
}

/// The device at `path` when it is connected and has a battery.
fn device_from_object(
    path: &str,
    interfaces: &HashMap<zbus::names::OwnedInterfaceName, HashMap<String, OwnedValue>>,
) -> Option<DeviceInfo> {
    let device = interfaces.get(DEVICE_IFACE)?;
    if !interfaces.contains_key(BATTERY_IFACE) {
        return None;
    }
    let connected = device
        .get("Connected")
        .and_then(|v| bool::try_from(v.clone()).ok())
        .unwrap_or(false);
    if !connected {
        return None;
    }
    let text = |key: &str| {
        device
            .get(key)
            .and_then(|v| String::try_from(v.clone()).ok())
    };
    let locator = address_of(path);
    let name = device_name(text("Alias").as_deref(), text("Name").as_deref(), &locator);
    Some(DeviceInfo {
        kind: guess_kind(&name),
        name,
        transport: Transport::Bluetooth,
        locator: Some(locator),
    })
}

/// The MAC BlueZ encodes in the object path (`/org/bluez/hci0/dev_AA_BB_…`),
/// colon-separated as `bluetoothctl` prints it.
fn address_of(path: &str) -> String {
    let tail = path.rsplit('/').next().unwrap_or("");
    tail.strip_prefix("dev_").unwrap_or(tail).replace('_', ":")
}
#[async_trait::async_trait]
impl BatterySource for BluezSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let conn = self.ctx.system_bus().await?;
        let props_device = zbus::fdo::PropertiesProxy::builder(&conn)
            .destination(BLUEZ)
            .context("setting destination for Device1")?
            .path(self.path.as_ref())
            .context("setting path for Device1")?
            .build()
            .await
            .context("building PropertiesProxy for Device1")?;

        let device_iface =
            zbus::names::InterfaceName::try_from(DEVICE_IFACE).context("invalid interface name")?;
        let battery_iface = zbus::names::InterfaceName::try_from(BATTERY_IFACE)
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

        let reading = percentage_reading(&pct_val)?;

        tracing::debug!(device = %self.info.name, percent = reading.percent, "bluez poll");
        Ok(reading)
    }

    async fn pushed(&mut self) -> BatteryReading {
        loop {
            let changes = match &mut self.changes {
                Some(changes) => changes,
                None => match battery_changes(&self.ctx, &self.path).await {
                    Ok(changes) => self.changes.insert(changes),
                    Err(e) => {
                        tracing::debug!(device = %self.info.name, "no Battery1 signals: {e:#}");
                        return std::future::pending().await;
                    }
                },
            };
            let Some(Ok(msg)) = changes.next().await else {
                // Resubscribed on the next call, after the poll interval.
                self.changes = None;
                return std::future::pending().await;
            };
            if let Some(reading) = pushed_percentage(&msg) {
                tracing::debug!(device = %self.info.name, percent = reading.percent, "bluez push");
                return reading;
            }
        }
    }
}

/// `PropertiesChanged` of `Battery1` at one device's object path.
async fn battery_changes(
    ctx: &Context,
    path: &OwnedObjectPath,
) -> anyhow::Result<zbus::MessageStream> {
    let conn = ctx.system_bus().await?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(BLUEZ)?
        .path(path.as_ref())?
        .interface(PROPERTIES_IFACE)?
        .member("PropertiesChanged")?
        .arg(0, BATTERY_IFACE)?
        .build();
    zbus::MessageStream::for_match_rule(rule, &conn, None)
        .await
        .context("subscribing to Battery1 PropertiesChanged")
}

/// The reading a `Battery1` `PropertiesChanged` carries, if it changed `Percentage`.
fn pushed_percentage(msg: &zbus::Message) -> Option<BatteryReading> {
    let signal = zbus::fdo::PropertiesChanged::from_message(msg.clone())?;
    let args = signal.args().ok()?;
    if args.interface_name().as_str() != BATTERY_IFACE {
        return None;
    }
    let value = args.changed_properties().get("Percentage")?;
    percentage_reading(value).ok()
}

/// `Battery1.Percentage` is a D-Bus byte (`y`); a value of any other type is
/// rejected, not coerced.
fn percentage_reading(value: &zbus::zvariant::Value<'_>) -> anyhow::Result<BatteryReading> {
    let percent = u8::try_from(value).context("parsing Battery1.Percentage as u8")?;
    Ok(BatteryReading::new(percent, ChargeState::Discharging))
}

#[cfg(test)]
mod tests {
    use super::{address_of, device_from_object, device_name};
    use std::collections::HashMap;
    use zbus::names::OwnedInterfaceName;
    use zbus::zvariant::{OwnedValue, Str};

    const PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";

    fn object(
        connected: bool,
        alias: &str,
        battery: bool,
    ) -> HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>> {
        let device = HashMap::from([
            ("Connected".to_owned(), OwnedValue::from(connected)),
            ("Alias".to_owned(), OwnedValue::from(Str::from(alias))),
        ]);
        let mut interfaces = HashMap::from([(
            OwnedInterfaceName::try_from("org.bluez.Device1").unwrap(),
            device,
        )]);
        if battery {
            interfaces.insert(
                OwnedInterfaceName::try_from("org.bluez.Battery1").unwrap(),
                HashMap::new(),
            );
        }
        interfaces
    }

    #[test]
    fn address_of_reads_the_mac_from_the_object_path() {
        assert_eq!(address_of(PATH), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn device_from_object_takes_a_connected_device_with_a_battery() {
        let info = device_from_object(PATH, &object(true, "MX Keys", true)).unwrap();
        assert_eq!(info.name, "MX Keys");
        assert_eq!(info.locator.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        let unnamed = device_from_object(PATH, &object(true, "", true)).unwrap();
        assert_eq!(unnamed.name, "AA:BB:CC:DD:EE:FF");
        assert!(device_from_object(PATH, &object(false, "MX Keys", true)).is_none());
        assert!(device_from_object(PATH, &object(true, "MX Keys", false)).is_none());
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
                let reading = percentage_reading(&OwnedValue::from(percent)).unwrap();
                prop_assert_eq!(reading.percent, percent.min(100));
                prop_assert_eq!(reading.state, ChargeState::Discharging);
            }

            #[test]
            fn a_value_of_another_type_is_rejected(value in non_byte_value()) {
                prop_assert!(percentage_reading(&value).is_err());
            }
        }
    }
}

#[cfg(test)]
mod bus_tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use tokio::time::timeout;
    use zbus::zvariant::{OwnedObjectPath, Value};

    use super::watch::watch_events_inner;
    use super::{BluezBackend, BluezSource};
    use crate::bus_test::isolated;
    use crate::domain::{BatteryReading, DeviceInfo, DeviceKind, Transport};
    use crate::refresh::{RefreshSignal, RefreshWaiter};
    use crate::sources::context::BUS_CALL_TIMEOUT;
    use crate::sources::{BatteryBackend as _, BatterySource as _, Context};

    const DEBOUNCE: Duration = Duration::from_millis(300);
    /// Room for a signal to cross the bus and the watcher to act on it.
    const SLACK: Duration = Duration::from_millis(150);
    const DEVICE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
    const OTHER: &str = "/org/bluez/hci0/dev_11_22_33_44_55_66";

    async fn fake_bluez() -> zbus::Connection {
        zbus::connection::Builder::system()
            .expect("private bus address")
            .name("org.bluez")
            .expect("well-known name")
            .build()
            .await
            .expect("fake BlueZ")
    }

    async fn properties_changed_at(
        bluez: &zbus::Connection,
        path: &str,
        interface: &str,
        property: &str,
        value: u8,
    ) {
        let changed = HashMap::from([(property, Value::U8(value))]);
        bluez
            .emit_signal(
                None::<()>,
                path,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(interface, changed, Vec::<&str>::new()),
            )
            .await
            .expect("emit PropertiesChanged");
    }

    async fn properties_changed(bluez: &zbus::Connection, interface: &str, property: &str) {
        properties_changed_at(bluez, DEVICE, interface, property, 42).await;
    }

    async fn fired(refreshed: &mut RefreshWaiter, within: Duration) -> bool {
        timeout(within, refreshed.wait()).await.is_ok()
    }

    #[tokio::test]
    async fn bluez_signals_trigger_a_debounced_rediscover() {
        if !isolated(
            module_path!(),
            "bluez_signals_trigger_a_debounced_rediscover",
        ) {
            return;
        }
        let bluez = fake_bluez().await;
        let refresh = RefreshSignal::new();
        let mut refreshed = refresh.sweep_waiter();
        let mut polled = refresh.waiter();
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
            properties_changed(&bluez, "org.bluez.Device1", "Connected").await;
        }
        while fired(&mut refreshed, DEBOUNCE + SLACK).await {}

        for _ in 0..5 {
            properties_changed(&bluez, "org.bluez.Device1", "Connected").await;
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
        properties_changed(&bluez, "org.bluez.Battery1", "Percentage").await;
        assert!(
            !fired(&mut refreshed, DEBOUNCE + SLACK).await,
            "a battery level is its own source's business"
        );

        let dbus = zbus::fdo::DBusProxy::new(&bluez).await.expect("DBus proxy");
        dbus.release_name("org.bluez".try_into().expect("name"))
            .await
            .expect("release org.bluez");
        let restarted = fake_bluez().await;
        assert!(
            fired(&mut refreshed, DEBOUNCE).await,
            "a restarted BlueZ re-discovers"
        );
        properties_changed(&bluez, "org.bluez.Device1", "Connected").await;
        assert!(
            !fired(&mut refreshed, SLACK).await,
            "the old owner is no longer BlueZ"
        );
        properties_changed(&restarted, "org.bluez.Device1", "Connected").await;
        assert!(fired(&mut refreshed, SLACK).await, "the new owner is heard");
        assert!(
            !fired(&mut polled, SLACK).await,
            "no Bluetooth signal re-polls every source"
        );
        assert!(!watcher.is_finished());
    }

    fn source(ctx: &Context, path: &str) -> BluezSource {
        BluezSource {
            info: DeviceInfo {
                name: path.to_owned(),
                kind: DeviceKind::Mouse,
                transport: Transport::Bluetooth,
                locator: None,
            },
            path: OwnedObjectPath::try_from(path).expect("object path"),
            ctx: ctx.clone(),
            changes: None,
        }
    }

    struct FakeDevice;

    #[zbus::interface(name = "org.bluez.Device1")]
    impl FakeDevice {
        #[zbus(property)]
        fn connected(&self) -> bool {
            true
        }
    }

    struct FakeBattery;

    #[zbus::interface(name = "org.bluez.Battery1")]
    impl FakeBattery {
        #[zbus(property)]
        fn percentage(&self) -> u8 {
            64
        }
    }

    struct WedgedObjectManager;

    #[zbus::interface(name = "org.freedesktop.DBus.ObjectManager")]
    impl WedgedObjectManager {
        async fn get_managed_objects(&self) -> HashMap<OwnedObjectPath, HashMap<String, u8>> {
            std::future::pending().await
        }
    }

    /// A `bluetoothd` that takes the call and never replies fails the sweep
    /// (which retires nothing) instead of blocking discovery for good.
    #[tokio::test]
    async fn discovery_against_a_bluez_that_never_replies_fails_in_bounded_time() {
        if !isolated(
            module_path!(),
            "discovery_against_a_bluez_that_never_replies_fails_in_bounded_time",
        ) {
            return;
        }
        let bluez = fake_bluez().await;
        bluez
            .object_server()
            .at("/", WedgedObjectManager)
            .await
            .expect("ObjectManager");

        let found = timeout(BUS_CALL_TIMEOUT * 3, BluezBackend.discover(&Context::new()))
            .await
            .expect("discovery still waiting on the wedged bluetoothd");
        let err = format!("{:#}", found.err().expect("a failed sweep"));
        assert!(err.contains("GetManagedObjects"), "{err}");
    }

    /// A `dbus-daemon` restart closes the shared connection; the source must
    /// poll through the one `Context` dials next, not the closed one it was built with.
    #[tokio::test]
    async fn a_poll_after_the_bus_reconnects_uses_the_new_connection() {
        if !isolated(
            module_path!(),
            "a_poll_after_the_bus_reconnects_uses_the_new_connection",
        ) {
            return;
        }
        let bluez = fake_bluez().await;
        bluez
            .object_server()
            .at(DEVICE, FakeDevice)
            .await
            .expect("Device1");
        bluez
            .object_server()
            .at(DEVICE, FakeBattery)
            .await
            .expect("Battery1");
        let ctx = Context::new();
        let conn = ctx.system_bus().await.expect("private system bus");
        let mut source = source(&ctx, DEVICE);
        assert_eq!(source.poll().await.expect("first poll").percent, 64);

        conn.close().await.expect("closing the connection");

        let reading = source.poll().await.expect("a poll after the reconnect");
        assert_eq!(reading.percent, 64);
    }

    fn next_push(
        mut source: BluezSource,
    ) -> tokio::task::JoinHandle<(BluezSource, BatteryReading)> {
        tokio::spawn(async move {
            let reading = source.pushed().await;
            (source, reading)
        })
    }

    /// A `Percentage` change reaches the source at its path, as a reading,
    /// and nothing else: not another device, not a re-poll, not a sweep.
    #[tokio::test]
    async fn a_percentage_signal_updates_only_its_device() {
        if !isolated(
            module_path!(),
            "a_percentage_signal_updates_only_its_device",
        ) {
            return;
        }
        let bluez = fake_bluez().await;
        let ctx = Context::new();
        let conn = ctx.system_bus().await.expect("private system bus");
        let refresh = RefreshSignal::new();
        let mut swept = refresh.sweep_waiter();
        let mut polled = refresh.waiter();
        tokio::spawn(watch_events_inner(refresh, conn, DEBOUNCE));

        // Subscriptions are made lazily; repeat until the watcher and both sources hear.
        let mut device = next_push(source(&ctx, DEVICE));
        let mut other = next_push(source(&ctx, OTHER));
        let mut attempts = 0;
        let mut watcher_heard = false;
        while !(watcher_heard && device.is_finished() && other.is_finished()) {
            attempts += 1;
            assert!(attempts < 100, "the subscriptions never became active");
            properties_changed_at(&bluez, DEVICE, "org.bluez.Battery1", "Percentage", 50).await;
            properties_changed_at(&bluez, OTHER, "org.bluez.Battery1", "Percentage", 50).await;
            properties_changed(&bluez, "org.bluez.Device1", "Connected").await;
            watcher_heard |= fired(&mut swept, Duration::from_millis(50)).await;
        }
        while fired(&mut swept, DEBOUNCE + SLACK).await {}
        let (device_source, _) = device.await.expect("device task");
        let (other_source, _) = other.await.expect("other task");

        device = next_push(device_source);
        other = next_push(other_source);
        properties_changed_at(&bluez, DEVICE, "org.bluez.Battery1", "Percentage", 42).await;

        let (_, reading) = timeout(SLACK * 4, device)
            .await
            .expect("the device heard its Percentage")
            .expect("device task");
        assert_eq!(reading.percent, 42);
        tokio::time::sleep(SLACK).await;
        assert!(!other.is_finished(), "another device took the reading");
        assert!(!fired(&mut swept, DEBOUNCE + SLACK).await, "swept");
        assert!(!fired(&mut polled, SLACK).await, "re-polled");
    }
}
