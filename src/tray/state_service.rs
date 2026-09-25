//! Serves the tray's device state to `rigbat dashboard` as `org.rigbat.Tray1`.

use std::time::Instant;

use tokio::sync::watch;
use zbus::object_server::SignalEmitter;

use super::manager::featured_id;
use crate::app::refresh::RefreshSignal;
use crate::app::supervisor::TrayState;
use crate::config::{Config, TrayMode};
use crate::domain::{DeviceState, Estimate, device_status, is_visible};
use crate::ipc::{DeviceCard, Snapshot, TRAY_PATH};

/// Every device, classified exactly as its tray icon is; hidden ones apart.
pub fn snapshot(state: &TrayState, cfg: &Config, now: Instant) -> Snapshot {
    let featured = featured_id(state, cfg, now);
    let card = |d: &DeviceState, in_tray: bool| {
        let (status, stale) = device_status(d, cfg.effective_low_threshold(&d.info.name));
        DeviceCard {
            name: d.info.name.clone(),
            kind: d.info.kind,
            transport: d.info.transport,
            locator: d.info.locator.clone(),
            presence: d.presence,
            percent: d.last_reading.map(|r| r.percent),
            charge: d.last_reading.map(|r| r.state),
            status,
            stale,
            seen_secs_ago: d
                .last_seen
                .map(|seen| now.saturating_duration_since(seen).as_secs()),
            remaining_secs: match d.estimate {
                Estimate::Remaining(left) => Some(left.as_secs()),
                Estimate::Unknown | Estimate::Charging => None,
            },
            in_tray,
        }
    };
    let (shown, hidden): (Vec<&DeviceState>, Vec<&DeviceState>) = state
        .devices
        .iter()
        .partition(|d| cfg.is_shown(&d.info.name));
    let devices = shown
        .into_iter()
        .map(|d| {
            let in_tray = match cfg.tray_mode {
                TrayMode::PerDevice => is_visible(d, |name| cfg.is_shown(name), now),
                TrayMode::PrimaryOnly => featured.as_ref() == Some(&d.info.id()),
            };
            card(d, in_tray)
        })
        .collect();
    Snapshot {
        display_mode: cfg.display_mode,
        devices,
        hidden: hidden.into_iter().map(|d| card(d, false)).collect(),
    }
}

struct StateService {
    rx: watch::Receiver<TrayState>,
    config: watch::Receiver<Config>,
    refresh: RefreshSignal,
}

#[zbus::interface(name = "org.rigbat.Tray1")]
impl StateService {
    fn state(&self) -> zbus::fdo::Result<String> {
        let snapshot = snapshot(&self.rx.borrow(), &self.config.borrow(), Instant::now());
        serde_json::to_string(&snapshot).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    fn refresh(&self) {
        self.refresh.trigger();
    }

    #[zbus(signal)]
    async fn state_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Publishes the state on `conn` and signals every change until a channel closes.
pub async fn serve(
    conn: zbus::Connection,
    mut rx: watch::Receiver<TrayState>,
    mut config: watch::Receiver<Config>,
    refresh: RefreshSignal,
) {
    let service = StateService {
        rx: rx.clone(),
        config: config.clone(),
        refresh,
    };
    if let Err(e) = conn.object_server().at(TRAY_PATH, service).await {
        tracing::warn!("cannot serve the dashboard's state: {e}");
        return;
    }
    let iface = match conn
        .object_server()
        .interface::<_, StateService>(TRAY_PATH)
        .await
    {
        Ok(iface) => iface,
        Err(e) => {
            tracing::warn!("cannot serve the dashboard's state: {e}");
            return;
        }
    };
    loop {
        tokio::select! {
            r = rx.changed() => if r.is_err() { break; },
            r = config.changed() => if r.is_err() { break; },
        }
        if let Err(e) = StateService::state_changed(iface.signal_emitter()).await {
            tracing::debug!("StateChanged not sent: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::{
        BatteryReading, ChargeState, DeviceInfo, DeviceKind, DeviceState, Presence, PrimaryStatus,
        Transport,
    };

    fn device(name: &str, presence: Presence, percent: u8, seen: Instant) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                name: name.to_owned(),
                kind: DeviceKind::Mouse,
                transport: Transport::Sysfs,
                locator: None,
            },
            last_reading: Some(BatteryReading::new(percent, ChargeState::Discharging)),
            last_seen: Some(seen),
            presence,
            estimate: Estimate::Remaining(Duration::from_secs(3600)),
        }
    }

    #[test]
    fn hidden_devices_are_listed_apart() {
        let now = Instant::now();
        let state = TrayState {
            devices: vec![
                device("mouse", Presence::Online, 80, now),
                device("keyboard", Presence::Online, 50, now),
            ],
        };
        let cfg = Config {
            hidden_devices: vec!["keyboard".to_owned()],
            ..Config::default()
        };
        let snapshot = snapshot(&state, &cfg, now);
        let names = |cards: &[DeviceCard]| cards.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&snapshot.devices), ["mouse"]);
        assert_eq!(names(&snapshot.hidden), ["keyboard"]);
        assert!(!snapshot.hidden[0].in_tray);
    }

    #[test]
    fn a_retained_reading_is_classified_like_its_tray_icon() {
        let seen = Instant::now();
        let now = seen + Duration::from_secs(600);
        let state = TrayState {
            devices: vec![device("mouse", Presence::Unreachable, 15, seen)],
        };
        let card = &snapshot(&state, &Config::default(), now).devices[0];
        assert_eq!(card.status, PrimaryStatus::Low { percent: 15 });
        assert!(card.stale);
        assert_eq!(card.seen_secs_ago, Some(600));
        assert_eq!(card.remaining_secs, Some(3600));
    }

    #[test]
    fn in_tray_follows_the_tray_mode() {
        let now = Instant::now();
        let state = TrayState {
            devices: vec![
                device("mouse", Presence::Online, 80, now),
                device("keyboard", Presence::Online, 50, now),
            ],
        };
        let pinned = Config {
            primary_device: Some("keyboard".to_owned()),
            ..Config::default()
        };
        let in_tray: Vec<_> = snapshot(&state, &pinned, now)
            .devices
            .iter()
            .map(|c| c.in_tray)
            .collect();
        assert_eq!(in_tray, [false, true]);

        let per_device = Config {
            tray_mode: TrayMode::PerDevice,
            ..pinned
        };
        assert!(
            snapshot(&state, &per_device, now)
                .devices
                .iter()
                .all(|c| c.in_tray)
        );
    }

    /// The aggregate icon shows one device, so one card says so even when
    /// another device shares its name.
    #[test]
    fn in_tray_marks_one_of_two_same_named_devices() {
        let now = Instant::now();
        let mut bluetooth = device("mouse", Presence::Online, 40, now);
        bluetooth.info.transport = Transport::Bluetooth;
        let state = TrayState {
            devices: vec![device("mouse", Presence::Unreachable, 80, now), bluetooth],
        };
        let in_tray: Vec<_> = snapshot(&state, &Config::default(), now)
            .devices
            .iter()
            .map(|c| (c.transport, c.in_tray))
            .collect();
        assert_eq!(
            in_tray,
            [(Transport::Sysfs, false), (Transport::Bluetooth, true)]
        );
    }
}

#[cfg(test)]
mod bus_tests {
    use std::time::Duration;

    use futures_util::StreamExt as _;
    use tokio::time::timeout;

    use super::*;
    use crate::bus_test::{eventually, isolated};
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind, DeviceState};
    use crate::domain::{Presence, Transport};
    use crate::ipc::{TRAY_NAME, Tray1Proxy};

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn device(name: &str, percent: u8) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                name: name.to_owned(),
                kind: DeviceKind::Mouse,
                transport: Transport::Sysfs,
                locator: None,
            },
            last_reading: Some(BatteryReading::new(percent, ChargeState::Discharging)),
            last_seen: Some(Instant::now()),
            presence: Presence::Online,
            estimate: Estimate::Unknown,
        }
    }

    fn cards(json: &str) -> Vec<(String, Option<u8>)> {
        let snapshot: Snapshot = serde_json::from_str(json).expect("State returns a Snapshot");
        snapshot
            .devices
            .into_iter()
            .map(|c| (c.name, c.percent))
            .collect()
    }

    #[tokio::test]
    async fn serves_the_state_and_signals_every_change() {
        if !isolated(module_path!(), "serves_the_state_and_signals_every_change") {
            return;
        }
        let (state_tx, state_rx) = watch::channel(TrayState {
            devices: vec![device("mouse", 80)],
        });
        let (config_tx, config_rx) = watch::channel(Config::default());
        let refresh = RefreshSignal::new();
        let mut refreshed = refresh.waiter();
        let conn = zbus::connection::Builder::session()
            .expect("private bus")
            .name(TRAY_NAME)
            .expect("name")
            .build()
            .await
            .expect("claiming the tray name");
        tokio::spawn(serve(conn, state_rx, config_rx, refresh));

        let client = zbus::Connection::session().await.expect("private bus");
        let tray = Tray1Proxy::new(&client).await.expect("Tray1 proxy");
        let mut changes = tray.receive_state_changed().await.expect("subscribe");
        let json = eventually(TIMEOUT, || async { tray.state().await.ok() }).await;
        assert_eq!(cards(&json), [("mouse".to_owned(), Some(80))]);

        state_tx.send_replace(TrayState {
            devices: vec![device("mouse", 79), device("keyboard", 50)],
        });
        timeout(TIMEOUT, changes.next())
            .await
            .expect("StateChanged after a state change");
        let json = tray.state().await.expect("State");
        assert_eq!(
            cards(&json),
            [
                ("mouse".to_owned(), Some(79)),
                ("keyboard".to_owned(), Some(50))
            ]
        );

        config_tx.send_modify(|c| c.hidden_devices.push("keyboard".to_owned()));
        timeout(TIMEOUT, changes.next())
            .await
            .expect("StateChanged after a config change");
        let json = tray.state().await.expect("State");
        assert_eq!(cards(&json), [("mouse".to_owned(), Some(79))]);

        tray.refresh().await.expect("Refresh");
        timeout(TIMEOUT, refreshed.wait())
            .await
            .expect("Refresh re-polls the devices");
    }
}
