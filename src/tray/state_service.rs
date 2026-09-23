//! Serves the tray's device state to `rigbat dashboard` as `org.rigbat.Tray1`.

use std::time::Instant;

use tokio::sync::watch;
use zbus::object_server::SignalEmitter;

use super::manager::{featured_name, tray_visible};
use crate::app::refresh::RefreshSignal;
use crate::app::supervisor::TrayState;
use crate::config::{Config, TrayMode};
use crate::domain::{Estimate, device_status};
use crate::ipc::{DeviceCard, Snapshot, TRAY_PATH};

/// Every shown device, classified exactly as its tray icon is.
pub fn snapshot(state: &TrayState, cfg: &Config, now: Instant) -> Snapshot {
    let featured = featured_name(state, cfg, now);
    let devices = state
        .devices
        .iter()
        .filter(|d| cfg.is_shown(&d.info.name))
        .map(|d| {
            let (status, stale) = device_status(d, cfg.effective_low_threshold(&d.info.name));
            let in_tray = match cfg.tray_mode {
                TrayMode::PerDevice => tray_visible(d, cfg, now),
                TrayMode::PrimaryOnly => featured.as_deref() == Some(d.info.name.as_str()),
            };
            DeviceCard {
                name: d.info.name.clone(),
                kind: d.info.kind,
                transport: d.info.transport,
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
        })
        .collect();
    Snapshot {
        display_mode: cfg.display_mode,
        devices,
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
    fn hidden_devices_are_left_out() {
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
        let names: Vec<_> = snapshot(&state, &cfg, now)
            .devices
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["mouse"]);
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
}
