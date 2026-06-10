mod app;
mod appearance;
mod autostart;
mod cli;
mod config;
mod discovery;
mod domain;
mod notifications;
mod session;
mod settings;
mod sources;
mod tray;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("list");

    if mode == "settings" {
        if let Err(e) = settings::run() {
            eprintln!("rigbat settings: {e}");
            std::process::exit(1);
        }
        return;
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("rigbat: failed to start runtime: {e}");
            std::process::exit(1);
        }
    };
    rt.block_on(async_main(mode));
}

async fn async_main(mode: &str) {
    if mode == "tray" {
        run_tray().await;
        return;
    }

    let sources = discovery::discover_all().await;

    let rows = app::poll_once(sources).await;

    if mode == "--json" {
        cli::print_json(&rows);
    } else {
        cli::print_table(&rows);
    }
}

async fn run_tray() {
    let sources = discovery::discover_all().await;

    let n = sources.len();
    println!("rigbat tray: {n} device(s)");

    let (rx, refresh) = app::supervisor::Supervisor::spawn(sources);
    crate::session::watch_resume(refresh.clone());
    let theme_rx = appearance::spawn();

    let config = crate::config::load();
    // Route config through a watch channel so the filesystem watcher and
    // manager can receive updates independently.
    let (config_tx, _config_rx) = tokio::sync::watch::channel(config);
    // Start the filesystem watcher. It pushes reloaded configs into config_tx
    // whenever config.json changes on disk (best-effort, never fatal).
    crate::config::watch_file(config_tx.clone());
    // Spawn the notifier after config_tx is available so it can receive the
    // notifications_enabled flag via a config receiver.
    crate::notifications::spawn(
        rx.clone(),
        config_tx.subscribe(),
        app::supervisor::LOW_THRESHOLD,
    );

    tray::manager::run(rx, theme_rx, config_tx.subscribe(), refresh).await;
}
