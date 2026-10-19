use std::time::Duration;

use futures_util::StreamExt as _;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zbus::fdo::{DBusProxy, NameOwnerChangedStream};
use zbus::names::BusName;

use super::FIRST_SWEEP_WAIT;
use crate::domain::{BatteryReading, BootTime, DeviceInfo, DeviceState, Estimate, TrayState};
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
