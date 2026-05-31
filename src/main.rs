mod app;
mod appearance;
mod cli;
mod config;
mod discovery;
mod domain;
mod sources;
mod tray;

use ksni::TrayMethods;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("list");

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

    let mut rx = app::supervisor::Supervisor::spawn(sources);
    let mut theme_rx = appearance::spawn();

    let config = crate::config::load();

    let app = tray::TrayApp::new(
        rx.clone(),
        theme_rx.clone(),
        Box::new(tray::icon::TinySkiaRenderer::default()),
        config,
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
        }
        if handle.update(|_| {}).await.is_none() {
            break;
        }
    }
}
