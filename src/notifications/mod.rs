use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::watch;

use crate::app::supervisor::TrayState;
use crate::config::Config;
use crate::domain::{Presence, PrimaryStatus, classify};

/// Upper bound on a single `notify` call. The D-Bus default reply timeout is
/// 25 s; a toast that hasn't been accepted well before that has already
/// missed its purpose, so this bounds the notifier task's stall, not the
/// D-Bus call itself.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// zbus proxy for org.freedesktop.Notifications (session bus).
#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

/// Tracks which devices have an outstanding low-battery notification, so each
/// low crossing fires exactly once. A device re-arms when it leaves the low
/// state (charges, rises above threshold, or goes offline).
#[derive(Default)]
struct LowTracker {
    notified: HashSet<String>,
}

impl LowTracker {
    /// Records the latest low/not-low observation for `name`. Returns true iff a
    /// notification should fire now (device is low and was not already notified).
    fn observe(&mut self, name: &str, is_low: bool) -> bool {
        if is_low {
            // insert() returns true only when the value is newly added — exactly
            // the "fire once per crossing" semantic we need.
            self.notified.insert(name.to_owned())
        } else {
            self.notified.remove(name);
            false
        }
    }
}

/// Computes which devices should fire a low-battery notification right now.
///
/// Only `Presence::Online` devices reach `LowTracker::observe`: a retained
/// reading from a device that is asleep, unreachable or disconnected is a
/// memory, not a live observation. Feeding it to the tracker would both fire
/// a false alert for a device the user cannot act on right now, and consume
/// the crossing — so the real alert would be lost when the device
/// reconnects still low. A non-Online device is therefore skipped entirely,
/// leaving its tracked state exactly as it was before it went away.
fn compute_pending(state: &TrayState, cfg: &Config, tracker: &mut LowTracker) -> Vec<(String, u8)> {
    state
        .devices
        .iter()
        .filter_map(|d| {
            if d.presence != Presence::Online {
                return None;
            }
            let threshold = cfg.effective_low_threshold(&d.info.name);
            let is_low = matches!(
                classify(d.last_reading, threshold),
                PrimaryStatus::Low { .. }
            );
            if tracker.observe(&d.info.name, is_low) {
                let pct = d.last_reading.map(|r| r.percent).unwrap_or(0);
                Some((d.info.name.clone(), pct))
            } else {
                None
            }
        })
        .collect()
}

/// Spawns a background task that fires a desktop notification whenever a device
/// crosses into the low-battery state while discharging.
///
/// `config_rx` is watched for the `notifications_enabled` flag. While disabled,
/// `LowTracker::observe` still runs (crossings are consumed silently), so toggling
/// notifications back on does not retroactively spam for devices that are already
/// low — they re-notify only on the next fresh low crossing.
///
/// The task exits quietly if the session bus or the Notifications service is
/// unavailable — battery monitoring continues unaffected.
pub fn spawn(mut rx: watch::Receiver<TrayState>, config_rx: watch::Receiver<Config>) {
    tokio::spawn(async move {
        let conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "session D-Bus unavailable: {e}; low battery notifications disabled"
                );
                return;
            }
        };
        let proxy = match NotificationsProxy::new(&conn).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "org.freedesktop.Notifications unavailable: {e}; low battery notifications disabled"
                );
                return;
            }
        };
        let mut tracker = LowTracker::default();

        loop {
            // Collect pending notifications and read config while holding borrows,
            // then drop all refs before any .await so no watch::Ref crosses an await point.
            let (pending, enabled): (Vec<(String, u8)>, bool) = {
                let state = rx.borrow_and_update();
                let cfg = config_rx.borrow();
                let pending = compute_pending(&state, &cfg, &mut tracker);
                let enabled = cfg.notifications_enabled;
                (pending, enabled)
            };

            // Only send if notifications are enabled. Crossings were already
            // consumed by LowTracker above regardless of this flag.
            if enabled {
                for (name, pct) in pending {
                    let summary = format!("{name} battery low");
                    let body = format!("{pct}% remaining");
                    let mut hints = std::collections::HashMap::new();
                    // urgency == 2 (critical) keeps the notification visible until dismissed.
                    hints.insert("urgency", zbus::zvariant::Value::U8(2));
                    let send = proxy.notify(
                        "rigbat",
                        0,
                        "battery-caution",
                        &summary,
                        &body,
                        &[],
                        hints,
                        0,
                    );
                    match tokio::time::timeout(NOTIFY_TIMEOUT, send).await {
                        Ok(Err(e)) => {
                            tracing::warn!(
                                device = %name,
                                "low battery alert not delivered: {e}"
                            );
                        }
                        Err(_) => {
                            // LowTracker already marked this crossing as notified; it stays
                            // that way. Re-arming would retry against a daemon that is still
                            // wedged, turning one missed toast into a retry storm at the poll
                            // interval, and the tray icon already shows the low colour.
                            tracing::warn!(
                                device = %name,
                                "low battery alert not delivered: timed out after {NOTIFY_TIMEOUT:?}"
                            );
                        }
                        Ok(Ok(_)) => {}
                    }
                }
            }

            if rx.changed().await.is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{LowTracker, NOTIFY_TIMEOUT, compute_pending};
    use crate::app::supervisor::TrayState;
    use crate::config::Config;
    use crate::domain::{
        BatteryReading, ChargeState, DeviceInfo, DeviceKind, DeviceState, Presence, Transport,
    };

    #[test]
    fn notify_timeout_is_well_under_dbus_default() {
        assert!(NOTIFY_TIMEOUT > Duration::ZERO);
        assert!(NOTIFY_TIMEOUT < Duration::from_secs(25));
    }

    #[test]
    fn first_low_fires_second_does_not() {
        let mut t = LowTracker::default();
        assert!(t.observe("mouse", true));
        assert!(!t.observe("mouse", true));
    }

    #[test]
    fn rearm_after_not_low_fires_again() {
        let mut t = LowTracker::default();
        assert!(t.observe("mouse", true));
        t.observe("mouse", false);
        assert!(t.observe("mouse", true));
    }

    #[test]
    fn offline_rearms() {
        let mut t = LowTracker::default();
        assert!(t.observe("mouse", true));
        // offline == is_low false
        t.observe("mouse", false);
        assert!(t.observe("mouse", true));
    }

    #[test]
    fn two_devices_tracked_independently() {
        let mut t = LowTracker::default();
        assert!(t.observe("mouse", true));
        assert!(t.observe("keyboard", true));
        // mouse already notified — no second fire
        assert!(!t.observe("mouse", true));
        // keyboard re-arms after recovery
        t.observe("keyboard", false);
        assert!(t.observe("keyboard", true));
        // mouse still armed
        assert!(!t.observe("mouse", true));
    }

    // --- compute_pending ------------------------------------------------

    fn device_state(name: &str, presence: Presence, percent: Option<u8>) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                name: name.to_owned(),
                kind: DeviceKind::Mouse,
                transport: Transport::Sysfs,
                locator: None,
            },
            last_reading: percent.map(|p| BatteryReading::new(p, ChargeState::Discharging)),
            last_seen: percent.map(|_| Instant::now()),
            presence,
            estimate: crate::domain::Estimate::Unknown,
        }
    }

    #[test]
    fn disconnected_device_with_low_retained_reading_does_not_notify() {
        let mut tracker = LowTracker::default();
        let cfg = Config::default();
        let state = TrayState {
            devices: vec![device_state("mouse", Presence::Disconnected, Some(5))],
        };

        let pending = compute_pending(&state, &cfg, &mut tracker);

        assert!(pending.is_empty());
    }

    #[test]
    fn unreachable_device_with_low_retained_reading_does_not_notify() {
        let mut tracker = LowTracker::default();
        let cfg = Config::default();
        let state = TrayState {
            devices: vec![device_state("mouse", Presence::Unreachable, Some(5))],
        };

        let pending = compute_pending(&state, &cfg, &mut tracker);

        assert!(pending.is_empty());
    }

    #[test]
    fn device_still_low_after_reconnecting_does_not_double_notify() {
        let mut tracker = LowTracker::default();
        let cfg = Config::default();

        // First, low while Online — fires and is recorded as notified.
        let online_low = TrayState {
            devices: vec![device_state("mouse", Presence::Online, Some(5))],
        };
        assert_eq!(
            compute_pending(&online_low, &cfg, &mut tracker),
            vec![("mouse".to_string(), 5)]
        );

        // Goes away while still low: skipped entirely, tracker untouched.
        let disconnected = TrayState {
            devices: vec![device_state("mouse", Presence::Disconnected, Some(5))],
        };
        assert!(compute_pending(&disconnected, &cfg, &mut tracker).is_empty());

        // Comes back Online, still low: no second notification.
        assert!(compute_pending(&online_low, &cfg, &mut tracker).is_empty());
    }

    #[test]
    fn online_device_above_threshold_does_not_notify() {
        let mut tracker = LowTracker::default();
        let cfg = Config::default();
        let state = TrayState {
            devices: vec![device_state("mouse", Presence::Online, Some(80))],
        };

        assert!(compute_pending(&state, &cfg, &mut tracker).is_empty());
    }
}
