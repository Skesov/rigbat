use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::domain::TrayState;

/// Runs `--waybar` as a long-lived process instead of a one-shot poll, so a
/// sleeping Bluetooth peripheral keeps showing its retained reading instead
/// of `offline` on every bar tick — "If no interval or signal is defined, it
/// is assumed that the out script loops itself" (`waybar-custom(5)`).
///
/// Modeled on `run_tray`, minus the icon/appearance/notifications stack: the
/// bar has no icon to render, and the tray process already owns low-battery
/// notifications, so spawning a second notifier here would double them up.
///
/// `start_own` starts polling in this process when no tray runs: it is called
/// inside an `OwnRuntime` with the state and config senders to publish to.
pub async fn serve(start_own: impl Fn(watch::Sender<TrayState>, watch::Sender<Config>)) {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rigbat waybar module"
    );

    let config = crate::config::load();
    let (config_tx, _config_rx) = watch::channel(config);
    crate::config::watch_file(config_tx.clone());

    let conn = zbus::Connection::session()
        .await
        .map_err(|e| tracing::warn!("no session bus, polling the devices here: {e}"))
        .ok();
    let (tray_running, mut tray_changes) = match &conn {
        Some(conn) => match super::watch_tray(conn).await {
            Ok((running, changes)) => (running, Some(changes)),
            Err(e) => {
                tracing::warn!("cannot tell whether the tray runs: {e:#}");
                (false, None)
            }
        },
        None => (false, None),
    };
    let (state_tx, state_rx) = watch::channel(TrayState { devices: vec![] });
    let mut current = source(
        conn.as_ref(),
        tray_running,
        &state_tx,
        &config_tx,
        &start_own,
    )
    .await;

    // A receiver of our own, so a primary_device/hidden_devices edit is picked
    // up between two TrayState publications; config_tx outlives the loop, as in run_tray.
    let module = super::run(state_rx, config_tx.subscribe());
    tokio::pin!(module);
    loop {
        tokio::select! {
            () = &mut module => return,
            Some(change) = next_change(&mut tray_changes) => {
                let running = change.args().is_ok_and(|args| args.new_owner().is_some());
                if current.as_ref().is_some_and(|s| s.is_tray() == running) {
                    continue;
                }
                tracing::info!(tray_running = running, "switching the waybar module's source");
                // The old source stops before the new one starts: never two pollers at once.
                drop(current.take());
                current = source(conn.as_ref(), running, &state_tx, &config_tx, &start_own).await;
            }
        }
    }
}

/// Where `--waybar` reads device state from; dropping it stops all of it.
enum Source {
    Tray(JoinHandle<()>),
    Own { _runtime: OwnRuntime },
}

impl Source {
    fn is_tray(&self) -> bool {
        matches!(self, Self::Tray(_))
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Self::Tray(task) = self {
            task.abort();
        }
    }
}

/// The running tray's state when one runs, else `start_own`'s polling.
async fn source(
    conn: Option<&zbus::Connection>,
    tray_running: bool,
    tx: &watch::Sender<TrayState>,
    config_tx: &watch::Sender<Config>,
    start_own: &impl Fn(watch::Sender<TrayState>, watch::Sender<Config>),
) -> Option<Source> {
    if tray_running && let Some(conn) = conn {
        match super::follow_tray(conn, tx.clone()).await {
            Ok(task) => {
                tracing::info!("following the running tray's state");
                return Some(Source::Tray(task));
            }
            Err(e) => tracing::warn!("cannot follow the running tray: {e:#}"),
        }
    }
    match OwnRuntime::start(|| start_own(tx.clone(), config_tx.clone())) {
        Ok(rt) => {
            tracing::info!("no tray runs, polling the devices here");
            Some(Source::Own { _runtime: rt })
        }
        Err(e) => {
            tracing::error!("cannot start polling the devices: {e:#}");
            None
        }
    }
}

/// A runtime of its own for everything `start` spawns, transitively: the
/// `Supervisor`, its source tasks and their hidraw handles, the BlueZ and
/// resume watchers, the system-bus connection's tasks. A `Supervisor` has no
/// stop of its own; shutting this runtime down drops all of it at once.
struct OwnRuntime(Option<tokio::runtime::Runtime>);

impl OwnRuntime {
    fn start(start: impl FnOnce()) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("rigbat-poll")
            .enable_all()
            .build()?;
        {
            let _entered = rt.enter();
            start();
        }
        Ok(Self(Some(rt)))
    }
}

impl Drop for OwnRuntime {
    fn drop(&mut self) {
        // Not a plain drop: that blocks, which panics inside the async caller.
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

async fn next_change(
    changes: &mut Option<zbus::fdo::NameOwnerChangedStream>,
) -> Option<zbus::fdo::NameOwnerChanged> {
    use futures_util::StreamExt as _;
    match changes {
        Some(changes) => changes.next().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::OwnRuntime;

    fn open_fds_to(path: &std::path::Path) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .expect("/proc/self/fd")
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .filter(|target| target == path)
            .count()
    }

    /// Stopping the module's own polling leaves no task and no open device
    /// node behind, however deep the task that holds it was spawned.
    #[tokio::test]
    async fn dropping_the_own_runtime_stops_every_task_and_closes_its_handles() {
        let scratch = crate::test_support::ScratchDir::new("own-runtime");
        let node = scratch.join("node");
        std::fs::write(&node, b"").unwrap();
        let (alive_tx, mut alive_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();

        let rt = OwnRuntime::start({
            let node = node.clone();
            move || {
                tokio::spawn(async move {
                    let _alive = alive_tx.clone();
                    tokio::spawn(async move {
                        let _alive = alive_tx;
                        let _handle = std::fs::File::open(&node).unwrap();
                        opened_tx.send(()).unwrap();
                        std::future::pending::<()>().await;
                    });
                    std::future::pending::<()>().await;
                });
            }
        })
        .unwrap();
        opened_rx.await.unwrap();
        assert_eq!(open_fds_to(&node), 1);

        drop(rt);

        let closed = tokio::time::timeout(Duration::from_secs(5), alive_rx.recv()).await;
        assert_eq!(closed, Ok(None), "a task outlived the runtime");
        assert_eq!(open_fds_to(&node), 0, "the device handle stayed open");
    }
}
