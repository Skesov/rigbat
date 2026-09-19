use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::app::supervisor::TrayState;
use crate::config::Config;
use crate::domain::{Presence, PrimaryStatus, classify};

/// Upper bound on a single `notify` call. The D-Bus default reply timeout is
/// 25 s; a toast that hasn't been accepted well before that has already
/// missed its purpose, so this bounds the notifier task's stall, not the
/// D-Bus call itself.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// Consecutive distinct low readings required before a notification fires.
/// Noisy BLE devices (e.g. the UGREEN HiTune Max5) have reported a single bad
/// sample tens of percentage points below the surrounding readings; requiring
/// confirmation costs one poll interval and rejects that kind of glitch.
const LOW_CONFIRMATIONS: u8 = 2;

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

/// A device's progress toward a confirmed low crossing: how many consecutive
/// distinct low readings have been seen, and the `last_seen` of the most
/// recent one counted (so a republished reading with the same timestamp does
/// not advance the streak).
struct LowStreak {
    last_seen: Instant,
    count: u8,
}

/// Tracks which devices have an outstanding low-battery notification, so each
/// confirmed low crossing fires exactly once. A device re-arms when it leaves
/// the low state (charges, rises above threshold, or goes offline).
///
/// A reading only counts toward a crossing if it is low AND it is a fresh
/// reading — identified by `DeviceState::last_seen`, since `TrayState` is
/// republished on every state change, not once per poll, and a republished
/// reading must not be double-counted.
#[derive(Default)]
struct LowTracker {
    notified: HashSet<String>,
    streaks: HashMap<String, LowStreak>,
}

impl LowTracker {
    /// Records the latest observation for `name`: whether it is low, and the
    /// `last_seen` of the reading that produced it. Returns true iff a
    /// notification should fire now — the device has reached
    /// `LOW_CONFIRMATIONS` consecutive distinct low readings and was not
    /// already notified.
    fn observe(&mut self, name: &str, is_low: bool, last_seen: Option<Instant>) -> bool {
        if !is_low {
            self.streaks.remove(name);
            self.notified.remove(name);
            return false;
        }
        let Some(last_seen) = last_seen else {
            // A device with no reading at all is never classified Low; handle it
            // defensively rather than panicking on the missing timestamp.
            return false;
        };
        let count = match self.streaks.get_mut(name) {
            Some(streak) if streak.last_seen == last_seen => {
                // Same reading republished — does not advance the streak.
                streak.count
            }
            Some(streak) => {
                streak.last_seen = last_seen;
                streak.count = streak.count.saturating_add(1);
                streak.count
            }
            None => {
                self.streaks.insert(
                    name.to_owned(),
                    LowStreak {
                        last_seen,
                        count: 1,
                    },
                );
                1
            }
        };
        if count < LOW_CONFIRMATIONS {
            // Makes the confirmation requirement visible. Sleeping does not
            // stall it — a non-Online device is skipped without touching its
            // streak, so the second confirmation simply waits for the next
            // successful reading, however much later that is. The alert is
            // therefore delayed by device usage, not by wall-clock time, and
            // only a device that never answers again stays stuck at one.
            // Without this line that wait is indistinguishable from a broken
            // notifier.
            tracing::debug!(
                device = %name,
                confirmations = count,
                needed = LOW_CONFIRMATIONS,
                "low reading not yet confirmed"
            );
        }
        count >= LOW_CONFIRMATIONS && self.notified.insert(name.to_owned())
    }
}

/// Computes which devices should fire a low-battery notification right now.
///
/// Only `Presence::Online` devices reach `LowTracker::observe`: a retained
/// reading from a device that is asleep, unreachable or disconnected is a
/// memory, not a live observation. Feeding it to the tracker would both fire
/// a false alert for a device the user cannot act on right now, and consume
/// a confirmation — so the real alert would be lost or delayed when the
/// device reconnects still low. A non-Online device is therefore skipped
/// entirely, leaving its tracked streak exactly as it was before it went
/// away.
///
/// A device fires only after `LOW_CONFIRMATIONS` consecutive distinct low
/// readings, identified by `DeviceState::last_seen` — `TrayState` is
/// republished on every state change, not once per poll, so a republication
/// of the same reading must not advance the streak.
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
            if tracker.observe(&d.info.name, is_low, d.last_seen) {
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
                        // The toast is the feature's whole output and it
                        // vanishes when dismissed. Without this line there is no
                        // way to tell "never fired" from "fired and was missed"
                        // from "the daemon swallowed it" after the fact.
                        Ok(Ok(_)) => {
                            tracing::info!(
                                device = %name,
                                percent = pct,
                                "low battery alert delivered"
                            );
                        }
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
    fn one_low_reading_does_not_notify() {
        let mut t = LowTracker::default();
        let t0 = Instant::now();
        assert!(!t.observe("mouse", true, Some(t0)));
    }

    #[test]
    fn two_distinct_low_readings_notify_exactly_once() {
        let mut t = LowTracker::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(60);
        assert!(!t.observe("mouse", true, Some(t0)));
        assert!(t.observe("mouse", true, Some(t1)));
        // Already notified for this crossing — no second fire.
        let t2 = t1 + Duration::from_secs(60);
        assert!(!t.observe("mouse", true, Some(t2)));
    }

    #[test]
    fn republished_same_reading_does_not_notify() {
        let mut t = LowTracker::default();
        let t0 = Instant::now();
        assert!(!t.observe("mouse", true, Some(t0)));
        // Same last_seen — a republication of the same reading, not a new one.
        assert!(!t.observe("mouse", true, Some(t0)));
        assert!(!t.observe("mouse", true, Some(t0)));
    }

    #[test]
    fn rearm_after_not_low_needs_two_fresh_confirmations() {
        let mut t = LowTracker::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(60);
        assert!(!t.observe("mouse", true, Some(t0)));
        assert!(t.observe("mouse", true, Some(t1)));

        // Recovers, then goes low again — the old streak must not carry over.
        t.observe("mouse", false, None);
        let t2 = t1 + Duration::from_secs(120);
        let t3 = t2 + Duration::from_secs(60);
        assert!(!t.observe("mouse", true, Some(t2)));
        assert!(t.observe("mouse", true, Some(t3)));
    }

    #[test]
    fn going_offline_between_low_readings_leaves_streak_intact() {
        let mut t = LowTracker::default();
        let t0 = Instant::now();
        // First low reading builds a streak of one.
        assert!(!t.observe("mouse", true, Some(t0)));
        // Device goes non-Online: compute_pending skips it entirely, so
        // observe is simply not called — the streak is untouched.
        let t1 = t0 + Duration::from_secs(60);
        // Comes back Online, still low, with a fresh reading: streak reaches
        // LOW_CONFIRMATIONS and fires.
        assert!(t.observe("mouse", true, Some(t1)));
    }

    #[test]
    fn missing_last_seen_does_not_panic_or_notify() {
        let mut t = LowTracker::default();
        assert!(!t.observe("mouse", true, None));
    }

    #[test]
    fn two_devices_tracked_independently() {
        let mut t = LowTracker::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(60);

        assert!(!t.observe("mouse", true, Some(t0)));
        assert!(!t.observe("keyboard", true, Some(t0)));
        assert!(t.observe("mouse", true, Some(t1)));
        assert!(t.observe("keyboard", true, Some(t1)));
        // mouse already notified — no second fire
        let t2 = t1 + Duration::from_secs(60);
        assert!(!t.observe("mouse", true, Some(t2)));
        // keyboard re-arms after recovery
        t.observe("keyboard", false, None);
        assert!(!t.observe("keyboard", true, Some(t2)));
        let t3 = t2 + Duration::from_secs(60);
        assert!(t.observe("keyboard", true, Some(t3)));
        // mouse still armed
        assert!(!t.observe("mouse", true, Some(t3)));
    }

    // --- compute_pending ------------------------------------------------

    fn device_state(name: &str, presence: Presence, percent: Option<u8>) -> DeviceState {
        device_state_at(name, presence, percent, percent.map(|_| Instant::now()))
    }

    fn device_state_at(
        name: &str,
        presence: Presence,
        percent: Option<u8>,
        last_seen: Option<Instant>,
    ) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                name: name.to_owned(),
                kind: DeviceKind::Mouse,
                transport: Transport::Sysfs,
                locator: None,
            },
            last_reading: percent.map(|p| BatteryReading::new(p, ChargeState::Discharging)),
            last_seen,
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
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(60);

        // First low reading while Online — only one confirmation so far.
        let online_low_1 = TrayState {
            devices: vec![device_state_at(
                "mouse",
                Presence::Online,
                Some(5),
                Some(t0),
            )],
        };
        assert!(compute_pending(&online_low_1, &cfg, &mut tracker).is_empty());

        // Goes away while still low: skipped entirely, tracker untouched.
        let disconnected = TrayState {
            devices: vec![device_state_at(
                "mouse",
                Presence::Disconnected,
                Some(5),
                Some(t0),
            )],
        };
        assert!(compute_pending(&disconnected, &cfg, &mut tracker).is_empty());

        // Comes back Online with a fresh low reading: second confirmation fires.
        let online_low_2 = TrayState {
            devices: vec![device_state_at(
                "mouse",
                Presence::Online,
                Some(5),
                Some(t1),
            )],
        };
        assert_eq!(
            compute_pending(&online_low_2, &cfg, &mut tracker),
            vec![("mouse".to_string(), 5)]
        );

        // Still low, same reading republished: no second notification.
        assert!(compute_pending(&online_low_2, &cfg, &mut tracker).is_empty());
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
