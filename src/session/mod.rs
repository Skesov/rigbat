use std::sync::Arc;

use anyhow::Context as _;
use futures_util::StreamExt as _;

use crate::app::refresh::RefreshSignal;
use crate::discovery::{Context, backoff};

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
/// (`discovery::backoff`): a lost system bus or a logind that stops emitting
/// is not fatal, the loop re-dials through `ctx.system_bus()` (which
/// re-connects a closed one) and resubscribes. An environment with no logind
/// (containers, BSDs) just keeps retrying quietly in the background — this
/// is an optimisation, never a dependency.
pub fn watch_resume(refresh: RefreshSignal, ctx: Arc<Context>) {
    tokio::spawn(async move {
        let mut delay = backoff::INITIAL_DELAY;
        let mut consecutive_failures: u32 = 0;
        loop {
            let attempt_start = tokio::time::Instant::now();
            let result: anyhow::Result<()> = async {
                let conn = ctx.system_bus().await?;
                watch_resume_inner(refresh.clone(), conn).await
            }
            .await;

            if backoff::is_healthy_run(attempt_start.elapsed()) {
                delay = backoff::INITIAL_DELAY;
                consecutive_failures = 0;
            }

            let detail = match &result {
                Ok(()) => "stream ended".to_owned(),
                Err(e) => format!("{e:#}"),
            };
            if consecutive_failures == 0 {
                tracing::warn!("resume watcher stopped: {detail}");
            } else {
                tracing::debug!(
                    failures = consecutive_failures,
                    "resume watcher stopped: {detail}"
                );
            }
            consecutive_failures = consecutive_failures.saturating_add(1);

            tokio::time::sleep(delay).await;
            delay = backoff::next_delay(delay);
        }
    });
}
