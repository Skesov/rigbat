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

struct CliOpts {
    wide: bool,
}

fn parse_args(args: &[String]) -> (CliOpts, &str) {
    let wide = args.iter().any(|a| a == "--wide");
    // --json is a legacy flag-style mode selector, not a subcommand name.
    // Check for it before falling back to the first non-flag positional.
    let json = args.iter().any(|a| a == "--json");
    let mode = if json {
        "--json"
    } else {
        args.iter()
            .find(|a| !a.starts_with('-'))
            .map(String::as_str)
            .unwrap_or("list")
    };
    (CliOpts { wide }, mode)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (opts, mode) = parse_args(&args);

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
    rt.block_on(async_main(mode, opts));
}

async fn async_main(mode: &str, opts: CliOpts) {
    if mode == "tray" {
        run_tray().await;
        return;
    }

    let sources = discovery::discover_all().await;

    let rows = app::poll_once(sources).await;

    if mode == "--json" {
        cli::print_json(&rows);
    } else if opts.wide {
        cli::print_table_wide(&rows);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wide_flag_after_subcommand() {
        let args = s(&["list", "--wide"]);
        let (opts, mode) = parse_args(&args);
        assert!(opts.wide);
        assert_eq!(mode, "list");
    }

    #[test]
    fn wide_flag_before_subcommand() {
        let args = s(&["--wide", "list"]);
        let (opts, mode) = parse_args(&args);
        assert!(opts.wide);
        assert_eq!(mode, "list");
    }

    #[test]
    fn wide_flag_alone() {
        let args = s(&["--wide"]);
        let (opts, mode) = parse_args(&args);
        assert!(opts.wide);
        assert_eq!(mode, "list");
    }

    #[test]
    fn no_wide_flag_default() {
        let args = s(&["list"]);
        let (opts, mode) = parse_args(&args);
        assert!(!opts.wide);
        assert_eq!(mode, "list");
    }

    #[test]
    fn mode_tray_no_wide() {
        let args = s(&["tray"]);
        let (opts, mode) = parse_args(&args);
        assert!(!opts.wide);
        assert_eq!(mode, "tray");
    }
}
