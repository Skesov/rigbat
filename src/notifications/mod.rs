use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt as _;
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

/// At or below this the device is about to switch off: the only level that
/// earns critical urgency, which hosts show through Do Not Disturb.
const CRITICAL_PERCENT: u8 = 5;

/// The shipped desktop file id (`packaging/rigbat.desktop`), so hosts can
/// attribute and mute rigbat's notifications per app.
const DESKTOP_ENTRY: &str = "rigbat";

/// The spec's action key for a click on the notification body.
const DEFAULT_ACTION: &str = "default";

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

    async fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// The spec's urgency levels rigbat uses; "low" is never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Urgency {
    Normal,
    Critical,
}

impl Urgency {
    fn of(percent: u8) -> Self {
        if percent <= CRITICAL_PERCENT {
            Self::Critical
        } else {
            Self::Normal
        }
    }

    fn wire(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }
}

/// A device's progress toward a confirmed low crossing: how many consecutive
/// distinct low readings have been seen (and how many of the latest were
/// critical), and the `last_seen` of the most recent one counted (so a
/// republished reading with the same timestamp does not advance the streak).
struct LowStreak {
    last_seen: BootTime,
    count: u8,
    critical: u8,
}

/// How far above the threshold a discharging device must read before a new
/// low crossing can notify again: a reading hovering ±1 % around the
/// threshold is one crossing, not one per dip.
const REARM_MARGIN: u8 = 5;

/// What one online reading means for the low-battery alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Low(Urgency),
    /// Not low, but too close to the threshold to re-arm.
    Recovering,
    /// Charging, or at least `REARM_MARGIN` above the threshold.
    Rearmed,
}

fn alert_level(reading: Option<BatteryReading>, threshold: u8) -> Level {
    match classify(reading, threshold) {
        PrimaryStatus::Low { percent } => Level::Low(Urgency::of(percent)),
        PrimaryStatus::Charging { .. } => Level::Rearmed,
        PrimaryStatus::Ok { percent } if percent >= threshold.saturating_add(REARM_MARGIN) => {
            Level::Rearmed
        }
        PrimaryStatus::Ok { .. } | PrimaryStatus::Offline => Level::Recovering,
    }
}

/// What the tracker decided about one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Quiet,
    /// Show (or replace) the device's notification at this urgency.
    Fire(Urgency),
    /// The crossing is over: withdraw the device's notification.
    Withdraw,
}

/// Tracks which devices have an outstanding low-battery notification, so each
/// confirmed low crossing fires once, and once more if it deepens to critical.
/// A device re-arms only when it charges or reads `REARM_MARGIN` above its
/// threshold; going offline does not re-arm it.
///
/// A reading only counts toward a crossing if it is low AND it is a fresh
/// reading — identified by `DeviceState::last_seen`, since `TrayState` is
/// republished on every state change, not once per poll, and a republished
/// reading must not be double-counted.
#[derive(Default)]
struct LowTracker {
    notified: HashMap<DeviceId, Urgency>,
    streaks: HashMap<DeviceId, LowStreak>,
}

impl LowTracker {
    /// Records the latest observation for `id`: its `Level`, and the
    /// `last_seen` of the reading that produced it. Fires once the device has
    /// `LOW_CONFIRMATIONS` consecutive distinct low readings and was not
    /// already notified at that urgency; critical needs the same number of
    /// critical readings. Withdraws when a notified device re-arms.
    ///
    /// Keyed by `DeviceId`, not by name: the project deliberately does not
    /// deduplicate a device seen over two transports, and two such entries can
    /// carry the same name. Keyed by name their streaks would merge — one
    /// entry's recovery re-arming the other, one entry's reading confirming
    /// the other's crossing.
    fn observe(&mut self, id: &DeviceId, level: Level, last_seen: Option<BootTime>) -> Outcome {
        let urgency = match level {
            Level::Low(urgency) => urgency,
            Level::Recovering => {
                self.streaks.remove(id);
                return Outcome::Quiet;
            }
            Level::Rearmed => {
                self.streaks.remove(id);
                return if self.notified.remove(id).is_some() {
                    Outcome::Withdraw
                } else {
                    Outcome::Quiet
                };
            }
        };
        let Some(last_seen) = last_seen else {
            // A device with no reading at all is never classified Low; handle it
            // defensively rather than panicking on the missing timestamp.
            return Outcome::Quiet;
        };
        let streak = self.streaks.entry(id.clone()).or_insert(LowStreak {
            last_seen,
            count: 0,
            critical: 0,
        });
        // Same reading republished — does not advance the streak.
        if streak.count == 0 || streak.last_seen != last_seen {
            streak.last_seen = last_seen;
            streak.count = streak.count.saturating_add(1);
            streak.critical = match urgency {
                Urgency::Critical => streak.critical.saturating_add(1),
                Urgency::Normal => 0,
            };
        }
        let (count, critical) = (streak.count, streak.critical);
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
            return Outcome::Quiet;
        }
        let confirmed = if critical >= LOW_CONFIRMATIONS {
            Urgency::Critical
        } else {
            Urgency::Normal
        };
        match self.notified.get(id) {
            Some(&shown) if shown >= confirmed => Outcome::Quiet,
            _ => {
                self.notified.insert(id.clone(), confirmed);
                Outcome::Fire(confirmed)
            }
        }
    }

    /// Drops every device `state` no longer lists (removed, or renamed away)
    /// and returns the notified ones among them.
    fn forget_missing(&mut self, state: &TrayState) -> Vec<DeviceId> {
        let listed = |id: &DeviceId| state.devices.iter().any(|d| d.info.id() == *id);
        self.streaks.retain(|id, _| listed(id));
        self.notified
            .extract_if(|id, _| !listed(id))
            .map(|(id, _)| id)
            .collect()
    }
}

/// What the notifier should do on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Show {
        device: DeviceId,
        percent: u8,
        urgency: Urgency,
    },
    Close(DeviceId),
}

/// Computes which device notifications to show or withdraw right now.
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
fn compute_commands(state: &TrayState, cfg: &Config, tracker: &mut LowTracker) -> Vec<Command> {
    let mut commands: Vec<Command> = tracker
        .forget_missing(state)
        .into_iter()
        .map(Command::Close)
        .collect();
    for d in &state.devices {
        let id = d.info.id();
        // Hiding a device is the only control the user has for saying
        // "this one is not my concern", and a hidden device has no tray
        // icon — a toast about it points at a state that cannot be
        // inspected. Skipped before the tracker sees it, so unhiding
        // later starts from a clean crossing rather than one consumed
        // while the device was invisible.
        if !cfg.is_shown(&d.info.name) {
            if tracker.notified.contains_key(&id) {
                commands.push(Command::Close(id));
            }
            continue;
        }
        if d.presence != Presence::Online {
            continue;
        }
        let threshold = cfg.effective_low_threshold(&d.info.name);
        match tracker.observe(&id, alert_level(d.last_reading, threshold), d.last_seen) {
            Outcome::Fire(urgency) => commands.push(Command::Show {
                device: id,
                percent: d.last_reading.map_or(0, |r| r.percent),
                urgency,
            }),
            Outcome::Withdraw => commands.push(Command::Close(id)),
            Outcome::Quiet => {}
        }
    }
    commands
}

/// The notifications the server is showing for rigbat: the id `Notify`
/// returned per device, reused as `replaces_id` so a device never stacks.
struct Toasts<'a> {
    proxy: NotificationsProxy<'a>,
    ids: HashMap<DeviceId, u32>,
}

impl Toasts<'_> {
    async fn show(
        &mut self,
        device: &DeviceId,
        percent: u8,
        urgency: Urgency,
        lang: crate::i18n::Lang,
    ) {
        let l = loader(lang);
        let summary = fl!(l, "notify-low-title", name = device.name.as_str());
        let body = match urgency {
            Urgency::Normal => fl!(l, "notify-low-body", percent = percent),
            Urgency::Critical => fl!(l, "notify-critical-body", percent = percent),
        };
        let open = fl!(l, "notify-open-overview");
        let actions = [DEFAULT_ACTION, open.as_str()];
        let hints = HashMap::from([
            ("urgency", zbus::zvariant::Value::U8(urgency.wire())),
            ("category", zbus::zvariant::Value::from("device")),
            ("desktop-entry", zbus::zvariant::Value::from(DESKTOP_ENTRY)),
        ]);
        let replaces_id = self.ids.get(device).copied().unwrap_or(0);
        let send = self.proxy.notify(
            "rigbat",
            replaces_id,
            "battery-caution",
            &summary,
            &body,
            &actions,
            hints,
            -1,
        );
        match tokio::time::timeout(NOTIFY_TIMEOUT, send).await {
            Ok(Err(e)) => {
                tracing::warn!(device = %device.name, "low battery alert not delivered: {e}");
            }
            Err(_) => {
                // LowTracker already marked this crossing as notified; it stays
                // that way. Re-arming would retry against a daemon that is still
                // wedged, turning one missed toast into a retry storm at the poll
                // interval, and the tray icon already shows the low colour.
                tracing::warn!(
                    device = %device.name,
                    "low battery alert not delivered: timed out after {NOTIFY_TIMEOUT:?}"
                );
            }
            // The toast is the feature's whole output and it
            // vanishes when dismissed. Without this line there is no
            // way to tell "never fired" from "fired and was missed"
            // from "the daemon swallowed it" after the fact.
            Ok(Ok(id)) => {
                self.ids.insert(device.clone(), id);
                tracing::info!(
                    device = %device.name,
                    percent,
                    ?urgency,
                    "low battery alert delivered"
                );
            }
        }
    }

    async fn close(&mut self, device: &DeviceId) {
        let Some(id) = self.ids.remove(device) else {
            return;
        };
        let close = self.proxy.close_notification(id);
        match tokio::time::timeout(NOTIFY_TIMEOUT, close).await {
            Ok(Ok(())) => tracing::debug!(device = %device.name, "low battery alert withdrawn"),
            Ok(Err(e)) => {
                tracing::warn!(device = %device.name, "low battery alert not withdrawn: {e}");
            }
            Err(_) => tracing::warn!(
                device = %device.name,
                "low battery alert not withdrawn: timed out after {NOTIFY_TIMEOUT:?}"
            ),
        }
    }

    fn is_ours(&self, id: u32) -> bool {
        self.ids.values().any(|&shown| shown == id)
    }

    /// The server closed `id` (expired, dismissed or clicked): it can no
    /// longer be replaced or withdrawn.
    fn forget(&mut self, id: u32) {
        self.ids.retain(|_, shown| *shown != id);
    }
}

/// Spawns a background task that fires a desktop notification whenever a device
/// crosses into the low-battery state while discharging, and withdraws it when
/// the crossing is over. Clicking a notification calls `open_dashboard`.
///
/// The task exits quietly if the session bus or the Notifications service is
/// unavailable — battery monitoring continues unaffected.
pub fn spawn(
    rx: watch::Receiver<TrayState>,
    config_rx: watch::Receiver<Config>,
    open_dashboard: impl Fn() + Send + 'static,
) {
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
        if let Err(e) = run(&conn, rx, config_rx, open_dashboard).await {
            tracing::warn!(
                "org.freedesktop.Notifications unavailable: {e}; low battery notifications disabled"
            );
        }
    });
}

/// The notifier loop on `conn`, until the state channel closes.
///
/// `config_rx` is watched for the `notifications_enabled` flag. While disabled,
/// `LowTracker::observe` still runs (crossings are consumed silently), so toggling
/// notifications back on does not retroactively spam for devices that are already
/// low — they re-notify only on the next fresh low crossing.
async fn run(
    conn: &zbus::Connection,
    mut rx: watch::Receiver<TrayState>,
    mut config_rx: watch::Receiver<Config>,
    open_dashboard: impl Fn() + Send,
) -> zbus::Result<()> {
    let proxy = NotificationsProxy::new(conn).await?;
    // Subscribed before the first `Notify`, so no click or close is missed.
    let mut invoked = proxy.receive_action_invoked().await?;
    let mut closed = proxy.receive_notification_closed().await?;
    let mut toasts = Toasts {
        proxy,
        ids: HashMap::new(),
    };
    let mut tracker = LowTracker::default();

    loop {
        // Collect commands and read config while holding borrows, then drop
        // all refs before any .await so no watch::Ref crosses an await point.
        let (commands, enabled, lang) = {
            let state = rx.borrow_and_update();
            let cfg = config_rx.borrow_and_update();
            let commands = compute_commands(&state, &cfg, &mut tracker);
            (commands, cfg.notifications_enabled, cfg.lang())
        };
        for command in commands {
            match command {
                Command::Show {
                    device,
                    percent,
                    urgency,
                } if enabled => toasts.show(&device, percent, urgency, lang).await,
                Command::Show { .. } => {}
                Command::Close(device) => toasts.close(&device).await,
            }
        }

        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    break;
                }
                changed = config_rx.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    break;
                }
                Some(signal) = invoked.next() => {
                    if let Ok(args) = signal.args()
                        && args.action_key == DEFAULT_ACTION
                        && toasts.is_ours(args.id)
                    {
                        open_dashboard();
                    }
                }
                Some(signal) = closed.next() => {
                    if let Ok(args) = signal.args() {
                        toasts.forget(args.id);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Command, Level, LowTracker, NOTIFY_TIMEOUT, Outcome, Urgency, compute_commands};
    use crate::config::Config;
    use crate::domain::{
        BatteryReading, BootTime, ChargeState, DeviceId, DeviceInfo, DeviceKind, DeviceState,
        Presence, Transport, TrayState,
    };

    const LOW: Level = Level::Low(Urgency::Normal);
    const CRITICAL: Level = Level::Low(Urgency::Critical);
    const FIRE: Outcome = Outcome::Fire(Urgency::Normal);
    const QUIET: Outcome = Outcome::Quiet;

    /// The `(name, percent)` of every `Show` in `commands`.
    fn shows(commands: &[Command]) -> Vec<(String, u8)> {
        commands
            .iter()
            .filter_map(|c| match c {
                Command::Show {
                    device, percent, ..
                } => Some((device.name.clone(), *percent)),
                Command::Close(_) => None,
            })
            .collect()
    }

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
        assert!(compute_commands(&state, &cfg, &mut tracker).is_empty());

        // Unhiding must not fire on a crossing consumed while invisible: the
        // tracker never saw the hidden readings, so confirmation starts now.
        let shown = Config {
            hidden_devices: Vec::new(),
            ..cfg
        };
        assert!(compute_commands(&state, &shown, &mut tracker).is_empty());
    }

    #[test]
    fn hiding_a_notified_device_withdraws_its_notification() {
        let cfg = threshold_20();
        let mut tracker = LowTracker::default();
        assert_eq!(
            pending_over(&[(19, ChargeState::Discharging); 2], &cfg, &mut tracker).len(),
            1
        );
        let hidden = Config {
            hidden_devices: vec!["mouse".to_owned()],
            ..cfg
        };
        let state = TrayState {
            devices: vec![device_state("mouse", Presence::Online, Some(19))],
        };
        assert_eq!(
            compute_commands(&state, &hidden, &mut tracker),
            [Command::Close(test_id("mouse"))]
        );
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
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t0)), QUIET);
    }

    #[test]
    fn two_distinct_low_readings_notify_exactly_once() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t0)), QUIET);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t1)), FIRE);
        // Already notified for this crossing — no second fire.
        let t2 = t1 + Duration::from_secs(60);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t2)), QUIET);
    }

    #[test]
    fn republished_same_reading_does_not_notify() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t0)), QUIET);
        // Same last_seen — a republication of the same reading, not a new one.
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t0)), QUIET);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t0)), QUIET);
    }

    #[test]
    fn rearm_after_not_low_needs_two_fresh_confirmations() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t0)), QUIET);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t1)), FIRE);

        // Recovers, then goes low again — the old streak must not carry over.
        assert_eq!(
            t.observe(&test_id("mouse"), Level::Rearmed, None),
            Outcome::Withdraw
        );
        let t2 = t1 + Duration::from_secs(120);
        let t3 = t2 + Duration::from_secs(60);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t2)), QUIET);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t3)), FIRE);
    }

    #[test]
    fn going_offline_between_low_readings_leaves_streak_intact() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        // First low reading builds a streak of one.
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t0)), QUIET);
        // Device goes non-Online: compute_commands skips it entirely, so
        // observe is simply not called — the streak is untouched.
        let t1 = t0 + Duration::from_secs(60);
        // Comes back Online, still low, with a fresh reading: streak reaches
        // LOW_CONFIRMATIONS and fires.
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t1)), FIRE);
    }

    #[test]
    fn missing_last_seen_does_not_panic_or_notify() {
        let mut t = LowTracker::default();
        assert_eq!(t.observe(&test_id("mouse"), LOW, None), QUIET);
    }

    #[test]
    fn two_devices_tracked_independently() {
        let mut t = LowTracker::default();
        let t0 = crate::clock::now();
        let t1 = t0 + Duration::from_secs(60);

        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t0)), QUIET);
        assert_eq!(t.observe(&test_id("keyboard"), LOW, Some(t0)), QUIET);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t1)), FIRE);
        assert_eq!(t.observe(&test_id("keyboard"), LOW, Some(t1)), FIRE);
        // mouse already notified — no second fire
        let t2 = t1 + Duration::from_secs(60);
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t2)), QUIET);
        // keyboard re-arms after recovery
        t.observe(&test_id("keyboard"), Level::Rearmed, None);
        assert_eq!(t.observe(&test_id("keyboard"), LOW, Some(t2)), QUIET);
        let t3 = t2 + Duration::from_secs(60);
        assert_eq!(t.observe(&test_id("keyboard"), LOW, Some(t3)), FIRE);
        // mouse still armed
        assert_eq!(t.observe(&test_id("mouse"), LOW, Some(t3)), QUIET);
    }

    #[test]
    fn urgency_is_normal_at_the_threshold_and_critical_at_five_percent() {
        assert_eq!(super::alert_level(Some(reading(20)), 20), LOW);
        assert_eq!(super::alert_level(Some(reading(6)), 20), LOW);
        assert_eq!(super::alert_level(Some(reading(5)), 20), CRITICAL);
        // A threshold at or under the critical level makes the first alert critical.
        assert_eq!(super::alert_level(Some(reading(5)), 5), CRITICAL);
    }

    #[test]
    fn a_crossing_that_deepens_to_critical_fires_once_more() {
        use ChargeState::Discharging as D;
        let commands = commands_over(
            &[(19, D), (19, D), (5, D), (4, D), (3, D)],
            &threshold_20(),
            &mut LowTracker::default(),
        );
        let urgencies: Vec<_> = commands
            .iter()
            .filter_map(|c| match c {
                Command::Show {
                    percent, urgency, ..
                } => Some((*percent, *urgency)),
                Command::Close(_) => None,
            })
            .collect();
        assert_eq!(urgencies, [(19, Urgency::Normal), (4, Urgency::Critical)]);
    }

    #[test]
    fn one_critical_sample_does_not_escalate() {
        use ChargeState::Discharging as D;
        let fired = pending_over(
            &[(19, D), (19, D), (3, D), (19, D)],
            &threshold_20(),
            &mut LowTracker::default(),
        );
        assert_eq!(fired, [("mouse".to_owned(), 19)]);
    }

    #[test]
    fn charging_withdraws_the_notification() {
        use ChargeState::{Charging as C, Discharging as D};
        let commands = commands_over(
            &[(19, D), (19, D), (19, C), (20, C)],
            &threshold_20(),
            &mut LowTracker::default(),
        );
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1], Command::Close(test_id("mouse")));
    }

    #[test]
    fn recovering_keeps_and_rearming_withdraws_the_notification() {
        use ChargeState::Discharging as D;
        let commands = commands_over(
            &[(19, D), (19, D), (22, D), (25, D)],
            &threshold_20(),
            &mut LowTracker::default(),
        );
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1], Command::Close(test_id("mouse")));
    }

    // --- compute_commands -----------------------------------------------

    /// A `DeviceId` matching what `device_state` builds, so tracker tests and
    /// `compute_commands` tests name the same device.
    fn test_id(name: &str) -> DeviceId {
        DeviceId {
            name: name.to_owned(),
            transport: Transport::Sysfs,
            locator: None,
        }
    }

    fn reading(percent: u8) -> BatteryReading {
        BatteryReading::new(percent, ChargeState::Discharging)
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
        assert_eq!(t.observe(&sysfs, LOW, Some(t0)), QUIET);
        assert_eq!(t.observe(&bluetooth, LOW, Some(t0)), QUIET);

        // The second confirmation fires each of them once, independently.
        assert_eq!(t.observe(&sysfs, LOW, Some(t1)), FIRE);
        assert_eq!(t.observe(&bluetooth, LOW, Some(t1)), FIRE);

        // And one recovering must not re-arm the other.
        t.observe(&sysfs, Level::Rearmed, None);
        assert_eq!(
            t.observe(&bluetooth, LOW, Some(t1 + Duration::from_secs(60))),
            QUIET
        );
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
            last_reading: percent.map(reading),
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

        assert!(compute_commands(&state, &cfg, &mut tracker).is_empty());
    }

    #[test]
    fn unreachable_device_with_low_retained_reading_does_not_notify() {
        let mut tracker = LowTracker::default();
        let cfg = Config::default();
        let state = TrayState {
            devices: vec![device_state("mouse", Presence::Unreachable, Some(5))],
        };

        assert!(compute_commands(&state, &cfg, &mut tracker).is_empty());
    }

    #[test]
    fn no_access_device_with_low_retained_reading_does_not_notify() {
        let mut tracker = LowTracker::default();
        let cfg = Config::default();
        let state = TrayState {
            devices: vec![device_state("mouse", Presence::NoAccess, Some(5))],
        };

        assert!(compute_commands(&state, &cfg, &mut tracker).is_empty());
        assert!(compute_commands(&state, &cfg, &mut tracker).is_empty());
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
        assert!(compute_commands(&online_low_1, &cfg, &mut tracker).is_empty());

        // Goes away while still low: skipped entirely, tracker untouched.
        let disconnected = TrayState {
            devices: vec![device_state_at(
                "mouse",
                Presence::Disconnected,
                Some(5),
                Some(t0),
            )],
        };
        assert!(compute_commands(&disconnected, &cfg, &mut tracker).is_empty());

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
            shows(&compute_commands(&online_low_2, &cfg, &mut tracker)),
            vec![("mouse".to_string(), 5)]
        );

        // Still low, same reading republished: no second notification.
        assert!(compute_commands(&online_low_2, &cfg, &mut tracker).is_empty());
    }

    fn commands_over(
        readings: &[(u8, ChargeState)],
        cfg: &Config,
        tracker: &mut LowTracker,
    ) -> Vec<Command> {
        let t0 = crate::clock::now();
        let mut commands = Vec::new();
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
            commands.extend(compute_commands(&state, cfg, tracker));
        }
        commands
    }

    fn pending_over(
        readings: &[(u8, ChargeState)],
        cfg: &Config,
        tracker: &mut LowTracker,
    ) -> Vec<(String, u8)> {
        shows(&commands_over(readings, cfg, tracker))
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
        assert!(compute_commands(&away, &cfg, &mut tracker).is_empty());
        assert!(pending_over(&low, &cfg, &mut tracker).is_empty());
    }

    #[test]
    fn a_device_that_leaves_the_roster_is_forgotten_and_withdrawn() {
        let cfg = threshold_20();
        let mut tracker = LowTracker::default();
        pending_over(&[(19, ChargeState::Discharging); 3], &cfg, &mut tracker);
        let commands = compute_commands(
            &TrayState {
                devices: Vec::new(),
            },
            &cfg,
            &mut tracker,
        );
        assert_eq!(commands, [Command::Close(test_id("mouse"))]);
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

        assert!(compute_commands(&state, &cfg, &mut tracker).is_empty());
    }

    mod bus {
        use std::collections::HashMap;
        use std::time::Duration;

        use tokio::sync::{mpsc, watch};
        use zbus::object_server::SignalEmitter;
        use zbus::zvariant::OwnedValue;

        use super::super::run;
        use super::{device_state_at, threshold_20};
        use crate::bus_test::isolated;
        use crate::config::Config;
        use crate::domain::{BatteryReading, BootTime, ChargeState, Presence, TrayState};

        const TIMEOUT: Duration = Duration::from_secs(5);
        const PATH: &str = "/org/freedesktop/Notifications";

        #[derive(Debug, PartialEq, Eq)]
        enum Call {
            Notify(Toast),
            Close(u32),
        }

        #[derive(Debug, PartialEq, Eq)]
        struct Toast {
            id: u32,
            replaces_id: u32,
            actions: Vec<String>,
            urgency: Option<u8>,
            desktop_entry: Option<String>,
            category: Option<String>,
            expire_timeout: i32,
        }

        struct FakeServer {
            calls: mpsc::UnboundedSender<Call>,
            last_id: u32,
        }

        #[zbus::interface(name = "org.freedesktop.Notifications")]
        impl FakeServer {
            #[expect(clippy::too_many_arguments)]
            fn notify(
                &mut self,
                _app_name: &str,
                replaces_id: u32,
                _app_icon: &str,
                _summary: &str,
                _body: &str,
                actions: Vec<String>,
                hints: HashMap<String, OwnedValue>,
                expire_timeout: i32,
            ) -> u32 {
                let id = if replaces_id == 0 {
                    self.last_id += 1;
                    self.last_id
                } else {
                    replaces_id
                };
                let text = |key: &str| {
                    hints
                        .get(key)
                        .and_then(|v| v.downcast_ref::<&str>().ok())
                        .map(str::to_owned)
                };
                let toast = Toast {
                    id,
                    replaces_id,
                    actions,
                    urgency: hints
                        .get("urgency")
                        .and_then(|v| v.downcast_ref::<u8>().ok()),
                    desktop_entry: text("desktop-entry"),
                    category: text("category"),
                    expire_timeout,
                };
                self.calls
                    .send(Call::Notify(toast))
                    .expect("the test holds the receiver");
                id
            }

            fn close_notification(&self, id: u32) {
                self.calls
                    .send(Call::Close(id))
                    .expect("the test holds the receiver");
            }

            #[zbus(signal)]
            async fn action_invoked(
                emitter: &SignalEmitter<'_>,
                id: u32,
                action_key: &str,
            ) -> zbus::Result<()>;

            #[zbus(signal)]
            async fn notification_closed(
                emitter: &SignalEmitter<'_>,
                id: u32,
                reason: u32,
            ) -> zbus::Result<()>;
        }

        struct Harness {
            server: zbus::Connection,
            calls: mpsc::UnboundedReceiver<Call>,
            opened: mpsc::UnboundedReceiver<()>,
            state: watch::Sender<TrayState>,
            config: watch::Sender<Config>,
            t0: BootTime,
            tick: u64,
        }

        impl Harness {
            async fn start() -> Self {
                let (calls_tx, calls) = mpsc::unbounded_channel();
                let server = zbus::connection::Builder::session()
                    .expect("private bus")
                    .name("org.freedesktop.Notifications")
                    .expect("name")
                    .serve_at(
                        PATH,
                        FakeServer {
                            calls: calls_tx,
                            last_id: 0,
                        },
                    )
                    .expect("serving the fake server")
                    .build()
                    .await
                    .expect("claiming the notifications name");
                let (state, state_rx) = watch::channel(TrayState {
                    devices: Vec::new(),
                });
                let (config, config_rx) = watch::channel(threshold_20());
                let (opened_tx, opened) = mpsc::unbounded_channel();
                let conn = zbus::Connection::session().await.expect("private bus");
                tokio::spawn(async move {
                    run(&conn, state_rx, config_rx, move || {
                        let _ = opened_tx.send(());
                    })
                    .await
                });
                Self {
                    server,
                    calls,
                    opened,
                    state,
                    config,
                    t0: crate::clock::now(),
                    tick: 0,
                }
            }

            /// Publishes a fresh reading and lets the notifier consume it, so
            /// the next one cannot overwrite it unseen.
            async fn reading(&mut self, percent: u8, charge: ChargeState) {
                self.tick += 1;
                let mut device = device_state_at(
                    "mouse",
                    Presence::Online,
                    Some(percent),
                    Some(self.t0 + Duration::from_secs(60 * self.tick)),
                );
                device.last_reading = Some(BatteryReading::new(percent, charge));
                self.state.send_replace(TrayState {
                    devices: vec![device],
                });
                settle().await;
            }

            async fn next_call(&mut self) -> Call {
                tokio::time::timeout(TIMEOUT, self.calls.recv())
                    .await
                    .expect("a call within the timeout")
                    .expect("the server is alive")
            }

            async fn next_toast(&mut self) -> Toast {
                match self.next_call().await {
                    Call::Notify(toast) => Some(toast),
                    Call::Close(_) => None,
                }
                .expect("a Notify, not a CloseNotification")
            }

            fn emitter(&self) -> SignalEmitter<'_> {
                SignalEmitter::new(&self.server, PATH).expect("a valid path")
            }
        }

        async fn settle() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        #[tokio::test]
        async fn one_toast_per_device_escalates_in_place_and_closes_on_charge() {
            if !isolated(
                module_path!(),
                "one_toast_per_device_escalates_in_place_and_closes_on_charge",
            ) {
                return;
            }
            let mut h = Harness::start().await;
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let first = h.next_toast().await;
            assert_eq!(first.replaces_id, 0);
            assert_eq!(first.urgency, Some(1), "normal at the threshold");
            assert_eq!(first.expire_timeout, -1);
            assert_eq!(first.desktop_entry.as_deref(), Some("rigbat"));
            assert_eq!(first.category.as_deref(), Some("device"));
            assert_eq!(first.actions.first().map(String::as_str), Some("default"));

            h.reading(5, ChargeState::Discharging).await;
            h.reading(4, ChargeState::Discharging).await;
            let critical = h.next_toast().await;
            assert_eq!(critical.replaces_id, first.id, "replaces, never stacks");
            assert_eq!(critical.urgency, Some(2), "critical at 5 % and below");

            h.reading(4, ChargeState::Charging).await;
            assert_eq!(h.next_call().await, Call::Close(first.id));
        }

        #[tokio::test]
        async fn a_click_opens_the_dashboard_and_a_closed_toast_is_not_reused() {
            if !isolated(
                module_path!(),
                "a_click_opens_the_dashboard_and_a_closed_toast_is_not_reused",
            ) {
                return;
            }
            let mut h = Harness::start().await;
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let toast = h.next_toast().await;

            FakeServer::action_invoked(&h.emitter(), toast.id + 100, "default")
                .await
                .expect("emit");
            FakeServer::action_invoked(&h.emitter(), toast.id, "default")
                .await
                .expect("emit");
            tokio::time::timeout(TIMEOUT, h.opened.recv())
                .await
                .expect("the dashboard opens")
                .expect("the notifier is alive");
            settle().await;
            assert!(h.opened.try_recv().is_err(), "a foreign id opened it too");

            FakeServer::notification_closed(&h.emitter(), toast.id, 2)
                .await
                .expect("emit");
            settle().await;
            h.reading(19, ChargeState::Charging).await;
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let next = h.next_toast().await;
            assert_eq!(next.replaces_id, 0, "the dismissed id is gone");
        }

        #[tokio::test]
        async fn hiding_the_device_withdraws_its_toast() {
            if !isolated(module_path!(), "hiding_the_device_withdraws_its_toast") {
                return;
            }
            let mut h = Harness::start().await;
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let toast = h.next_toast().await;
            h.config.send_replace(Config {
                hidden_devices: vec!["mouse".to_owned()],
                ..threshold_20()
            });
            assert_eq!(h.next_call().await, Call::Close(toast.id));
        }
    }
}
