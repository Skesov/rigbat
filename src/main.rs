mod app;
mod appearance;
mod cli;
mod config;
mod discovery;
mod domain;
mod notifications;
mod session;
mod settings;
mod sources;
mod tray;

use ksni::TrayMethods;

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

    let (mut rx, refresh) = app::supervisor::Supervisor::spawn(sources);
    crate::session::watch_resume(refresh.clone());
    let mut theme_rx = appearance::spawn();

    let config = crate::config::load();
    // Route config through a watch channel so the radio handler can notify the
    // main loop, which then calls handle.update to re-publish the icon.
    let (config_tx, mut config_rx) = tokio::sync::watch::channel(config);
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

    let app = tray::TrayApp::new(
        rx.clone(),
        theme_rx.clone(),
        config_tx.subscribe(),
        Box::new(tray::icon::TinySkiaRenderer::default()),
        refresh,
    );

    let handle = match app.spawn().await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("rigbat tray: failed to start: {e}");
            return;
        }
    };

    loop {
        tokio::select! {
            r = rx.changed() => {
                if r.is_err() { break; }
            }
            r = theme_rx.changed() => {
                if r.is_err() { break; }
            }
            r = config_rx.changed() => {
                if r.is_err() { break; }
            }
        }
        if handle.update(|_| {}).await.is_none() {
            break;
        }
    }
}
