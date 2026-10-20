/// A runtime of its own for everything `start` spawns, transitively: the
/// `Supervisor`, its source tasks and their hidraw handles, the BlueZ and
/// resume watchers, the system-bus connection's tasks. A `Supervisor` has no
/// stop of its own; shutting this runtime down drops all of it at once.
pub struct OwnRuntime(Option<tokio::runtime::Runtime>);

impl OwnRuntime {
    pub fn start(start: impl FnOnce()) -> anyhow::Result<Self> {
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

pub async fn next_change(
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
