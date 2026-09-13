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

const USAGE: &str = "\
rigbat — system tray battery monitor for peripherals

Usage:
  rigbat [list] [--wide]   Print a one-shot battery table (default)
  rigbat --json            Print battery data as JSON
  rigbat --waybar          Print one waybar custom-module JSON line (featured device)
  rigbat tray              Run the system tray daemon
  rigbat settings          Open the settings window

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
    // names. Check for them before falling back to the first non-flag positional.
    let json = args.iter().any(|a| a == "--json");
    let waybar = args.iter().any(|a| a == "--waybar");
    if json && waybar {
        return Invocation::UsageError("--json and --waybar are mutually exclusive".to_string());
    }
    if json {
        return Invocation::Json;
    }
    if waybar {
        return Invocation::Waybar;
    }

    if let Some(unknown_flag) = args
        .iter()
        .find(|a| a.starts_with('-') && a.as_str() != "--wide")
    {
        return Invocation::Unknown(unknown_flag.to_string());
    }

    let positional = args.iter().find(|a| !a.starts_with('-'));
    let mode = match positional {
        None => "list",
        Some(tok) => tok.as_str(),
    };

    match mode {
        "list" => Invocation::List { wide },
        "tray" => Invocation::Tray,
        "settings" => Invocation::Settings,
        other => Invocation::Unknown(other.to_string()),
    }
}

// print!/println!/eprintln! here are the CLI's own output (--help, --version,
// usage errors), not diagnostics — hence the narrow allow of the project's
// tracing-only print lints.
#[allow(clippy::print_stdout, clippy::print_stderr)]
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

/// The two logging defaults an `Invocation` maps to: the daemons (`tray`,
/// `settings`) run chatty by default, one-shot CLI output stays quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogProfile {
    Daemon,
    OneShot,
}

impl From<&Invocation> for LogProfile {
    fn from(invocation: &Invocation) -> Self {
        match invocation {
            Invocation::Tray | Invocation::Settings => LogProfile::Daemon,
            Invocation::List { .. }
            | Invocation::Json
            | Invocation::Waybar
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

    let sources = discovery::discover_all().await;

    let rows = app::poll_once(sources).await;

    if invocation == Invocation::Json {
        cli::print_json(&rows);
    } else if invocation == Invocation::Waybar {
        let cfg = crate::config::load();
        cli::print_waybar(&rows, &cfg);
    } else if opts.wide {
        cli::print_table_wide(&rows);
    } else {
        cli::print_table(&rows);
    }
}

async fn run_tray() {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting rigbat tray");

    let config = crate::config::load();
    let (config_tx, config_rx) = tokio::sync::watch::channel(config);
    // Start the filesystem watcher. It pushes reloaded configs into config_tx
    // whenever config.json changes on disk (best-effort, never fatal).
    crate::config::watch_file(config_tx.clone());

    let (rx, refresh) = app::supervisor::Supervisor::spawn(config_rx);
    crate::session::watch_resume(refresh.clone());
    crate::sources::bluez::watch_events(refresh.clone());
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
        for token in ["list", "tray", "settings", "--json", "--waybar", "--wide"] {
            assert!(USAGE.contains(token), "USAGE missing '{token}'");
        }
    }
}
