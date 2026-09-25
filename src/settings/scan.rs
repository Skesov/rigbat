//! Where the settings window's device list comes from. SteelSeries devices
//! answer request/response, so a second process polling one can interleave
//! with the tray's own exchange: while a tray runs, its roster is read over
//! the session bus, and only without one does this process poll.

use std::future::Future;
use std::time::Duration;

use futures_util::StreamExt as _;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;

use crate::domain::{BatteryReading, DeviceInfo, PollOutcome, Presence, roster_order};
use crate::ipc::{DeviceCard, Snapshot, TRAY_NAME, Tray1Proxy};

const STATE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a Refresh waits for the tray's re-poll before reading what is there.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(3);

pub type Discovered = Vec<(DeviceInfo, PollOutcome)>;

/// What a scan asks of a running tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scan {
    /// Read the roster as it stands.
    Read,
    /// Have the tray re-poll first (the Refresh button).
    Refresh,
}

/// The running tray's roster, or what `poll_locally` finds when no tray runs.
/// Fails when a tray runs but does not answer: polling next to it is what
/// this avoids, so there is nothing to show but what was shown before.
pub async fn scan_devices<F, Fut>(scan: Scan, poll_locally: F) -> anyhow::Result<Discovered>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Discovered>,
{
    match tray_roster(scan).await {
        TrayRoster::Absent => Ok(poll_locally().await),
        TrayRoster::Unreadable(e) => {
            tracing::warn!("the running tray did not list its devices: {e:#}");
            Err(e)
        }
        TrayRoster::Read(snapshot) => Ok(discovered(snapshot)),
    }
}

enum TrayRoster {
    Absent,
    /// A tray runs but did not answer; polling next to it is what this avoids.
    Unreadable(anyhow::Error),
    Read(Snapshot),
}

async fn tray_roster(scan: Scan) -> TrayRoster {
    let Ok(conn) = zbus::Connection::session().await else {
        return TrayRoster::Absent;
    };
    match tray_running(&conn).await {
        Ok(false) => TrayRoster::Absent,
        Ok(true) => match read_state(&conn, scan).await {
            Ok(snapshot) => TrayRoster::Read(snapshot),
            Err(e) => TrayRoster::Unreadable(e),
        },
        Err(e) => TrayRoster::Unreadable(e),
    }
}

async fn tray_running(conn: &zbus::Connection) -> anyhow::Result<bool> {
    let dbus = DBusProxy::new(conn).await?;
    Ok(dbus.name_has_owner(BusName::try_from(TRAY_NAME)?).await?)
}

async fn read_state(conn: &zbus::Connection, scan: Scan) -> anyhow::Result<Snapshot> {
    let tray = Tray1Proxy::new(conn).await?;
    if scan == Scan::Refresh
        && let Err(e) = repoll(&tray).await
    {
        tracing::warn!("the running tray did not re-poll: {e:#}");
    }
    let json = tokio::time::timeout(STATE_TIMEOUT, tray.state())
        .await
        .map_err(|_| anyhow::anyhow!("no answer within {}s", STATE_TIMEOUT.as_secs()))??;
    Ok(serde_json::from_str(&json)?)
}

/// Asks the tray to re-poll and waits, bounded, for the state that follows.
async fn repoll(tray: &Tray1Proxy<'_>) -> anyhow::Result<()> {
    let mut changes = tray.receive_state_changed().await?;
    tray.refresh().await?;
    // On timeout the tray's current state is still worth reading.
    let _ = tokio::time::timeout(REFRESH_TIMEOUT, changes.next()).await;
    Ok(())
}

fn discovered(snapshot: Snapshot) -> Discovered {
    let mut rows: Discovered = snapshot
        .devices
        .into_iter()
        .chain(snapshot.hidden)
        .filter_map(row)
        .collect();
    rows.sort_by_cached_key(|(info, outcome)| roster_order(&info.name, outcome.presence()));
    rows
}

/// A card as the outcome a local poll would have given; a disconnected device
/// is one discovery did not find, so it is no row at all.
fn row(card: DeviceCard) -> Option<(DeviceInfo, PollOutcome)> {
    let outcome = match card.presence {
        Presence::Online => match (card.percent, card.charge) {
            (Some(percent), Some(state)) => {
                PollOutcome::Reading(BatteryReading::new(percent, state))
            }
            _ => PollOutcome::Failed,
        },
        Presence::Unreachable => PollOutcome::Failed,
        Presence::NoAccess => PollOutcome::NoAccess,
        Presence::Disconnected => return None,
    };
    let info = DeviceInfo {
        name: card.name,
        kind: card.kind,
        transport: card.transport,
        locator: card.locator,
    };
    Some((info, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ChargeState, DeviceKind, DisplayMode, PrimaryStatus, Transport};

    fn card(name: &str, presence: Presence, percent: Option<u8>) -> DeviceCard {
        DeviceCard {
            name: name.to_owned(),
            kind: DeviceKind::Mouse,
            transport: Transport::Hidraw,
            locator: Some(format!("{name}-serial")),
            presence,
            percent,
            charge: percent.map(|_| ChargeState::Discharging),
            status: PrimaryStatus::Offline,
            stale: false,
            seen_secs_ago: None,
            remaining_secs: None,
            in_tray: false,
        }
    }

    #[test]
    fn cards_read_as_the_outcomes_a_local_poll_gives() {
        let snapshot = Snapshot {
            display_mode: DisplayMode::IconOnly,
            devices: vec![
                card("online", Presence::Online, Some(80)),
                card("retained", Presence::Unreachable, Some(40)),
                card("gone", Presence::Disconnected, Some(90)),
            ],
            hidden: vec![card("denied", Presence::NoAccess, None)],
        };

        let rows = discovered(snapshot);

        let outcomes: Vec<(&str, Option<&str>, PollOutcome)> = rows
            .iter()
            .map(|(info, outcome)| (info.name.as_str(), info.locator.as_deref(), *outcome))
            .collect();
        assert_eq!(
            outcomes,
            [
                (
                    "online",
                    Some("online-serial"),
                    PollOutcome::Reading(BatteryReading::new(80, ChargeState::Discharging))
                ),
                ("denied", Some("denied-serial"), PollOutcome::NoAccess),
                ("retained", Some("retained-serial"), PollOutcome::Failed),
            ]
        );
    }
}

#[cfg(test)]
mod bus_tests {
    use std::cell::Cell;
    use std::time::Duration;

    use tokio::sync::watch;

    use super::*;
    use crate::bus_test::{eventually, isolated};
    use crate::config::Config;
    use crate::domain::{ChargeState, DeviceKind, DeviceState, Estimate, Transport, TrayState};
    use crate::refresh::RefreshSignal;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            kind: DeviceKind::Mouse,
            transport: Transport::Hidraw,
            locator: Some(format!("{name}-serial")),
        }
    }

    fn local_poll() -> Discovered {
        vec![(device("local"), PollOutcome::Failed)]
    }

    fn online(name: &str) -> DeviceState {
        DeviceState {
            info: device(name),
            last_reading: Some(BatteryReading::new(70, ChargeState::Discharging)),
            last_seen: Some(crate::clock::now()),
            presence: Presence::Online,
            estimate: Estimate::Unknown,
        }
    }

    /// Serves the real `org.rigbat.Tray1` under the tray's name, and returns once it answers.
    async fn serve_tray(
        state: watch::Receiver<TrayState>,
        config: watch::Receiver<Config>,
        refresh: RefreshSignal,
    ) {
        let conn = zbus::connection::Builder::session()
            .expect("private bus")
            .name(TRAY_NAME)
            .expect("name")
            .build()
            .await
            .expect("claiming the tray name");
        tokio::spawn(crate::tray::state_service::serve(
            conn, state, config, refresh,
        ));
        let client = zbus::Connection::session().await.expect("private bus");
        let tray = Tray1Proxy::new(&client).await.expect("Tray1 proxy");
        eventually(TIMEOUT, || async { tray.state().await.ok() }).await;
    }

    #[tokio::test]
    async fn a_running_tray_is_read_instead_of_polled() {
        if !isolated(module_path!(), "a_running_tray_is_read_instead_of_polled") {
            return;
        }
        let (_state_tx, state_rx) = watch::channel(TrayState {
            devices: vec![online("mouse"), online("keyboard")],
        });
        let (_config_tx, config_rx) = watch::channel(Config {
            hidden_devices: vec!["keyboard".to_owned()],
            ..Config::default()
        });
        serve_tray(state_rx, config_rx, RefreshSignal::new()).await;

        let polled = Cell::new(false);
        let rows = scan_devices(Scan::Read, || {
            polled.set(true);
            async { local_poll() }
        })
        .await
        .expect("a scan");

        assert!(!polled.get(), "polled the devices next to a running tray");
        let mut ids: Vec<_> = rows.into_iter().map(|(info, _)| info).collect();
        ids.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(ids, [device("keyboard"), device("mouse")]);
    }

    #[tokio::test]
    async fn a_refresh_waits_for_the_trays_next_state() {
        if !isolated(module_path!(), "a_refresh_waits_for_the_trays_next_state") {
            return;
        }
        let (state_tx, state_rx) = watch::channel(TrayState {
            devices: vec![online("mouse")],
        });
        let (_config_tx, config_rx) = watch::channel(Config::default());
        let refresh = RefreshSignal::new();
        let mut refreshed = refresh.waiter();
        serve_tray(state_rx, config_rx, refresh).await;
        tokio::spawn(async move {
            refreshed.wait().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            state_tx.send_replace(TrayState {
                devices: vec![online("mouse"), online("keyboard")],
            });
            // Dropping the sender would end the tray's service.
            std::future::pending::<()>().await;
        });

        let rows = scan_devices(Scan::Refresh, || async { local_poll() })
            .await
            .expect("a scan");

        let names: Vec<_> = rows.into_iter().map(|(info, _)| info.name).collect();
        assert_eq!(names, ["keyboard", "mouse"]);
    }

    /// A tray that holds its name but does not answer is neither polled
    /// around nor taken to have no devices.
    #[tokio::test]
    async fn a_tray_that_does_not_answer_fails_the_scan() {
        if !isolated(module_path!(), "a_tray_that_does_not_answer_fails_the_scan") {
            return;
        }
        let _silent_tray = zbus::connection::Builder::session()
            .expect("private bus")
            .name(TRAY_NAME)
            .expect("name")
            .build()
            .await
            .expect("claiming the tray name");

        let polled = Cell::new(false);
        let scanned = scan_devices(Scan::Read, || {
            polled.set(true);
            async { local_poll() }
        })
        .await;

        assert!(!polled.get(), "polled the devices next to a running tray");
        assert!(scanned.is_err(), "an unanswered scan read as {scanned:?}");
    }

    #[tokio::test]
    async fn without_a_tray_the_devices_are_polled() {
        if !isolated(module_path!(), "without_a_tray_the_devices_are_polled") {
            return;
        }
        let polled = Cell::new(false);
        let rows = scan_devices(Scan::Read, || {
            polled.set(true);
            async { local_poll() }
        })
        .await
        .expect("a scan");

        assert!(polled.get());
        assert_eq!(rows, local_poll());
    }
}
