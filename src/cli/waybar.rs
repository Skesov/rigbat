use std::time::Duration;

use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use zbus::fdo::{DBusProxy, NameOwnerChangedStream};
use zbus::names::BusName;

use crate::config::Config;
use crate::domain::{AGE_STEP, BootTime, DeviceState, PrimaryStatus, Roster, TrayState};
use crate::domain::{BatteryReading, DeviceInfo, Estimate, device_line, device_status};
use crate::i18n::Lang;
use crate::ipc::{DeviceCard, Snapshot, TRAY_NAME, Tray1Proxy};

/// How long one `State` call to the tray may take.
const TRAY_STATE_TIMEOUT: Duration = Duration::from_secs(2);
const TRAY_START_RETRIES: u32 = 10;
const TRAY_START_RETRY_DELAY: Duration = Duration::from_millis(300);

/// The device states a running tray's snapshot describes, as of `now`, so the
/// module renders the tray's readings through the same policy as its own.
pub fn tray_state(snapshot: Snapshot, now: BootTime) -> TrayState {
    let state = |card: DeviceCard| DeviceState {
        info: DeviceInfo {
            name: card.name,
            kind: card.kind,
            transport: card.transport,
            locator: card.locator,
        },
        last_reading: card
            .percent
            .zip(card.charge)
            .map(|(percent, charge)| BatteryReading::new(percent, charge)),
        last_seen: card
            .seen_secs_ago
            .and_then(|secs| now.checked_sub(Duration::from_secs(secs))),
        presence: card.presence,
        estimate: card.remaining_secs.map_or(Estimate::Unknown, |secs| {
            Estimate::Remaining(Duration::from_secs(secs))
        }),
    };
    TrayState {
        devices: snapshot
            .devices
            .into_iter()
            .chain(snapshot.hidden)
            .map(state)
            .collect(),
    }
}

/// Whether a tray runs now, and every later change of the tray's name owner.
/// Subscribes before asking, so a tray starting in between is not missed.
pub async fn watch_tray(conn: &zbus::Connection) -> anyhow::Result<(bool, NameOwnerChangedStream)> {
    let dbus = DBusProxy::new(conn).await?;
    let changes = dbus
        .receive_name_owner_changed_with_args(&[(0, TRAY_NAME)])
        .await?;
    let running = dbus.name_has_owner(BusName::try_from(TRAY_NAME)?).await?;
    Ok((running, changes))
}

/// The running tray's state, re-read on every `StateChanged`: the module
/// follows the tray instead of polling the hardware a second time.
pub async fn follow_tray(
    conn: &zbus::Connection,
    tx: watch::Sender<TrayState>,
) -> anyhow::Result<JoinHandle<()>> {
    let tray = Tray1Proxy::new(conn).await?;
    let mut changes = tray.receive_state_changed().await?;
    Ok(tokio::spawn(async move {
        // A tray that just started owns its name a moment before it serves its state.
        for _ in 0..TRAY_START_RETRIES {
            match read_tray(&tray).await {
                Ok(snapshot) => {
                    tx.send_replace(tray_state(snapshot, crate::clock::now()));
                    break;
                }
                Err(e) => {
                    tracing::debug!("the tray did not send its state yet: {e:#}");
                    tokio::time::sleep(TRAY_START_RETRY_DELAY).await;
                }
            }
        }
        while changes.next().await.is_some() {
            match read_tray(&tray).await {
                Ok(snapshot) => {
                    tx.send_replace(tray_state(snapshot, crate::clock::now()));
                }
                Err(e) => tracing::warn!("the running tray did not send its state: {e:#}"),
            }
        }
    }))
}

/// Copies the module's own `Supervisor` state into `tx`. Skips the empty
/// placeholder published before discovery, and waits (bounded) for a first
/// reading, so a switch away from the tray does not flash a roster with no charge.
pub async fn forward(mut rx: watch::Receiver<TrayState>, tx: watch::Sender<TrayState>) {
    if rx.changed().await.is_err() {
        return;
    }
    let has_reading = |s: &TrayState| s.devices.iter().any(|d| d.last_reading.is_some());
    let _ = tokio::time::timeout(FIRST_SWEEP_WAIT, rx.wait_for(has_reading)).await;
    loop {
        tx.send_replace(rx.borrow_and_update().clone());
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn read_tray(tray: &Tray1Proxy<'_>) -> anyhow::Result<Snapshot> {
    let json = tokio::time::timeout(TRAY_STATE_TIMEOUT, tray.state()).await??;
    Ok(serde_json::from_str(&json)?)
}

/// The device the waybar module features and its status: the aggregate tray
/// icon's pick and classification, from the same `domain` policy.
pub fn waybar_featured<'a>(
    states: &'a [DeviceState],
    cfg: &Config,
    now: BootTime,
) -> Option<(&'a DeviceState, PrimaryStatus)> {
    let roster = Roster::visible(
        states,
        |name| cfg.is_shown(name),
        now,
        cfg.hide_offline_after(),
    );
    let device = roster.featured(cfg.primary_device.as_deref())?;
    let (status, _) = device_status(device, cfg.effective_low_threshold(&device.info.name));
    Some((device, status))
}

/// Builds the waybar `custom` module payload (`return-type: json`): a single
/// object describing the featured device (`waybar_featured`), with a tooltip
/// line per device the tray lists, in the tray's order.
///
/// Renders from the live `DeviceState` snapshots `Supervisor` publishes, so a
/// sleeping device keeps its retained reading, as it does in the tray. `now`
/// is a parameter so tests are deterministic.
///
/// `class` vocabulary (documented in the README, styled by the user's CSS):
/// `charging`, `low`, `ok`, `offline`. `percentage` is omitted, not `0`, when
/// there is no reading to report.
pub fn to_waybar(states: &[DeviceState], cfg: &Config, now: BootTime) -> Value {
    let roster = Roster::visible(
        states,
        |name| cfg.is_shown(name),
        now,
        cfg.hide_offline_after(),
    );

    let tooltip = if roster.devices().is_empty() {
        "No devices".to_owned()
    } else {
        roster
            .devices()
            .iter()
            // CLI surface: always English.
            .map(|d| {
                let (status, _) = device_status(d, cfg.effective_low_threshold(&d.info.name));
                device_line(d, status, now, Lang::En)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let (text, class, percentage): (String, &str, Option<u8>) =
        match waybar_featured(states, cfg, now) {
            None => ("no devices".to_owned(), "offline", None),
            Some((_, status)) => match status {
                PrimaryStatus::Offline => ("offline".to_owned(), "offline", None),
                PrimaryStatus::Charging { percent } => {
                    (format!("{percent}%"), "charging", Some(percent))
                }
                PrimaryStatus::Low { percent } => (format!("{percent}%"), "low", Some(percent)),
                PrimaryStatus::Ok { percent } => (format!("{percent}%"), "ok", Some(percent)),
            },
        };

    let mut obj = json!({
        "text": text,
        "tooltip": tooltip,
        "class": class,
    });
    if let Some(p) = percentage {
        obj["percentage"] = json!(p);
    }
    obj
}

/// Serializes the `to_waybar` payload as one line of JSON — waybar's `custom`
/// module reads exactly one JSON object per line. Falls back to a valid (never
/// empty) line on the practically-impossible serialization failure, since a
/// custom module that receives malformed output logs an error on every update.
///
/// Returns the line rather than printing it so the streaming loop can compare
/// consecutive lines and skip a repeat: the supervisor republishes its state on
/// every poll, most of which leave the rendered line byte-identical.
fn render_waybar_line(states: &[DeviceState], cfg: &Config, now: BootTime) -> String {
    let value = to_waybar(states, cfg, now);
    serde_json::to_string(&value).unwrap_or_else(|_| {
        r#"{"text":"error","tooltip":"rigbat: failed to render status","class":"offline"}"#
            .to_owned()
    })
}

/// How long `run` waits for a first battery reading before printing
/// whatever state it has.
const FIRST_SWEEP_WAIT: Duration = Duration::from_secs(2);

// See print_json: the waybar module line is program output on stdout.
#[expect(clippy::print_stdout)]
fn print_line(line: &str) {
    println!("{line}");
}

/// Prints the featured device's line on startup and on every change of the
/// supervisor's state or the config.
pub async fn run(mut rx: watch::Receiver<TrayState>, mut cfg_rx: watch::Receiver<Config>) {
    // `Supervisor::spawn` publishes an empty `TrayState` before any backend has
    // run, so printing straight away puts a false "no devices" frame on the bar
    // until the first sweep lands. A device that has been discovered but not yet
    // polled is indistinguishable in `TrayState` from one that is genuinely
    // unreachable — both carry no reading — so waiting for the first publication
    // is not enough: that one announces the roster, not its charge.
    //
    // Wait for a reading, bounded: an all-offline roster never produces one, and
    // a module with no label at all is worse than a late one. An empty roster is
    // already final and does not wait.
    let _ = tokio::time::timeout(FIRST_SWEEP_WAIT, async {
        // The value already in the channel is the placeholder the supervisor
        // published before discovery ran; an empty roster there means "not yet",
        // not "none". Take the first real publication before judging.
        if rx.changed().await.is_err() {
            return;
        }
        loop {
            {
                let state = rx.borrow();
                let cfg = cfg_rx.borrow();
                // Judge the device the frame is actually built from. A roster can
                // carry the same mouse twice (sysfs and Bluetooth), and requiring
                // every shown device to answer never settles when one of them is
                // permanently asleep, which is the normal state of a wireless
                // mouse on its charger.
                let now = crate::clock::now();
                let settled = match waybar_featured(&state.devices, &cfg, now) {
                    Some((d, _)) => d.last_reading.is_some(),
                    None => !state.devices.iter().any(|d| cfg.is_shown(&d.info.name)),
                };
                if settled {
                    return;
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await;

    let mut last_line: Option<String> = None;
    // The tooltip's "last reading 2 h ago" ages with nothing published.
    let mut age_tick = interval_at(Instant::now() + AGE_STEP, AGE_STEP);
    age_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        {
            let state = rx.borrow();
            let cfg = cfg_rx.borrow();
            let line = render_waybar_line(&state.devices, &cfg, crate::clock::now());
            if last_line.as_deref() != Some(line.as_str()) {
                print_line(&line);
                last_line = Some(line);
            }
        }

        tokio::select! {
            r = rx.changed() => if r.is_err() { break; },
            r = cfg_rx.changed() => if r.is_err() { break; },
            _ = age_tick.tick() => {}
        }
    }

    // Reached only when a sender is gone, which means the supervisor is no
    // longer publishing. Waybar restarts the module after `restart-interval`;
    // say why the line stopped so the restart is not a silent mystery.
    tracing::error!("state channel closed, waybar module exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::tests::device;
    use crate::domain::{BatteryReading, ChargeState, Presence};

    use std::time::Duration;

    use crate::domain::Estimate;

    fn assert_single_line_json(text: &str) -> Value {
        assert_eq!(text.lines().count(), 1, "expected exactly one line");
        serde_json::from_str(text).expect("output must parse as JSON")
    }

    fn device_state(
        name: &str,
        presence: Presence,
        last_reading: Option<BatteryReading>,
        last_seen: Option<BootTime>,
    ) -> DeviceState {
        DeviceState {
            info: device(name),
            last_reading,
            last_seen,
            presence,
            estimate: Estimate::Unknown,
        }
    }

    #[test]
    fn waybar_charging_device_is_charging_class() {
        let reading = BatteryReading::new(80, ChargeState::Charging);
        let states = vec![device_state("mouse", Presence::Online, Some(reading), None)];
        let cfg = Config::default();

        let value = to_waybar(&states, &cfg, crate::clock::now());
        assert_eq!(value["class"], "charging");
        assert_eq!(value["percentage"], 80);
        assert_eq!(value["text"], "80%");
    }

    #[test]
    fn waybar_low_battery_is_low_class() {
        let reading = BatteryReading::new(10, ChargeState::Discharging);
        let states = vec![device_state("mouse", Presence::Online, Some(reading), None)];
        let cfg = Config {
            low_threshold: 20,
            ..Config::default()
        };

        let value = to_waybar(&states, &cfg, crate::clock::now());
        assert_eq!(value["class"], "low");
        assert_eq!(value["percentage"], 10);
    }

    #[test]
    fn waybar_healthy_battery_is_ok_class() {
        let reading = BatteryReading::new(80, ChargeState::Discharging);
        let states = vec![device_state("mouse", Presence::Online, Some(reading), None)];
        let cfg = Config::default();

        let value = to_waybar(&states, &cfg, crate::clock::now());
        assert_eq!(value["class"], "ok");
        assert_eq!(value["percentage"], 80);
    }

    #[test]
    fn waybar_leaves_out_a_device_that_never_answered_like_the_tray() {
        let states = vec![device_state("mouse", Presence::Disconnected, None, None)];
        let cfg = Config::default();

        let value = to_waybar(&states, &cfg, crate::clock::now());
        assert_eq!(value["class"], "offline");
        assert!(value.get("percentage").is_none());
        assert_eq!(value["text"], "no devices");
        assert_eq!(value["tooltip"], "No devices");
    }

    #[test]
    fn waybar_unreachable_with_retained_reading_shows_it_like_the_tray() {
        let seen = crate::clock::now();
        let now = seen + Duration::from_secs(300);
        let reading = BatteryReading::new(88, ChargeState::Discharging);
        let states = vec![device_state(
            "mouse",
            Presence::Unreachable,
            Some(reading),
            Some(seen),
        )];
        let cfg = Config::default();

        let value = to_waybar(&states, &cfg, now);
        assert_eq!(value["class"], "ok");
        assert_eq!(value["percentage"], 88);
        let tooltip = value["tooltip"].as_str().expect("tooltip is a string");
        assert_eq!(tooltip, "mouse: unreachable · last reading 5\u{a0}min ago");
    }

    #[test]
    fn waybar_no_devices_is_offline_with_fallback_text() {
        let states: Vec<DeviceState> = vec![];
        let cfg = Config::default();

        let value = to_waybar(&states, &cfg, crate::clock::now());
        assert_eq!(value["class"], "offline");
        assert_eq!(value["text"], "no devices");
        assert!(value.get("percentage").is_none());
    }

    #[test]
    fn waybar_tooltip_lists_the_tray_roster_in_its_order() {
        let seen = crate::clock::now();
        let now = seen + Duration::from_secs(300);
        let charging = BatteryReading::new(80, ChargeState::Charging);
        let retained = BatteryReading::new(50, ChargeState::Discharging);
        let states = vec![
            device_state(
                "keyboard",
                Presence::Unreachable,
                Some(retained),
                Some(seen),
            ),
            device_state("dongle", Presence::Disconnected, None, None),
            device_state("mouse", Presence::Online, Some(charging), Some(now)),
        ];
        let cfg = Config::default();

        let value = to_waybar(&states, &cfg, now);
        let tooltip = value["tooltip"].as_str().expect("tooltip is a string");
        let lines: Vec<&str> = tooltip.lines().collect();
        assert_eq!(
            lines,
            [
                "mouse: ⚡ 80%",
                "keyboard: unreachable · last reading 5\u{a0}min ago"
            ]
        );
    }

    #[test]
    fn waybar_features_an_online_device_over_one_without_access() {
        let reading = BatteryReading::new(60, ChargeState::Discharging);
        let states = vec![
            device_state("mouse", Presence::NoAccess, None, None),
            device_state("keyboard", Presence::Online, Some(reading), None),
        ];

        let value = to_waybar(&states, &Config::default(), crate::clock::now());
        assert_eq!(value["text"], "60%");
        let tooltip = value["tooltip"].as_str().expect("tooltip is a string");
        assert!(tooltip.contains("mouse: no access · run rigbat doctor"));
    }

    #[test]
    fn waybar_respects_hidden_devices_filter() {
        let reading = BatteryReading::new(50, ChargeState::Discharging);
        let states = vec![
            device_state("mouse", Presence::Online, Some(reading), None),
            device_state("keyboard", Presence::Online, Some(reading), None),
        ];
        let cfg = Config {
            hidden_devices: vec!["mouse".to_string()],
            ..Config::default()
        };

        let value = to_waybar(&states, &cfg, crate::clock::now());
        let tooltip = value["tooltip"].as_str().expect("tooltip is a string");
        assert_eq!(tooltip.lines().count(), 1);
        assert!(tooltip.contains("keyboard"));
    }

    #[test]
    fn waybar_respects_explicit_primary_device() {
        let reading_mouse = BatteryReading::new(90, ChargeState::Discharging);
        let reading_kbd = BatteryReading::new(10, ChargeState::Discharging);
        let states = vec![
            device_state("mouse", Presence::Online, Some(reading_mouse), None),
            device_state("keyboard", Presence::Online, Some(reading_kbd), None),
        ];
        let cfg = Config {
            primary_device: Some("keyboard".to_string()),
            ..Config::default()
        };

        let value = to_waybar(&states, &cfg, crate::clock::now());
        assert_eq!(value["percentage"], 10);
        assert_eq!(value["class"], "low");
    }

    #[test]
    fn waybar_percentage_key_absent_when_featured_device_has_no_reading() {
        let states = vec![device_state("mouse", Presence::NoAccess, None, None)];
        let cfg = Config::default();

        let value = to_waybar(&states, &cfg, crate::clock::now());
        assert!(value.get("percentage").is_none());
        assert_eq!(value["text"], "offline");
    }

    #[test]
    fn waybar_output_is_one_line_and_parses_as_json() {
        let reading = BatteryReading::new(80, ChargeState::Charging);
        let states = vec![device_state("mouse", Presence::Online, Some(reading), None)];
        let cfg = Config::default();

        let value = to_waybar(&states, &cfg, crate::clock::now());
        let text = serde_json::to_string(&value).expect("serializes");
        let parsed = assert_single_line_json(&text);
        assert_eq!(parsed["class"], "charging");
    }

    /// The own Supervisor's placeholder and a roster without a charge yet are
    /// held back; the first reading goes through at once.
    #[tokio::test(start_paused = true)]
    async fn forward_waits_for_the_first_reading() {
        let roster = |reading| TrayState {
            devices: vec![device_state("mouse", Presence::Online, reading, None)],
        };
        let (src_tx, src_rx) = watch::channel(TrayState { devices: vec![] });
        let (tx, mut rx) = watch::channel(roster(None));
        tokio::spawn(forward(src_rx, tx));

        src_tx.send_replace(roster(None));
        tokio::time::sleep(FIRST_SWEEP_WAIT / 2).await;
        assert!(
            !rx.has_changed().unwrap(),
            "forwarded a roster with no charge"
        );

        let reading = BatteryReading::new(70, ChargeState::Discharging);
        src_tx.send_replace(roster(Some(reading)));
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), roster(Some(reading)));
    }

    /// Following the tray loses nothing the module renders: the states read
    /// back from its snapshot are the states it was built from.
    #[test]
    fn the_trays_snapshot_reads_back_as_the_trays_states() {
        let now = BootTime::TEST_NOW;
        let five_min_ago = now.checked_sub(Duration::from_secs(300)).unwrap();
        let mut keyboard = device_state(
            "keyboard",
            Presence::Unreachable,
            Some(BatteryReading::new(50, ChargeState::Discharging)),
            Some(five_min_ago),
        );
        keyboard.estimate = Estimate::Remaining(Duration::from_secs(5 * 3600));
        let states = vec![
            device_state(
                "mouse",
                Presence::Online,
                Some(BatteryReading::new(80, ChargeState::Charging)),
                Some(now),
            ),
            keyboard,
            device_state("pad", Presence::NoAccess, None, None),
            device_state(
                "headset",
                Presence::Online,
                Some(BatteryReading::new(10, ChargeState::Discharging)),
                Some(now),
            ),
        ];
        let cfg = Config {
            hidden_devices: vec!["headset".to_owned()],
            primary_device: Some("keyboard".to_owned()),
            ..Config::default()
        };

        let snapshot = crate::tray::state_service::snapshot(
            &TrayState {
                devices: states.clone(),
            },
            &cfg,
            now,
        );
        let followed = tray_state(snapshot, now);

        assert_eq!(followed.devices, states);
        assert_eq!(
            to_waybar(&followed.devices, &cfg, now),
            to_waybar(&states, &cfg, now)
        );
    }
}

#[cfg(test)]
mod bus_tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;
    use crate::bus_test::isolated;
    use crate::domain::{ChargeState, DeviceKind, Presence, Transport};
    use crate::refresh::RefreshSignal;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn mouse(percent: u8) -> TrayState {
        TrayState {
            devices: vec![DeviceState {
                info: DeviceInfo {
                    name: "mouse".to_owned(),
                    kind: DeviceKind::Mouse,
                    transport: Transport::Hidraw,
                    locator: None,
                },
                last_reading: Some(BatteryReading::new(percent, ChargeState::Discharging)),
                last_seen: Some(crate::clock::now()),
                presence: Presence::Online,
                estimate: Estimate::Unknown,
            }],
        }
    }

    async fn percent_once(rx: &mut watch::Receiver<TrayState>, want: u8) {
        timeout(
            TIMEOUT,
            rx.wait_for(|s| {
                s.devices
                    .first()
                    .and_then(|d| d.last_reading)
                    .is_some_and(|r| r.percent == want)
            }),
        )
        .await
        .expect("no state with the wanted charge")
        .expect("the follower stopped");
    }

    /// With a tray on the bus the module reads its state and every change
    /// of it, and sees the tray go away.
    #[tokio::test]
    async fn follows_a_running_tray_and_sees_it_stop() {
        if !isolated(module_path!(), "follows_a_running_tray_and_sees_it_stop") {
            return;
        }
        let client = zbus::Connection::session().await.expect("private bus");
        let (running, _) = watch_tray(&client).await.expect("bus");
        assert!(!running);

        let (state_tx, state_rx) = watch::channel(mouse(80));
        let (_config_tx, config_rx) = watch::channel(Config::default());
        let tray = zbus::connection::Builder::session()
            .expect("private bus")
            .name(TRAY_NAME)
            .expect("name")
            .build()
            .await
            .expect("claiming the tray name");
        tokio::spawn(crate::tray::state_service::serve(
            tray.clone(),
            state_rx,
            config_rx,
            RefreshSignal::new(),
        ));

        let (running, mut owners) = watch_tray(&client).await.expect("bus");
        assert!(running);
        let (tx, mut rx) = watch::channel(TrayState { devices: vec![] });
        let _follower = follow_tray(&client, tx).await.expect("following the tray");
        percent_once(&mut rx, 80).await;

        state_tx.send_replace(mouse(79));
        percent_once(&mut rx, 79).await;

        tray.release_name(TRAY_NAME).await.expect("releasing");
        let change = timeout(TIMEOUT, owners.next())
            .await
            .expect("NameOwnerChanged when the tray stops")
            .expect("owner stream");
        assert!(change.args().expect("args").new_owner().is_none());
    }
}
