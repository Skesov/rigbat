mod app;
mod cli;
mod domain;
mod sources;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("list");

    if mode == "tray" {
        eprintln!("tray mode: not implemented yet (M1b)");
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
