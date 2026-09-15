use futures_util::StreamExt as _;

use crate::app::refresh::RefreshSignal;

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

/// Spawns a background task that fires `refresh` whenever the system resumes.
/// Quietly exits if logind is unavailable so the main binary still works in
/// environments without systemd (e.g. containers, BSDs). `conn` is the
/// process-wide system-bus connection, opened by the caller.
pub fn watch_resume(refresh: RefreshSignal, conn: zbus::Connection) {
    tokio::spawn(async move {
        let manager = match LogindManagerProxy::new(&conn).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("logind unavailable: {e}; no re-poll after suspend");
                return;
            }
        };
        let mut stream = match manager.receive_prepare_for_sleep().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("logind unavailable: {e}; no re-poll after suspend");
                return;
            }
        };
        while let Some(signal) = stream.next().await {
            if let Ok(args) = signal.args() {
                // start == false: the system has finished resuming from sleep.
                if !args.start {
                    tracing::info!("resumed from suspend, triggering re-poll");
                    refresh.trigger();
                }
            }
        }
    });
}
