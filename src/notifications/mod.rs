use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::sync::watch;

use crate::config::Config;
use crate::domain::{
    BatteryReading, BootTime, DeviceId, Presence, PrimaryStatus, TrayState, classify,
};
use crate::i18n::{fl, loader};

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
    #[expect(clippy::too_many_arguments)]
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
    last_seen: BootTime,
    count: u8,
}

/// How far above the threshold a discharging device must read before a new
/// low crossing can notify again: a reading hovering ±1 % around the
/// threshold is one crossing, not one per dip.
const REARM_MARGIN: u8 = 5;

/// What one online reading means for the low-battery alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Low,
    /// Not low, but too close to the threshold to re-arm.
    Recovering,
    /// Charging, or at least `REARM_MARGIN` above the threshold.
    Rearmed,
}

fn alert_level(reading: Option<BatteryReading>, threshold: u8) -> Level {
    match classify(reading, threshold) {
        PrimaryStatus::Low { .. } => Level::Low,
        PrimaryStatus::Charging { .. } => Level::Rearmed,
        PrimaryStatus::Ok { percent } if percent >= threshold.saturating_add(REARM_MARGIN) => {
            Level::Rearmed
        }
        PrimaryStatus::Ok { .. } | PrimaryStatus::Offline => Level::Recovering,
    }
}

/// Tracks which devices have an outstanding low-battery notification, so each
/// confirmed low crossing fires exactly once. A device re-arms only when it
/// charges or reads `REARM_MARGIN` above its threshold; going offline does
/// not re-arm it.
///
/// A reading only counts toward a crossing if it is low AND it is a fresh
/// reading — identified by `DeviceState::last_seen`, since `TrayState` is
/// republished on every state change, not once per poll, and a republished
/// reading must not be double-counted.
#[derive(Default)]
struct LowTracker {
    notified: HashSet<DeviceId>,
    streaks: HashMap<DeviceId, LowStreak>,
}

impl LowTracker {
    /// Records the latest observation for `id`: its `Level`, and the
    /// `last_seen` of the reading that produced it. Returns true iff a
    /// notification should fire now — the device has reached
    /// `LOW_CONFIRMATIONS` consecutive distinct low readings and was not
    /// already notified.
    ///
    /// Keyed by `DeviceId`, not by name: the project deliberately does not
    /// deduplicate a device seen over two transports, and two such entries can
    /// carry the same name. Keyed by name their streaks would merge — one
    /// entry's recovery re-arming the other, one entry's reading confirming
    /// the other's crossing.
    fn observe(&mut self, id: &DeviceId, level: Level, last_seen: Option<BootTime>) -> bool {
        if level != Level::Low {
            self.streaks.remove(id);
            if level == Level::Rearmed {
                self.notified.remove(id);
            }
            return false;
        }
        let Some(last_seen) = last_seen else {
            // A device with no reading at all is never classified Low; handle it
            // defensively rather than panicking on the missing timestamp.
            return false;
        };
        let count = match self.streaks.get_mut(id) {
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
                    id.clone(),
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
                device = %id.name,
                confirmations = count,
                needed = LOW_CONFIRMATIONS,
                "low reading not yet confirmed"
            );
        }
        count >= LOW_CONFIRMATIONS && self.notified.insert(id.clone())
    }

    /// Drops every device `state` no longer lists (removed, or renamed away).
    fn forget_missing(&mut self, state: &TrayState) {
        let listed = |id: &DeviceId| state.devices.iter().any(|d| d.info.id() == *id);
        self.notified.retain(|id| listed(id));
        self.streaks.retain(|id, _| listed(id));
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
    tracker.forget_missing(state);
    state
        .devices
        .iter()
        .filter_map(|d| {
            // Hiding a device is the only control the user has for saying
            // "this one is not my concern", and a hidden device has no tray
            // icon — a toast about it points at a state that cannot be
            // inspected. Skipped before the tracker sees it, so unhiding
            // later starts from a clean crossing rather than one consumed
            // while the device was invisible.
            if !cfg.is_shown(&d.info.name) {
                return None;
            }
            if d.presence != Presence::Online {
                return None;
            }
            let threshold = cfg.effective_low_threshold(&d.info.name);
            let level = alert_level(d.last_reading, threshold);
            if tracker.observe(&d.info.id(), level, d.last_seen) {
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
            let (pending, enabled, lang): (Vec<(String, u8)>, bool, _) = {
                let state = rx.borrow_and_update();
                let cfg = config_rx.borrow();
                let pending = compute_pending(&state, &cfg, &mut tracker);
                (pending, cfg.notifications_enabled, cfg.lang())
            };

            // Only send if notifications are enabled. Crossings were already
            // consumed by LowTracker above regardless of this flag.
            if enabled {
                for (name, pct) in pending {
                    let l = loader(lang);
                    let summary = fl!(l, "notify-low-title", name = name.as_str());
                    let body = fl!(l, "notify-low-body", percent = pct);
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
    use std::time::Duration;

    use super::{Level, LowTracker, NOTIFY_TIMEOUT, compute_pending};
    use crate::config::Config;
    use crate::domain::{
        BatteryReading, BootTime, ChargeState, DeviceId, DeviceInfo, DeviceKind, DeviceState,
        Presence, Transport, TrayState,
    };

    #[test]
    fn hidden_device_never_notifies() {
        let mut tracker = LowTracker::default();
        let cfg = Config {
            hidden_devices: vec!["mouse".to_string()],
            low_threshold: 20,
            ..Config::default()
        };
        let now = crate::clock::now();
        let state = TrayState {
            devices: vec![device_state_at(
                "mouse",
                Presence::Online,
                Some(5),
                Some(now),
            )],
        };
        assert!(compute_pending(&state, &cfg, &mut tracker).is_empty());

        // Unhiding must not fire on a crossing consumed while invisible: the
        // tracker never saw the hidden readings, so confirmation starts now.
        let shown = Config {
            hidden_devices: Vec::new(),
            ..cfg
        };
        assert!(compute_pending(&state, &shown, &mut tracker).is_empty());
    }

    #[test]
    fn notify_timeout_is_well_under_dbus_default() {
        assert!(NOTIFY_TIMEOUT > Duration::ZERO);
        assert!(NOTIFY_TIMEOUT < Duration::from_secs(25));
    }

    #[test]
    fn one_low_reading_does_not_notify() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t0)));
    }

    #[test]
    fn two_distinct_low_readings_notify_exactly_once() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        let t1 = t0 + Duration::from_secs(60);
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t0)));
        assert!(t.observe(&test_id("mouse"), Level::Low, Some(t1)));
        // Already notified for this crossing — no second fire.
        let t2 = t1 + Duration::from_secs(60);
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t2)));
    }

    #[test]
    fn republished_same_reading_does_not_notify() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t0)));
        // Same last_seen — a republication of the same reading, not a new one.
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t0)));
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t0)));
    }

    #[test]
    fn rearm_after_not_low_needs_two_fresh_confirmations() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        let t1 = t0 + Duration::from_secs(60);
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t0)));
        assert!(t.observe(&test_id("mouse"), Level::Low, Some(t1)));

        // Recovers, then goes low again — the old streak must not carry over.
        t.observe(&test_id("mouse"), Level::Rearmed, None);
        let t2 = t1 + Duration::from_secs(120);
        let t3 = t2 + Duration::from_secs(60);
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t2)));
        assert!(t.observe(&test_id("mouse"), Level::Low, Some(t3)));
    }

    #[test]
    fn going_offline_between_low_readings_leaves_streak_intact() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        // First low reading builds a streak of one.
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t0)));
        // Device goes non-Online: compute_pending skips it entirely, so
        // observe is simply not called — the streak is untouched.
        let t1 = t0 + Duration::from_secs(60);
        // Comes back Online, still low, with a fresh reading: streak reaches
        // LOW_CONFIRMATIONS and fires.
        assert!(t.observe(&test_id("mouse"), Level::Low, Some(t1)));
    }

    #[test]
    fn missing_last_seen_does_not_panic_or_notify() {
        let mut t = LowTracker::default();
        assert!(!t.observe(&test_id("mouse"), Level::Low, None));
    }

    #[test]
    fn two_devices_tracked_independently() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        let t1 = t0 + Duration::from_secs(60);

        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t0)));
        assert!(!t.observe(&test_id("keyboard"), Level::Low, Some(t0)));
        assert!(t.observe(&test_id("mouse"), Level::Low, Some(t1)));
        assert!(t.observe(&test_id("keyboard"), Level::Low, Some(t1)));
        // mouse already notified — no second fire
        let t2 = t1 + Duration::from_secs(60);
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t2)));
        // keyboard re-arms after recovery
        t.observe(&test_id("keyboard"), Level::Rearmed, None);
        assert!(!t.observe(&test_id("keyboard"), Level::Low, Some(t2)));
        let t3 = t2 + Duration::from_secs(60);
        assert!(t.observe(&test_id("keyboard"), Level::Low, Some(t3)));
        // mouse still armed
        assert!(!t.observe(&test_id("mouse"), Level::Low, Some(t3)));
    }

    // --- compute_pending ------------------------------------------------

    /// A `DeviceId` matching what `device_state` builds, so tracker tests and
    /// `compute_pending` tests name the same device.
    fn test_id(name: &str) -> DeviceId {
        DeviceId {
            name: name.to_owned(),
            transport: Transport::Sysfs,
            locator: None,
        }
    }

    /// Two entries for one physical device — the sysfs and Bluetooth views the
    /// project deliberately does not merge — can carry the same name. Their
    /// low-battery streaks must stay apart.
    #[test]
    fn same_name_on_two_transports_tracks_separately() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        let t1 = t0 + Duration::from_secs(60);
        let sysfs = DeviceId {
            name: "MX Anywhere 3".to_owned(),
            transport: Transport::Sysfs,
            locator: Some("00:00:5e:00:53:01".to_owned()),
        };
        let bluetooth = DeviceId {
            transport: Transport::Bluetooth,
            ..sysfs.clone()
        };

        // One confirmation each: neither may borrow the other's.
        assert!(!t.observe(&sysfs, Level::Low, Some(t0)));
        assert!(!t.observe(&bluetooth, Level::Low, Some(t0)));

        // The second confirmation fires each of them once, independently.
        assert!(t.observe(&sysfs, Level::Low, Some(t1)));
        assert!(t.observe(&bluetooth, Level::Low, Some(t1)));

        // And one recovering must not re-arm the other.
        t.observe(&sysfs, Level::Rearmed, None);
        assert!(!t.observe(&bluetooth, Level::Low, Some(t1 + Duration::from_secs(60))));
    }

    fn device_state(name: &str, presence: Presence, percent: Option<u8>) -> DeviceState {
        device_state_at(
            name,
            presence,
            percent,
            percent.map(|_| crate::clock::now()),
        )
    }

    fn device_state_at(
        name: &str,
        presence: Presence,
        percent: Option<u8>,
        last_seen: Option<BootTime>,
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
    fn no_access_device_with_low_retained_reading_does_not_notify() {
        let mut tracker = LowTracker::default();
        let cfg = Config::default();
        let state = TrayState {
            devices: vec![device_state("mouse", Presence::NoAccess, Some(5))],
        };

        assert!(compute_pending(&state, &cfg, &mut tracker).is_empty());
        assert!(compute_pending(&state, &cfg, &mut tracker).is_empty());
    }

    #[test]
    fn device_still_low_after_reconnecting_does_not_double_notify() {
        let mut tracker = LowTracker::default();
        let cfg = Config::default();
        let t0 = crate::clock::now();
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

    fn pending_over(
        readings: &[(u8, ChargeState)],
        cfg: &Config,
        tracker: &mut LowTracker,
    ) -> Vec<(String, u8)> {
        let t0 = crate::clock::now();
        let mut fired = Vec::new();
        for (i, (percent, charge)) in readings.iter().enumerate() {
            let mut device = device_state_at(
                "mouse",
                Presence::Online,
                Some(*percent),
                Some(t0 + Duration::from_secs(60 * i as u64)),
            );
            device.last_reading = Some(BatteryReading::new(*percent, *charge));
            let state = TrayState {
                devices: vec![device],
            };
            fired.extend(compute_pending(&state, cfg, tracker));
        }
        fired
    }

    fn threshold_20() -> Config {
        Config {
            low_threshold: 20,
            ..Config::default()
        }
    }

    #[test]
    fn hovering_around_the_threshold_notifies_once() {
        use ChargeState::Discharging as D;
        let readings = [
            (19, D),
            (19, D),
            (21, D),
            (19, D),
            (19, D),
            (24, D),
            (19, D),
            (18, D),
        ];
        let fired = pending_over(&readings, &threshold_20(), &mut LowTracker::default());
        assert_eq!(fired, [("mouse".to_owned(), 19)]);
    }

    #[test]
    fn rising_well_above_the_threshold_rearms() {
        use ChargeState::Discharging as D;
        let readings = [(19, D), (19, D), (25, D), (19, D), (18, D)];
        let fired = pending_over(&readings, &threshold_20(), &mut LowTracker::default());
        assert_eq!(fired, [("mouse".to_owned(), 19), ("mouse".to_owned(), 18)]);
    }

    #[test]
    fn charging_rearms() {
        use ChargeState::{Charging as C, Discharging as D};
        let readings = [(19, D), (19, D), (19, C), (19, D), (18, D)];
        let fired = pending_over(&readings, &threshold_20(), &mut LowTracker::default());
        assert_eq!(fired, [("mouse".to_owned(), 19), ("mouse".to_owned(), 18)]);
    }

    #[test]
    fn going_offline_does_not_rearm() {
        let cfg = threshold_20();
        let mut tracker = LowTracker::default();
        let low = [(19, ChargeState::Discharging); 2];
        assert_eq!(pending_over(&low, &cfg, &mut tracker).len(), 1);
        let away = TrayState {
            devices: vec![device_state("mouse", Presence::Unreachable, Some(19))],
        };
        assert!(compute_pending(&away, &cfg, &mut tracker).is_empty());
        assert!(pending_over(&low, &cfg, &mut tracker).is_empty());
    }

    #[test]
    fn a_device_that_leaves_the_roster_is_forgotten() {
        let cfg = threshold_20();
        let mut tracker = LowTracker::default();
        pending_over(&[(19, ChargeState::Discharging); 3], &cfg, &mut tracker);
        compute_pending(
            &TrayState {
                devices: Vec::new(),
            },
            &cfg,
            &mut tracker,
        );
        assert!(tracker.notified.is_empty());
        assert!(tracker.streaks.is_empty());
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
