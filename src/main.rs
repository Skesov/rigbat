mod app;
mod cli;
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

    let sources = sources::sysfs::SysfsSource::enumerate()
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn sources::BatterySource>)
        .collect();

    let rows = app::poll_once(sources).await;

    if mode == "--json" {
        cli::print_json(&rows);
    } else {
        cli::print_table(&rows);
    }
}

async fn run_tray() {
    let sources: Vec<Box<dyn sources::BatterySource>> = sources::sysfs::SysfsSource::enumerate()
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn sources::BatterySource>)
        .collect();

    let n = sources.len();
    println!("rigbat tray: {n} device(s)");

    let mut rx = app::supervisor::Supervisor::spawn(sources);

    let app = tray::TrayApp::new(
        rx.clone(),
        Box::new(tray::icon::TinySkiaRenderer::default()),
    );

    let handle = match app.spawn().await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("rigbat tray: failed to start: {e}");
            return;
        }
    };

    while rx.changed().await.is_ok() {
        if handle.update(|_| {}).await.is_none() {
            break;
        }
    }
}
