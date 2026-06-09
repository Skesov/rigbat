use std::collections::HashSet;

use tokio::sync::watch;

use crate::app::supervisor::TrayState;
use crate::config::Config;
use crate::domain::{PrimaryStatus, classify};

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
pub fn spawn(
    mut rx: watch::Receiver<TrayState>,
    config_rx: watch::Receiver<Config>,
    threshold: u8,
) {
    tokio::spawn(async move {
        let Ok(conn) = zbus::Connection::session().await else {
            return;
        };
        let Ok(proxy) = NotificationsProxy::new(&conn).await else {
            return;
        };
        let mut tracker = LowTracker::default();

        loop {
            // Collect pending notifications and read the enabled flag while
            // holding borrows, then drop both refs before any .await so no
            // watch::Ref crosses an await point.
            let (pending, enabled): (Vec<(String, u8)>, bool) = {
                let state = rx.borrow_and_update();
                let pending = state
                    .devices
                    .iter()
                    .filter_map(|(info, reading)| {
                        let is_low =
                            matches!(classify(*reading, threshold), PrimaryStatus::Low { .. });
                        if tracker.observe(&info.name, is_low) {
                            let pct = reading.map(|r| r.percent).unwrap_or(0);
                            Some((info.name.clone(), pct))
                        } else {
                            None
                        }
                    })
                    .collect();
                let enabled = config_rx.borrow().notifications_enabled;
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
                    let _ = proxy
                        .notify(
                            "rigbat",
                            0,
                            "battery-caution",
                            &summary,
                            &body,
                            &[],
                            hints,
                            0,
                        )
                        .await;
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
    use super::LowTracker;

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
}
