use std::time::Duration;

use std::sync::Arc;

use tokio::sync::{Notify, mpsc, watch};
use tokio::time::sleep;

use crate::domain::{BatteryReading, DeviceInfo, PrimaryStatus, classify};
use crate::sources::BatterySource;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
pub const LOW_THRESHOLD: u8 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub devices: Vec<(DeviceInfo, Option<BatteryReading>)>,
    pub primary: Option<usize>,
    pub primary_status: PrimaryStatus,
}

pub struct Supervisor;

impl Supervisor {
    /// Spawns background polling tasks and the aggregator.
    /// Returns a watch receiver for the current state and a Notify handle that
    /// immediately re-polls all sources when triggered (e.g. on system resume).
    /// Tasks live until the runtime terminates.
    pub fn spawn(
        sources: Vec<Box<dyn BatterySource>>,
    ) -> (watch::Receiver<TrayState>, Arc<Notify>) {
        let device_infos: Vec<DeviceInfo> = sources.iter().map(|s| s.device().clone()).collect();

        let initial = build_initial_state(&device_infos);
        let (watch_tx, watch_rx) = watch::channel(initial);

        let refresh = Arc::new(Notify::new());

        if sources.is_empty() {
            return (watch_rx, refresh);
        }

        let (mpsc_tx, mut mpsc_rx) = mpsc::channel::<(usize, Option<BatteryReading>)>(32);

        for (i, mut src) in sources.into_iter().enumerate() {
            let tx = mpsc_tx.clone();
            let refresh = refresh.clone();
            tokio::spawn(async move {
                loop {
                    let reading = src.poll().await.ok();
                    if tx.send((i, reading)).await.is_err() {
                        return;
                    }
                    // Wait for the normal poll interval or an early wake-up from refresh.
                    tokio::select! {
                        _ = sleep(POLL_INTERVAL) => {}
                        _ = refresh.notified() => {}
                    }
                }
            });
        }

        // Aggregator: receives updates from sources, recalculates state.
        let n = device_infos.len();
        tokio::spawn(async move {
            let mut readings: Vec<Option<BatteryReading>> = vec![None; n];

            while let Some((i, reading)) = mpsc_rx.recv().await {
                readings[i] = reading;

                let primary = compute_primary(&readings);
                let primary_status = primary
                    .map(|idx| classify(readings[idx], LOW_THRESHOLD))
                    .unwrap_or(PrimaryStatus::Offline);

                let devices = device_infos
                    .iter()
                    .cloned()
                    .zip(readings.iter().copied())
                    .collect();

                let state = TrayState {
                    devices,
                    primary,
                    primary_status,
                };

                // If the receiver is closed, terminate the task.
                if watch_tx.send(state).is_err() {
                    return;
                }
            }
        });

        (watch_rx, refresh)
    }
}

fn build_initial_state(device_infos: &[DeviceInfo]) -> TrayState {
    TrayState {
        devices: device_infos.iter().map(|d| (d.clone(), None)).collect(),
        primary: (!device_infos.is_empty()).then_some(0),
        primary_status: PrimaryStatus::Offline,
    }
}

/// Returns the index of the first device with a Some reading,
/// or 0 (if devices exist), or None.
fn compute_primary(readings: &[Option<BatteryReading>]) -> Option<usize> {
    if readings.is_empty() {
        return None;
    }
    readings.iter().position(|r| r.is_some()).or(Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};
    use crate::sources::BatterySource;
    use tokio::time::timeout;

    struct OkSource {
        info: DeviceInfo,
        reading: BatteryReading,
    }

    struct ErrSource {
        info: DeviceInfo,
    }

    #[async_trait::async_trait]
    impl BatterySource for OkSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            Ok(self.reading)
        }
    }

    #[async_trait::async_trait]
    impl BatterySource for ErrSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            anyhow::bail!("device unavailable")
        }
    }

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            kind: DeviceKind::Other,
            transport: crate::domain::Transport::Sysfs,
            locator: None,
        }
    }

    fn reading_discharging(percent: u8) -> BatteryReading {
        BatteryReading::new(percent, ChargeState::Discharging)
    }

    /// Waits for a state where primary_status != Offline, with a timeout.
    async fn wait_for_connected(rx: &mut watch::Receiver<TrayState>) -> TrayState {
        timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let state = rx.borrow().clone();
                    if state.primary_status != PrimaryStatus::Offline {
                        return state;
                    }
                }
                rx.changed().await.expect("watch channel closed");
            }
        })
        .await
        .expect("timed out waiting for connected state")
    }

    /// [Err, Ok(80% discharging)] → primary == Some(1), primary_status Ok{80}
    #[tokio::test]
    async fn primary_is_first_connected() {
        let sources: Vec<Box<dyn BatterySource>> = vec![
            Box::new(ErrSource {
                info: device("mouse"),
            }),
            Box::new(OkSource {
                info: device("keyboard"),
                reading: reading_discharging(80),
            }),
        ];

        let (mut rx, _refresh) = Supervisor::spawn(sources);
        let state = wait_for_connected(&mut rx).await;

        assert_eq!(state.primary, Some(1));
        assert_eq!(state.primary_status, PrimaryStatus::Ok { percent: 80 });
    }

    /// All sources Err → primary_status Offline, primary Some(0)
    #[tokio::test]
    async fn all_err_stays_offline() {
        let sources: Vec<Box<dyn BatterySource>> = vec![
            Box::new(ErrSource {
                info: device("mouse"),
            }),
            Box::new(ErrSource {
                info: device("keyboard"),
            }),
        ];

        let (rx, _refresh) = Supervisor::spawn(sources);

        // Sources are instant — give the scheduler a chance to run tasks.
        // yield_now guarantees that all ready tasks will be executed.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let state = rx.borrow().clone();
        assert_eq!(state.primary, Some(0));
        assert_eq!(state.primary_status, PrimaryStatus::Offline);
    }

    /// spawn(vec![]) → devices empty, primary None, primary_status Offline
    #[tokio::test]
    async fn empty_sources() {
        let (rx, _refresh) = Supervisor::spawn(vec![]);
        let state = rx.borrow().clone();

        assert!(state.devices.is_empty());
        assert_eq!(state.primary, None);
        assert_eq!(state.primary_status, PrimaryStatus::Offline);
    }
}
