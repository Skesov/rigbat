/// Starts `rigbat <subcommand>` as a window process of its own.
pub(super) fn launch(subcommand: &'static str) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            tracing::error!("cannot find own executable: {e}");
            return;
        }
    };
    match std::process::Command::new(exe).arg(subcommand).spawn() {
        // `Child` does not reap on drop: without the wait, every closed window
        // stays a zombie for the tray's lifetime. The wait blocks, so it gets a
        // thread of its own rather than the tray's.
        Ok(mut child) => {
            std::thread::spawn(move || {
                if let Err(e) = child.wait() {
                    tracing::warn!("{subcommand} process could not be reaped: {e}");
                }
            });
        }
        Err(e) => tracing::error!("failed to launch rigbat {subcommand}: {e}"),
    }
}
