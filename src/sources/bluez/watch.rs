use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures_util::StreamExt as _;
use tokio::time::Instant;

use crate::refresh::RefreshSignal;
use crate::sources::Context;
use crate::sources::supervise::supervise;

use super::{BLUEZ, DEVICE_IFACE, PROPERTIES_IFACE, object_manager};

/// BlueZ emits a burst of signals for one physical event (several interfaces
/// added at once when a device connects); coalescing runs one discovery sweep
/// for the burst instead of one per signal.
const SIGNAL_DEBOUNCE: Duration = Duration::from_secs(5);

/// Subscribes to BlueZ D-Bus signals and asks `refresh` to re-discover when a
/// device is added, removed, connects or disconnects. A new battery level is
/// not handled here: each `BluezSource` receives its own (`pushed`). Runs under a supervising
/// retry loop with exponential backoff (`sources::supervise`): a lost system
/// bus, a BlueZ that leaves in a way `NameOwnerChanged` cannot recover from,
/// or any other stream ending is not fatal — the loop re-dials through
/// `ctx.system_bus()` (which re-connects a closed one) and resubscribes.
/// Still an optimisation, never a dependency: the supervisor's periodic
/// discovery sweep is the safety net while a watcher is down.
pub fn watch_events(refresh: RefreshSignal, ctx: Arc<Context>) {
    supervise("bluez event watcher", move || {
        let (refresh, ctx) = (refresh.clone(), ctx.clone());
        async move { watch_events_inner(refresh, ctx.system_bus().await?, SIGNAL_DEBOUNCE).await }
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
    let om = object_manager(conn).await?;

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
        .sender(BLUEZ)
        .context("setting sender filter")?
        .interface(PROPERTIES_IFACE)
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

pub(super) async fn watch_events_inner(
    refresh: RefreshSignal,
    conn: zbus::Connection,
    debounce: Duration,
) -> anyhow::Result<()> {
    let dbus = zbus::fdo::DBusProxy::new(&conn)
        .await
        .context("building DBusProxy")?;
    let mut name_owner_changed = dbus
        .receive_name_owner_changed_with_args(&[(0, BLUEZ)])
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
                    refresh.rediscover();
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
                    // Devices may have connected while BlueZ was down; the
                    // periodic sweep would catch it, a sweep now closes the gap.
                    refresh.rediscover();
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
        refresh.rediscover();
    } else if deferred.is_none()
        && let Some(deadline) = debouncer.deadline()
    {
        *deferred = Some(Box::pin(tokio::time::sleep_until(deadline)));
    }
}

/// Only `Device1.Connected` (attach or detach) changes the roster. `Device1`
/// also fires on `RSSI`, `ServicesResolved`, `TxPower` and other radio-link
/// chatter, and `Battery1` reaches its own source through `pushed`.
fn is_relevant_properties_change(interface: &str, changed_properties: &[&str]) -> bool {
    interface == DEVICE_IFACE && changed_properties.contains(&"Connected")
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

#[cfg(test)]
mod tests {
    use super::{Debouncer, SIGNAL_DEBOUNCE, is_relevant_properties_change};
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
    fn battery1_change_does_not_rediscover() {
        assert!(!is_relevant_properties_change(
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
}
