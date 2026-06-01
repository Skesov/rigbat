use std::sync::Arc;

use futures_util::StreamExt as _;
use tokio::sync::Notify;

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
/// environments without systemd (e.g. containers, BSDs).
pub fn watch_resume(refresh: Arc<Notify>) {
    tokio::spawn(async move {
        let Ok(conn) = zbus::Connection::system().await else {
            return;
        };
        let Ok(manager) = LogindManagerProxy::new(&conn).await else {
            return;
        };
        let Ok(mut stream) = manager.receive_prepare_for_sleep().await else {
            return;
        };
        while let Some(signal) = stream.next().await {
            if let Ok(args) = signal.args() {
                // start == false: the system has finished resuming from sleep.
                if !args.start {
                    refresh.notify_waiters();
                }
            }
        }
    });
}
