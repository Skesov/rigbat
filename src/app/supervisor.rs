use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::sleep;

use crate::config::Config;
use crate::domain::{BatteryReading, DeviceId, DeviceInfo, PrimaryStatus, classify};
use crate::sources::BatterySource;

const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub devices: Vec<(DeviceInfo, Option<BatteryReading>)>,
    pub primary: Option<usize>,
    pub primary_status: PrimaryStatus,
}

pub struct Supervisor;

impl Supervisor {
    /// Spawns the manager with the real `discovery::discover_all` backend.
    pub fn spawn(config: Config) -> (watch::Receiver<TrayState>, Arc<Notify>) {
        Self::spawn_with(config, || Box::pin(crate::discovery::discover_all()))
    }

    /// Injectable discovery for tests. `discover` is called once at start, then
    /// on every discovery tick and every `refresh` notification.
    pub fn spawn_with<F, Fut>(
        config: Config,
        discover: F,
    ) -> (watch::Receiver<TrayState>, Arc<Notify>)
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Vec<Box<dyn BatterySource>>> + Send + 'static,
    {
        let initial = TrayState {
            devices: vec![],
            primary: None,
            primary_status: PrimaryStatus::Offline,
        };
        let (watch_tx, watch_rx) = watch::channel(initial);
        let refresh = Arc::new(Notify::new());

        let refresh_inner = refresh.clone();
        tokio::spawn(manager_task(config, discover, watch_tx, refresh_inner));

        (watch_rx, refresh)
    }
}

async fn manager_task<F, Fut>(
    config: Config,
    discover: F,
    watch_tx: watch::Sender<TrayState>,
    refresh: Arc<Notify>,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Vec<Box<dyn BatterySource>>> + Send + 'static,
{
    let (mpsc_tx, mut mpsc_rx) = mpsc::channel::<(DeviceId, Option<BatteryReading>)>(64);

    let mut order: Vec<DeviceId> = Vec::new();
    let mut infos: HashMap<DeviceId, DeviceInfo> = HashMap::new();
    let mut readings: HashMap<DeviceId, Option<BatteryReading>> = HashMap::new();
    let mut tasks: HashMap<DeviceId, AbortHandle> = HashMap::new();

    // Initial discovery.
    let fresh = discover().await;
    reconcile(
        fresh,
        &mut order,
        &mut infos,
        &mut readings,
        &mut tasks,
        &mpsc_tx,
        &config,
        &refresh,
    );
    publish(&order, &infos, &readings, &config, &watch_tx);

    loop {
        tokio::select! {
            msg = mpsc_rx.recv() => {
                match msg {
                    Some((id, reading)) => {
                        readings.insert(id, reading);
                        publish(&order, &infos, &readings, &config, &watch_tx);
                    }
                    // All source tasks dropped their senders — nothing left to do.
                    None => return,
                }
            }
            _ = sleep(DISCOVERY_INTERVAL) => {
                let fresh = discover().await;
                reconcile(
                    fresh,
                    &mut order,
                    &mut infos,
                    &mut readings,
                    &mut tasks,
                    &mpsc_tx,
                    &config,
                    &refresh,
                );
                publish(&order, &infos, &readings, &config, &watch_tx);
            }
            _ = refresh.notified() => {
                let fresh = discover().await;
                reconcile(
                    fresh,
                    &mut order,
                    &mut infos,
                    &mut readings,
                    &mut tasks,
                    &mpsc_tx,
                    &config,
                    &refresh,
                );
                publish(&order, &infos, &readings, &config, &watch_tx);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile(
    fresh: Vec<Box<dyn BatterySource>>,
    order: &mut Vec<DeviceId>,
    infos: &mut HashMap<DeviceId, DeviceInfo>,
    readings: &mut HashMap<DeviceId, Option<BatteryReading>>,
    tasks: &mut HashMap<DeviceId, AbortHandle>,
    mpsc_tx: &mpsc::Sender<(DeviceId, Option<BatteryReading>)>,
    config: &Config,
    refresh: &Arc<Notify>,
) {
    let fresh_ids: Vec<DeviceId> = fresh.iter().map(|s| s.device().id()).collect();

    // Spawn tasks for new devices; drop fresh sources for already-running ones.
    for src in fresh {
        let id = src.device().id();
        match tasks.entry(id.clone()) {
            // Device already has a running task — drop the transient handle.
            std::collections::hash_map::Entry::Occupied(_) => drop(src),
            std::collections::hash_map::Entry::Vacant(slot) => {
                infos.insert(id.clone(), src.device().clone());
                readings.insert(id.clone(), None);
                order.push(id);
                let handle =
                    spawn_source_task(src, slot.key().clone(), mpsc_tx.clone(), config, refresh);
                slot.insert(handle);
            }
        }
    }

    // Abort tasks for devices that disappeared from discovery.
    let vanished: Vec<DeviceId> = tasks
        .keys()
        .filter(|id| !fresh_ids.contains(id))
        .cloned()
        .collect();
    for id in vanished {
        if let Some(handle) = tasks.remove(&id) {
            handle.abort();
        }
        infos.remove(&id);
        readings.remove(&id);
        order.retain(|oid| oid != &id);
    }
}

fn spawn_source_task(
    mut src: Box<dyn BatterySource>,
    id: DeviceId,
    tx: mpsc::Sender<(DeviceId, Option<BatteryReading>)>,
    config: &Config,
    refresh: &Arc<Notify>,
) -> AbortHandle {
    let name = src.device().name.clone();
    let interval = Duration::from_secs(config.effective_poll_interval_secs(&name));
    let refresh = refresh.clone();

    let handle = tokio::spawn(async move {
        loop {
            let reading = src.poll().await.ok();
            if tx.send((id.clone(), reading)).await.is_err() {
                return;
            }
            tokio::select! {
                _ = sleep(interval) => {}
                _ = refresh.notified() => {}
            }
        }
    });
    handle.abort_handle()
}

fn publish(
    order: &[DeviceId],
    infos: &HashMap<DeviceId, DeviceInfo>,
    readings: &HashMap<DeviceId, Option<BatteryReading>>,
    config: &Config,
    watch_tx: &watch::Sender<TrayState>,
) {
    let devices: Vec<(DeviceInfo, Option<BatteryReading>)> = order
        .iter()
        .filter_map(|id| {
            let info = infos.get(id)?.clone();
            let reading = readings.get(id).copied().flatten();
            Some((info, reading))
        })
        .collect();

    let primary = if devices.is_empty() {
        None
    } else {
        devices.iter().position(|(_, r)| r.is_some()).or(Some(0))
    };

    let primary_status = primary
        .map(|idx| {
            let (info, reading) = &devices[idx];
            let threshold = config.effective_low_threshold(&info.name);
            classify(*reading, threshold)
        })
        .unwrap_or(PrimaryStatus::Offline);

    let state = TrayState {
        devices,
        primary,
        primary_status,
    };

    // Ignore error — receiver closed means the tray exited.
    let _ = watch_tx.send(state);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::Config;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind, Transport};
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
            transport: Transport::Sysfs,
            locator: Some(name.to_owned()),
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

    /// Waits for TrayState.devices to be non-empty, with a timeout.
    async fn wait_for_devices(rx: &mut watch::Receiver<TrayState>) -> TrayState {
        timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let state = rx.borrow().clone();
                    if !state.devices.is_empty() {
                        return state;
                    }
                }
                rx.changed().await.expect("watch channel closed");
            }
        })
        .await
        .expect("timed out waiting for devices to appear")
    }

    /// [Err, Ok(80% discharging)] → primary == Some(1), primary_status Ok{80}
    #[tokio::test]
    async fn primary_is_first_connected() {
        let (mut rx, _refresh) = Supervisor::spawn_with(Config::default(), || {
            let sources: Vec<Box<dyn BatterySource>> = vec![
                Box::new(ErrSource {
                    info: device("mouse"),
                }),
                Box::new(OkSource {
                    info: device("keyboard"),
                    reading: reading_discharging(80),
                }),
            ];
            async move { sources }
        });

        let state = wait_for_connected(&mut rx).await;
        assert_eq!(state.primary, Some(1));
        assert_eq!(state.primary_status, PrimaryStatus::Ok { percent: 80 });
    }

    /// All sources Err → primary_status Offline, primary Some(0)
    #[tokio::test]
    async fn all_err_stays_offline() {
        let (rx, _refresh) = Supervisor::spawn_with(Config::default(), || {
            let sources: Vec<Box<dyn BatterySource>> = vec![
                Box::new(ErrSource {
                    info: device("mouse"),
                }),
                Box::new(ErrSource {
                    info: device("keyboard"),
                }),
            ];
            async move { sources }
        });

        // Sources are instant — give the scheduler a chance to run tasks.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let state = rx.borrow().clone();
        assert_eq!(state.primary, Some(0));
        assert_eq!(state.primary_status, PrimaryStatus::Offline);
    }

    /// spawn_with(|| vec![]) → devices empty, primary None, primary_status Offline
    #[tokio::test]
    async fn empty_sources() {
        let (rx, _refresh) = Supervisor::spawn_with(Config::default(), || async {
            Vec::<Box<dyn BatterySource>>::new()
        });

        // Give the manager task a chance to run its initial reconcile.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let state = rx.borrow().clone();
        assert!(state.devices.is_empty());
        assert_eq!(state.primary, None);
        assert_eq!(state.primary_status, PrimaryStatus::Offline);
    }

    /// First discovery returns []; after refresh it returns [OkSource("kbd", 70)].
    /// The device should appear in TrayState after the refresh.
    #[tokio::test]
    async fn hotplug_adds_device() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let (mut rx, refresh) = Supervisor::spawn_with(Config::default(), move || {
            let count = call_count_clone.fetch_add(1, Ordering::Relaxed);
            let sources: Vec<Box<dyn BatterySource>> = if count == 0 {
                vec![]
            } else {
                vec![Box::new(OkSource {
                    info: device("kbd"),
                    reading: reading_discharging(70),
                })]
            };
            async move { sources }
        });

        // Initial reconcile with empty list — give it time to settle.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let initial = rx.borrow().clone();
        assert!(initial.devices.is_empty());

        // Trigger re-discovery with the kbd present.
        refresh.notify_waiters();

        let state = wait_for_devices(&mut rx).await;
        assert_eq!(state.devices.len(), 1);
        assert_eq!(state.devices[0].0.name, "kbd");
    }
}
