use std::collections::HashMap;

use crate::config::Config;
use crate::domain::{
    BatteryReading, BootTime, DeviceId, Presence, PrimaryStatus, TrayState, classify,
};

/// Consecutive distinct low readings required before a notification fires.
/// Noisy BLE devices (e.g. the UGREEN HiTune Max5) have reported a single bad
/// sample tens of percentage points below the surrounding readings; requiring
/// confirmation costs one poll interval and rejects that kind of glitch.
const LOW_CONFIRMATIONS: u8 = 2;

/// At or below this the device is about to switch off: the only level that
/// earns critical urgency, which hosts show through Do Not Disturb.
const CRITICAL_PERCENT: u8 = 5;

/// The spec's urgency levels rigbat uses; "low" is never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Urgency {
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

    pub(super) fn wire(self) -> u8 {
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
#[derive(Default)]
pub(super) struct LowTracker {
    notified: HashMap<DeviceId, Urgency>,
    streaks: HashMap<DeviceId, LowStreak>,
}

impl LowTracker {
    /// Records the latest observation for `id`: its `Level`, and the
    /// `last_seen` of the reading that produced it. Fires once the device has
    /// `LOW_CONFIRMATIONS` consecutive distinct low readings and was not
    /// already notified at that urgency; critical needs the same number of
    /// critical readings. Withdraws when a notified device re-arms.
    /// Readings are told apart by `last_seen`: `TrayState` is republished on
    /// every state change, not once per poll.
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
            // The wait for the next fresh reading is otherwise invisible.
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
pub(super) enum Command {
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
/// reading is a memory, and feeding it in would fire for a device the user
/// cannot act on and consume a confirmation. A non-Online device keeps its
/// streak exactly as it was.
pub(super) fn compute_commands(
    state: &TrayState,
    cfg: &Config,
    tracker: &mut LowTracker,
) -> Vec<Command> {
    let mut commands: Vec<Command> = tracker
        .forget_missing(state)
        .into_iter()
        .map(Command::Close)
        .collect();
    for d in &state.devices {
        let id = d.info.id();
        // Hidden devices skip the tracker, so unhiding starts a clean crossing.
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

#[cfg(test)]
pub(super) mod tests {
    use std::time::Duration;

    use super::{Command, Level, LowTracker, Outcome, Urgency, compute_commands};
    use crate::config::Config;
    use crate::domain::{
        BatteryReading, BootTime, ChargeState, DeviceId, DeviceState, Presence, Transport,
        TrayState,
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

    pub(crate) fn device_state_at(
        name: &str,
        presence: Presence,
        percent: Option<u8>,
        last_seen: Option<BootTime>,
    ) -> DeviceState {
        DeviceState::test(name)
            .presence(presence)
            .reading(percent.map(reading))
            .seen(last_seen)
    }

    #[test]
    fn a_device_that_is_not_online_never_notifies_from_a_retained_reading() {
        for presence in [
            Presence::Disconnected,
            Presence::Unreachable,
            Presence::NoAccess,
        ] {
            let mut tracker = LowTracker::default();
            let cfg = Config::default();
            let state = TrayState {
                devices: vec![device_state("mouse", presence, Some(5))],
            };

            assert!(
                compute_commands(&state, &cfg, &mut tracker).is_empty(),
                "{presence:?}"
            );
            assert!(
                compute_commands(&state, &cfg, &mut tracker).is_empty(),
                "{presence:?}"
            );
        }
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

    pub(crate) fn threshold_20() -> Config {
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
}
