use std::sync::Arc;

use anyhow::Context as _;
use futures_util::StreamExt as _;

use crate::refresh::RefreshSignal;
use crate::sources::Context;
use crate::sources::supervise::supervise;

/// zbus proxy for the logind Manager interface.
/// Used only for the PrepareForSleep signal.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LogindManager {
    /// Emitted before suspend (start == true) and after resume (start == false).
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

async fn watch_resume_inner(refresh: RefreshSignal, conn: zbus::Connection) -> anyhow::Result<()> {
    let manager = LogindManagerProxy::new(&conn)
        .await
        .context("building logind manager proxy")?;
    let mut stream = manager
        .receive_prepare_for_sleep()
        .await
        .context("subscribing to PrepareForSleep")?;
    while let Some(signal) = stream.next().await {
        if let Ok(args) = signal.args()
            // start == false: the system has finished resuming from sleep.
            && !args.start
        {
            tracing::info!("resumed from suspend, triggering re-poll");
            refresh.trigger();
        }
    }
    Ok(())
}

/// Spawns a background task that fires `refresh` whenever the system resumes.
/// Runs under a supervising retry loop with exponential backoff
/// (`sources::supervise`): a lost system bus or a logind that stops emitting
/// is not fatal, the loop re-dials through `ctx.system_bus()` (which
/// re-connects a closed one) and resubscribes. An environment with no logind
/// (containers, BSDs) just keeps retrying quietly in the background — this
/// is an optimisation, never a dependency.
pub fn watch_resume(refresh: RefreshSignal, ctx: Arc<Context>) {
    supervise("resume watcher", ctx, move |conn| {
        watch_resume_inner(refresh.clone(), conn)
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zbus::object_server::SignalEmitter;

    use super::watch_resume_inner;
    use crate::bus_test::isolated;
    use crate::refresh::RefreshSignal;

    const LOGIND_PATH: &str = "/org/freedesktop/login1";
    const TIMEOUT: Duration = Duration::from_secs(5);
    /// Long enough for a signal already on the wire to reach the watcher.
    const QUIET: Duration = Duration::from_millis(300);

    struct FakeLogind;

    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl FakeLogind {
        #[zbus(signal)]
        async fn prepare_for_sleep(emitter: &SignalEmitter<'_>, start: bool) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn resume_triggers_a_refresh_and_suspend_does_not() {
        if !isolated(
            module_path!(),
            "resume_triggers_a_refresh_and_suspend_does_not",
        ) {
            return;
        }
        let logind = zbus::connection::Builder::session()
            .expect("private bus")
            .name("org.freedesktop.login1")
            .expect("name")
            .serve_at(LOGIND_PATH, FakeLogind)
            .expect("path")
            .build()
            .await
            .expect("fake logind");
        let emitter = SignalEmitter::new(&logind, LOGIND_PATH).expect("emitter");
        let refresh = RefreshSignal::new();
        let mut waiter = refresh.waiter();
        let conn = zbus::Connection::session().await.expect("private bus");
        tokio::spawn(watch_resume_inner(refresh.clone(), conn));

        // The watcher subscribes after it starts, so an early signal can go unheard.
        let resumed = tokio::time::timeout(TIMEOUT, async {
            loop {
                FakeLogind::prepare_for_sleep(&emitter, false)
                    .await
                    .expect("PrepareForSleep");
                if tokio::time::timeout(Duration::from_millis(20), waiter.wait())
                    .await
                    .is_ok()
                {
                    break;
                }
            }
        })
        .await;
        assert!(resumed.is_ok(), "PrepareForSleep(false) never refreshed");
        // A fresh waiter, once the probes still in flight have landed.
        tokio::time::sleep(QUIET).await;
        let mut waiter = refresh.waiter();

        FakeLogind::prepare_for_sleep(&emitter, true)
            .await
            .expect("PrepareForSleep");
        assert!(
            tokio::time::timeout(QUIET, waiter.wait()).await.is_err(),
            "PrepareForSleep(true) refreshed"
        );

        FakeLogind::prepare_for_sleep(&emitter, false)
            .await
            .expect("PrepareForSleep");
        assert!(
            tokio::time::timeout(TIMEOUT, waiter.wait()).await.is_ok(),
            "a second resume did not refresh"
        );
    }
}
