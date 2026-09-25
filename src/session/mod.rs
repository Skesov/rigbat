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
