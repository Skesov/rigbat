use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::config::Config;
use crate::domain::{AGE_STEP, TrayState};

mod follow;
mod render;

pub use follow::{follow_tray, forward, watch_tray};
use render::render_waybar_line;
pub use render::waybar_featured;

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
    use super::follow::tray_state;
    use super::render::to_waybar;
    use super::*;
    use crate::cli::tests::device;
    use crate::domain::{BatteryReading, BootTime, ChargeState, DeviceState, Presence};
    use serde_json::Value;

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
    use futures_util::StreamExt as _;

    use crate::domain::{
        BatteryReading, ChargeState, DeviceInfo, DeviceKind, DeviceState, Estimate, Presence,
        Transport,
    };
    use crate::ipc::TRAY_NAME;
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

#[cfg(test)]
mod featured_tests {
    use std::time::Duration;

    use crate::config::Config;
    use crate::domain::{
        BatteryReading, BootTime, ChargeState, DeviceInfo, DeviceKind, DeviceState, Estimate,
        Presence, PrimaryStatus, Transport, TrayState,
    };

    fn device(
        name: &str,
        transport: Transport,
        presence: Presence,
        percent: Option<u8>,
        seen: BootTime,
    ) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                name: name.to_owned(),
                kind: DeviceKind::Mouse,
                transport,
                locator: None,
            },
            last_reading: percent.map(|p| BatteryReading::new(p, ChargeState::Discharging)),
            last_seen: percent.map(|_| seen),
            presence,
            estimate: Estimate::Unknown,
        }
    }

    /// Two copies of one mouse, a sleeping keyboard and a dongle that never answered.
    fn roster(now: BootTime) -> TrayState {
        let seen = now.checked_sub(Duration::from_secs(300)).unwrap();
        TrayState {
            devices: vec![
                device(
                    "MX",
                    Transport::Sysfs,
                    Presence::Unreachable,
                    Some(70),
                    seen,
                ),
                device("MX", Transport::Bluetooth, Presence::Online, Some(40), now),
                device(
                    "Keys",
                    Transport::Hidraw,
                    Presence::Unreachable,
                    Some(88),
                    seen,
                ),
                device(
                    "Dongle",
                    Transport::Hidraw,
                    Presence::Unreachable,
                    None,
                    seen,
                ),
            ],
        }
    }

    #[test]
    fn waybar_and_the_tray_icon_feature_the_same_device_and_status() {
        let now = crate::clock::now();
        let state = roster(now);
        let cases = [
            (None, "MX", Transport::Bluetooth, 40),
            (Some("MX"), "MX", Transport::Bluetooth, 40),
            (Some("Keys"), "Keys", Transport::Hidraw, 88),
            (Some("Dongle"), "MX", Transport::Bluetooth, 40),
        ];
        for (primary, name, transport, percent) in cases {
            let cfg = Config {
                primary_device: primary.map(str::to_owned),
                ..Config::default()
            };
            let tray = crate::tray::resolve::resolve_for(None, &state, &cfg, now)
                .expect("the tray features a device");
            let (waybar, waybar_status) =
                crate::cli::waybar::waybar_featured(&state.devices, &cfg, now).expect("waybar too");
            assert_eq!(waybar.info.id(), tray.state.info.id(), "pin {primary:?}");
            assert_eq!(waybar_status, tray.status, "pin {primary:?}");
            assert_eq!(
                (
                    waybar.info.name.as_str(),
                    waybar.info.transport,
                    waybar_status
                ),
                (name, transport, PrimaryStatus::Ok { percent }),
                "pin {primary:?}"
            );

            let json = super::render::to_waybar(&state.devices, &cfg, now);
            assert_eq!(json["percentage"], percent, "pin {primary:?}");
        }
    }
}
