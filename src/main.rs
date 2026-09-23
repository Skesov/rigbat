mod app;
mod appearance;
mod autostart;
mod cli;
mod config;
mod dashboard;
mod discovery;
mod domain;
#[cfg(test)]
mod egui_test;
mod gui;
mod i18n;
mod icon;
mod ipc;
mod notifications;
mod session;
mod settings;
mod sources;
mod state;
mod tray;

use crate::domain::Presence;
use crate::domain::select_featured;

const USAGE: &str = "\
rigbat — system tray battery monitor for peripherals

Usage:
  rigbat [list] [--wide]   Print a one-shot battery table (default)
  rigbat --json            Print battery data as JSON
  rigbat --waybar          Stream waybar custom-module JSON lines (featured device)
  rigbat tray              Run the system tray daemon
  rigbat settings          Open the settings window
  rigbat dashboard         Open the device overview (the tray icon's left click)

Options:
  --wide        Add transport and locator columns to the table
  -h, --help    Show this help and exit
  -V, --version Show the version and exit
";

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    List {
        wide: bool,
    },
    Json,
    Waybar,
    Tray,
    Settings,
    Dashboard,
    Help,
    Version,
    /// Unrecognised argument; carries the offending token for the error message.
    Unknown(String),
    /// Recognised arguments that conflict with each other; carries the message.
    UsageError(String),
}

fn parse_args(args: &[String]) -> Invocation {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Invocation::Help;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        return Invocation::Version;
    }

    let wide = args.iter().any(|a| a == "--wide");
    // --json and --waybar are legacy flag-style mode selectors, not subcommand
    // names.
    let json = args.iter().any(|a| a == "--json");
    let waybar = args.iter().any(|a| a == "--waybar");
    if json && waybar {
        return Invocation::UsageError("--json and --waybar are mutually exclusive".to_string());
    }

    // Unknown flags are rejected before any mode is chosen. Returning on
    // --json first would swallow them: `rigbat --oops --json` printed JSON.
    if let Some(unknown_flag) = args
        .iter()
        .find(|a| a.starts_with('-') && !MODE_FLAGS.contains(&a.as_str()))
    {
        return Invocation::Unknown(unknown_flag.to_string());
    }

    let positional = args.iter().find(|a| !a.starts_with('-'));
    let mode = match positional {
        None => "list",
        Some(tok) => tok.as_str(),
    };

    // A mode selector only makes sense for the one-shot reading. Pairing it
    // with a subcommand used to win silently, so `rigbat tray --json` printed
    // one JSON line and exited 0 instead of starting the daemon.
    if (json || waybar) && mode != "list" {
        let flag = if json { "--json" } else { "--waybar" };
        return Invocation::UsageError(format!("{flag} cannot be combined with `{mode}`"));
    }
    if json {
        return Invocation::Json;
    }
    if waybar {
        return Invocation::Waybar;
    }

    match mode {
        "list" => Invocation::List { wide },
        "tray" => Invocation::Tray,
        "settings" => Invocation::Settings,
        "dashboard" => Invocation::Dashboard,
        other => Invocation::Unknown(other.to_string()),
    }
}

/// Flags that select a mode or shape its output, as opposed to an unknown
/// flag, which is a usage error wherever it appears.
const MODE_FLAGS: [&str; 3] = ["--wide", "--json", "--waybar"];

// print!/println!/eprintln! here are the CLI's own output (--help, --version,
// usage errors), not diagnostics — hence the narrow exemption from the
// project's tracing-only print lints.
#[expect(clippy::print_stdout, clippy::print_stderr)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let invocation = parse_args(&args);

    match invocation {
        Invocation::Help => {
            print!("{USAGE}");
            std::process::exit(0);
        }
        Invocation::Version => {
            println!("rigbat {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        Invocation::Unknown(tok) => {
            eprintln!("rigbat: unrecognized argument '{tok}'");
            eprintln!("Try 'rigbat --help' for more information.");
            std::process::exit(2);
        }
        Invocation::UsageError(msg) => {
            eprintln!("rigbat: {msg}");
            eprintln!("Try 'rigbat --help' for more information.");
            std::process::exit(2);
        }
        _ => {}
    }

    init_logging(&invocation);

    if invocation == Invocation::Settings {
        if let Err(e) = settings::run() {
            tracing::error!("settings window failed to start: {e}");
            std::process::exit(1);
        }
        return;
    }
    if invocation == Invocation::Dashboard {
        if let Err(e) = dashboard::run() {
            tracing::error!("dashboard window failed to start: {e:#}");
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
            tracing::error!("failed to start tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    rt.block_on(async_main(invocation));
}

/// Level policy for every `tracing` event in this codebase:
/// - `error`: the user loses a feature and must act (config save failed, tray icon
///   registration failed, cannot find own executable to launch settings).
/// - `warn`: degraded but self-healing or optional (config parse error, file watcher
///   or notifications unavailable, a source failed to open).
/// - `info`: lifecycle a user would want in the journal (daemon start, tray icon
///   count changed, device appeared/vanished, resume-from-suspend re-poll).
/// - `debug`: per-poll detail (each reading, each discovery pass, theme change,
///   config reload applied).
/// - `trace`: raw protocol detail (hidraw bytes, D-Bus property maps) — unused today.
fn init_logging(invocation: &Invocation) {
    use std::io::IsTerminal as _;

    let profile = LogProfile::from(invocation);
    let rigbat_log = std::env::var("RIGBAT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    let directive = resolve_filter_directive(profile, rigbat_log.as_deref(), rust_log.as_deref());

    let filter = tracing_subscriber::EnvFilter::try_new(&directive)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter_level(profile)));

    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(filter)
        .finish();

    // Never abort the program over a logging setup failure.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// The logging defaults an `Invocation` maps to: the daemons (`tray`,
/// `settings`, and now the long-lived `--waybar` module) run chatty, one-shot
/// CLI output stays quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogProfile {
    Daemon,
    OneShot,
}

impl From<&Invocation> for LogProfile {
    fn from(invocation: &Invocation) -> Self {
        match invocation {
            Invocation::Tray
            | Invocation::Settings
            | Invocation::Dashboard
            | Invocation::Waybar => LogProfile::Daemon,
            Invocation::List { .. }
            | Invocation::Json
            | Invocation::Help
            | Invocation::Version
            | Invocation::Unknown(_)
            | Invocation::UsageError(_) => LogProfile::OneShot,
        }
    }
}

/// The default `EnvFilter` directive for `profile` when neither `RIGBAT_LOG`
/// nor `RUST_LOG` is set: quiet for one-shot CLI output, chatty for the daemons.
fn default_filter_level(profile: LogProfile) -> &'static str {
    match profile {
        LogProfile::Daemon => "info",
        LogProfile::OneShot => "warn",
    }
}

/// Picks the filter directive: `RIGBAT_LOG` wins, then `RUST_LOG`, then the
/// profile's default. Pure function so the precedence is testable without
/// touching the process environment.
fn resolve_filter_directive(
    profile: LogProfile,
    rigbat_log: Option<&str>,
    rust_log: Option<&str>,
) -> String {
    rigbat_log
        .or(rust_log)
        .map(str::to_owned)
        .unwrap_or_else(|| default_filter_level(profile).to_owned())
}

struct CliOpts {
    wide: bool,
}

async fn async_main(invocation: Invocation) {
    let opts = match &invocation {
        Invocation::List { wide } => CliOpts { wide: *wide },
        _ => CliOpts { wide: false },
    };

    if invocation == Invocation::Tray {
        run_tray().await;
        return;
    }
    if invocation == Invocation::Waybar {
        run_waybar().await;
        return;
    }

    let ctx = discovery::Context::new();
    let sweeps = discovery::discover_all(&ctx).await;
    let sources = discovery::flatten(sweeps);

    let rows = app::poll_once(sources).await;

    if invocation == Invocation::Json {
        cli::print_json(&rows);
    } else if opts.wide {
        cli::print_table_wide(&rows);
    } else {
        cli::print_table(&rows);
    }
}

/// Spawns the two long-lived watchers that need the system bus (resume
/// detection, BlueZ event watching), handing each the shared `ctx` rather
/// than a resolved connection. Both are optimisations, never dependencies —
/// the periodic discovery sweep is the safety net — so a missing or later
/// lost bus never delays or fails startup: each watcher runs its own
/// supervising retry loop (`discovery::backoff`) and keeps trying to dial in
/// through `ctx.system_bus()` in the background.
fn spawn_bus_dependent_tasks(
    ctx: std::sync::Arc<discovery::Context>,
    refresh: crate::app::refresh::RefreshSignal,
) {
    crate::session::watch_resume(refresh.clone(), ctx.clone());
    crate::sources::bluez::watch_events(refresh, ctx);
}

/// Prints the human-facing explanation for a second `rigbat tray` standing
/// down. Not a `tracing` log line: this is addressed to whoever is watching
/// the terminal, not the journal.
#[expect(clippy::print_stderr)]
fn announce_already_running() {
    eprintln!(
        "another rigbat tray is already running in this session; leaving it in charge \
         (enable either the systemd service or the Startup toggle, not both)"
    );
}

async fn run_tray() {
    // Keep the session-bus connection alive for the whole process: dropping
    // it releases the single-instance name and reopens the collision.
    let bus = match ipc::single_instance::acquire().await {
        ipc::single_instance::SingleInstance::AlreadyRunning => {
            tracing::info!(
                "another rigbat tray already owns the session-bus single-instance name; exiting"
            );
            announce_already_running();
            return;
        }
        ipc::single_instance::SingleInstance::Acquired(conn) => Some(conn),
        ipc::single_instance::SingleInstance::Unavailable => None,
    };

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting rigbat tray");

    let config = crate::config::load();
    let (config_tx, _config_rx) = tokio::sync::watch::channel(config);
    // Start the filesystem watcher. It pushes reloaded configs into config_tx
    // whenever config.json changes on disk (best-effort, never fatal).
    crate::config::watch_file(config_tx.clone());

    // Optional: device inventory and reading history. `state::open` logs its
    // own warning and returns None if the store cannot be opened — battery
    // monitoring must not depend on it. Only the tray writes to it (see
    // `src/state/mod.rs`); `--waybar` runs its own Supervisor without one.
    let store = crate::state::open();
    if let Some(store) = &store {
        crate::state::spawn_retention(store.clone());
    }

    let ctx = std::sync::Arc::new(discovery::Context::new());
    let (rx, refresh) = app::supervisor::Supervisor::spawn(
        config_tx.clone(),
        ctx.clone(),
        store.clone(),
        app::supervisor::ConfigRole::Owner,
    );
    spawn_bus_dependent_tasks(ctx.clone(), refresh.clone());
    if let Some(conn) = &bus {
        tokio::spawn(tray::state_service::serve(
            conn.clone(),
            rx.clone(),
            config_tx.subscribe(),
            refresh.clone(),
        ));
    }
    let theme_rx = appearance::spawn();
    // Spawn the notifier after config_tx is available so it can receive the
    // notifications_enabled flag and per-device thresholds via a config receiver.
    crate::notifications::spawn(rx.clone(), config_tx.subscribe());

    // Wait for the first discovery pass so the initial roster can be logged.
    let mut probe = rx.clone();
    if probe.changed().await.is_ok() {
        tracing::info!(
            devices = probe.borrow().devices.len(),
            "initial discovery complete"
        );
    }

    // config_tx must outlive every subscriber: it is the sole sender, and a
    // dropped sender makes every source task's config_rx.changed() resolve
    // with an error instead of waiting, spinning that task's select loop.
    tray::manager::run(rx, theme_rx, config_tx.subscribe(), refresh).await;
}

/// Runs `--waybar` as a long-lived process instead of a one-shot poll, so a
/// sleeping Bluetooth peripheral keeps showing its retained reading instead
/// of `offline` on every bar tick — "If no interval or signal is defined, it
/// is assumed that the out script loops itself" (`waybar-custom(5)`).
///
/// Modeled on `run_tray`, minus the icon/appearance/notifications stack: the
/// bar has no icon to render, and the tray process already owns low-battery
/// notifications, so spawning a second notifier here would double them up.
/// How long `run_waybar` waits for a first battery reading before printing
/// whatever state it has.
const FIRST_SWEEP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

// See cli::print_json: the waybar module line is program output on stdout.
#[expect(clippy::print_stdout)]
fn print_line(line: &str) {
    println!("{line}");
}

async fn run_waybar() {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rigbat waybar module"
    );

    let config = crate::config::load();
    let (config_tx, _config_rx) = tokio::sync::watch::channel(config);
    crate::config::watch_file(config_tx.clone());

    let ctx = std::sync::Arc::new(discovery::Context::new());
    // No state store here: only `rigbat tray` writes to it (see
    // `src/state/mod.rs`'s module doc and `run_tray`).
    let (mut rx, refresh) = app::supervisor::Supervisor::spawn(
        config_tx.clone(),
        ctx.clone(),
        None,
        app::supervisor::ConfigRole::Reader,
    );
    spawn_bus_dependent_tasks(ctx.clone(), refresh.clone());

    // A receiver of our own, so a primary_device/hidden_devices edit is
    // picked up even between two TrayState publications.
    let mut cfg_rx = config_tx.subscribe();

    // `Supervisor::spawn` publishes an empty `TrayState` before any backend has
    // run, so printing straight away puts a false "no devices" frame on the bar
    // until the first sweep lands. A device that has been discovered but not yet
    // polled is indistinguishable in `TrayState` from one that is genuinely
    // unreachable — both carry no reading — so waiting for the first publication
    // is not enough: that one announces the roster, not its charge.
    //
    // Wait for a reading, bounded: an all-offline roster never produces one, and
    // a module with no label at all is worse than a late one. An empty roster is
    // already final and does not wait.
    let _ = tokio::time::timeout(FIRST_SWEEP_WAIT, async {
        // The value already in the channel is the placeholder the supervisor
        // published before discovery ran; an empty roster there means "not yet",
        // not "none". Take the first real publication before judging.
        if rx.changed().await.is_err() {
            return;
        }
        loop {
            {
                let state = rx.borrow();
                let cfg = cfg_rx.borrow();
                // Judge the device the frame is actually built from, resolved the
                // same way the renderer resolves it. A roster can carry the same
                // mouse twice (sysfs and Bluetooth) and a hidden copy answering
                // first says nothing about the visible one; conversely, requiring
                // every shown device to answer never settles when one of them is
                // permanently asleep, which is the normal state of a wireless
                // mouse on its charger.
                let shown: Vec<(&str, bool)> = state
                    .devices
                    .iter()
                    .filter(|d| cfg.is_shown(&d.info.name))
                    .map(|d| (d.info.name.as_str(), d.presence == Presence::Online))
                    .collect();
                let settled = match select_featured(&shown, cfg.primary_device.as_deref()) {
                    None => true,
                    Some(name) => state
                        .devices
                        .iter()
                        .any(|d| d.info.name == name && d.last_reading.is_some()),
                };
                if settled {
                    return;
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await;

    let mut last_line: Option<String> = None;

    loop {
        {
            let state = rx.borrow();
            let cfg = cfg_rx.borrow();
            let line = cli::render_waybar_line(&state.devices, &cfg, std::time::Instant::now());
            if last_line.as_deref() != Some(line.as_str()) {
                print_line(&line);
                last_line = Some(line);
            }
        }

        // config_tx must outlive this loop for the same reason run_tray keeps
        // its config_tx alive: it is the sole sender, and dropping it makes
        // every source task's config_rx.changed() resolve with an error,
        // spinning that task's select loop.
        tokio::select! {
            r = rx.changed() => if r.is_err() { break; },
            r = cfg_rx.changed() => if r.is_err() { break; },
        }
    }

    // Reached only when a sender is gone, which means the supervisor is no
    // longer publishing. Waybar restarts the module after `restart-interval`;
    // say why the line stopped so the restart is not a silent mystery.
    tracing::error!("state channel closed, waybar module exiting");
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
        assert_eq!(parse_args(&args), Invocation::List { wide: true });
    }

    #[test]
    fn wide_flag_before_subcommand() {
        let args = s(&["--wide", "list"]);
        assert_eq!(parse_args(&args), Invocation::List { wide: true });
    }

    #[test]
    fn wide_flag_alone() {
        let args = s(&["--wide"]);
        assert_eq!(parse_args(&args), Invocation::List { wide: true });
    }

    #[test]
    fn no_wide_flag_default() {
        let args = s(&["list"]);
        assert_eq!(parse_args(&args), Invocation::List { wide: false });
    }

    #[test]
    fn mode_tray_no_wide() {
        let args = s(&["tray"]);
        assert_eq!(parse_args(&args), Invocation::Tray);
    }

    #[test]
    fn no_args_defaults_to_list() {
        let args = s(&[]);
        assert_eq!(parse_args(&args), Invocation::List { wide: false });
    }

    #[test]
    fn help_flag_wins_over_mode() {
        assert_eq!(parse_args(&s(&["--help"])), Invocation::Help);
        assert_eq!(parse_args(&s(&["-h"])), Invocation::Help);
        assert_eq!(parse_args(&s(&["tray", "--help"])), Invocation::Help);
    }

    #[test]
    fn version_flag() {
        assert_eq!(parse_args(&s(&["--version"])), Invocation::Version);
        assert_eq!(parse_args(&s(&["-V"])), Invocation::Version);
    }

    #[test]
    fn json_mode() {
        assert_eq!(parse_args(&s(&["--json"])), Invocation::Json);
        assert_eq!(parse_args(&s(&["--json", "--wide"])), Invocation::Json);
    }

    /// `--json` is a mode, so pairing it with a subcommand is a contradiction,
    /// not a preference. It used to win silently: `rigbat tray --json` printed
    /// one reading and exited 0 instead of starting the daemon.
    #[test]
    fn mode_flag_with_a_subcommand_is_a_usage_error() {
        for (argv, expected) in [
            (
                vec!["tray", "--json"],
                "--json cannot be combined with `tray`",
            ),
            (
                vec!["settings", "--json"],
                "--json cannot be combined with `settings`",
            ),
            (
                vec!["tray", "--waybar"],
                "--waybar cannot be combined with `tray`",
            ),
        ] {
            assert_eq!(
                parse_args(&s(&argv)),
                Invocation::UsageError(expected.to_string()),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn mode_flag_with_the_list_subcommand_is_accepted() {
        assert_eq!(parse_args(&s(&["list", "--json"])), Invocation::Json);
        assert_eq!(parse_args(&s(&["list", "--waybar"])), Invocation::Waybar);
    }

    /// An unknown flag was swallowed whenever a mode flag sat beside it,
    /// because the mode returned before anything validated the rest.
    #[test]
    fn unknown_flag_is_reported_even_beside_a_mode_flag() {
        assert_eq!(
            parse_args(&s(&["--oops", "--json"])),
            Invocation::Unknown("--oops".to_string())
        );
    }

    #[test]
    fn waybar_mode() {
        assert_eq!(parse_args(&s(&["--waybar"])), Invocation::Waybar);
    }

    #[test]
    fn json_and_waybar_together_is_usage_error() {
        assert_eq!(
            parse_args(&s(&["--json", "--waybar"])),
            Invocation::UsageError("--json and --waybar are mutually exclusive".to_string())
        );
    }

    #[test]
    fn mode_settings() {
        assert_eq!(parse_args(&s(&["settings"])), Invocation::Settings);
        assert_eq!(parse_args(&s(&["dashboard"])), Invocation::Dashboard);
    }

    #[test]
    fn default_filter_level_tray_is_info() {
        assert_eq!(
            default_filter_level(LogProfile::from(&Invocation::Tray)),
            "info"
        );
    }

    #[test]
    fn default_filter_level_settings_is_info() {
        assert_eq!(
            default_filter_level(LogProfile::from(&Invocation::Settings)),
            "info"
        );
    }

    #[test]
    fn default_filter_level_list_is_warn() {
        assert_eq!(
            default_filter_level(LogProfile::from(&Invocation::List { wide: false })),
            "warn"
        );
    }

    #[test]
    fn default_filter_level_json_is_warn() {
        assert_eq!(
            default_filter_level(LogProfile::from(&Invocation::Json)),
            "warn"
        );
    }

    #[test]
    fn default_filter_level_waybar_is_info() {
        assert_eq!(
            default_filter_level(LogProfile::from(&Invocation::Waybar)),
            "info"
        );
    }

    #[test]
    fn resolve_filter_directive_prefers_rigbat_log() {
        assert_eq!(
            resolve_filter_directive(LogProfile::OneShot, Some("debug"), Some("trace")),
            "debug"
        );
    }

    #[test]
    fn resolve_filter_directive_falls_back_to_rust_log() {
        assert_eq!(
            resolve_filter_directive(LogProfile::OneShot, None, Some("trace")),
            "trace"
        );
    }

    #[test]
    fn resolve_filter_directive_falls_back_to_mode_default() {
        assert_eq!(
            resolve_filter_directive(LogProfile::Daemon, None, None),
            "info"
        );
    }

    #[test]
    fn unknown_positional() {
        assert_eq!(
            parse_args(&s(&["lst"])),
            Invocation::Unknown("lst".to_string())
        );
    }

    #[test]
    fn unknown_flag() {
        assert_eq!(
            parse_args(&s(&["--wdie"])),
            Invocation::Unknown("--wdie".to_string())
        );
    }

    #[test]
    fn usage_mentions_all_modes_and_flags() {
        for token in [
            "list",
            "tray",
            "settings",
            "dashboard",
            "--json",
            "--waybar",
            "--wide",
        ] {
            assert!(USAGE.contains(token), "USAGE missing '{token}'");
        }
    }
}
