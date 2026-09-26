mod app;
mod appearance;
mod autostart;
#[cfg(test)]
mod bus_test;
mod cli;
mod clock;
mod config;
mod dashboard;
mod discovery;
mod doctor;
mod domain;
#[cfg(test)]
mod egui_test;
mod gui;
mod i18n;
mod icon;
mod ipc;
mod notifications;
mod palette;
mod refresh;
mod session;
mod settings;
mod sources;
mod state;
mod tray;

const USAGE: &str = "\
rigbat — system tray battery monitor for peripherals

Usage:
  rigbat [list] [--wide]   Print a one-shot battery table (default)
  rigbat --json            Print battery data as JSON
  rigbat --waybar          Stream waybar custom-module JSON lines (featured device)
  rigbat tray              Run the system tray daemon
  rigbat settings [general|appearance|devices]
                           Open the settings window, on that tab
  rigbat dashboard         Open the device overview (the tray icon's left click)
  rigbat doctor            Check the setup and print how to fix each problem

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
    Settings(settings::Tab),
    Dashboard,
    Doctor,
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

    let mut positionals = args.iter().filter(|a| !a.starts_with('-'));
    let mode = match positionals.next() {
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
        "settings" => match positionals.next().map(String::as_str) {
            None => Invocation::Settings(settings::Tab::General),
            Some(tab) => settings::Tab::from_arg(tab)
                .map_or_else(|| Invocation::Unknown(tab.to_owned()), Invocation::Settings),
        },
        "dashboard" => Invocation::Dashboard,
        "doctor" => Invocation::Doctor,
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

    if let Invocation::Settings(tab) = invocation {
        if let Err(e) = settings::run(tab) {
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

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    // ~20 events a minute need no worker per core; the variable still wins, for measuring.
    if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
        builder.worker_threads(2);
    }
    let rt = match builder.enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("failed to start tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    if invocation == Invocation::Doctor {
        std::process::exit(rt.block_on(doctor::run()));
    }
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
            | Invocation::Settings(_)
            | Invocation::Dashboard
            | Invocation::Waybar => LogProfile::Daemon,
            Invocation::List { .. }
            | Invocation::Json
            | Invocation::Doctor
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

    let ctx = sources::Context::new();
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
/// supervising retry loop (`sources::supervise`) and keeps trying to dial in
/// through `ctx.system_bus()` in the background.
fn spawn_bus_dependent_tasks(
    ctx: std::sync::Arc<sources::Context>,
    refresh: crate::refresh::RefreshSignal,
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

    let ctx = std::sync::Arc::new(sources::Context::new());
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
    let (theme_tx, theme_rx) = tokio::sync::watch::channel(appearance::ColorScheme::Dark);
    sources::supervise::supervise("color-scheme watcher", move || {
        let tx = theme_tx.clone();
        async move { appearance::follow_color_scheme(zbus::Connection::session().await?, tx).await }
    });
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
    tray::manager::run(
        rx,
        theme_rx,
        config_tx.clone(),
        crate::config::save,
        refresh,
    )
    .await;
}

/// Runs `--waybar` as a long-lived process instead of a one-shot poll, so a
/// sleeping Bluetooth peripheral keeps showing its retained reading instead
/// of `offline` on every bar tick — "If no interval or signal is defined, it
/// is assumed that the out script loops itself" (`waybar-custom(5)`).
///
/// Modeled on `run_tray`, minus the icon/appearance/notifications stack: the
/// bar has no icon to render, and the tray process already owns low-battery
/// notifications, so spawning a second notifier here would double them up.
async fn run_waybar() {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rigbat waybar module"
    );

    let config = crate::config::load();
    let (config_tx, _config_rx) = tokio::sync::watch::channel(config);
    crate::config::watch_file(config_tx.clone());

    let ctx = std::sync::Arc::new(sources::Context::new());
    // No state store here: only `rigbat tray` writes to it (see
    // `src/state/mod.rs`'s module doc and `run_tray`).
    let (rx, refresh) = app::supervisor::Supervisor::spawn(
        config_tx.clone(),
        ctx.clone(),
        None,
        app::supervisor::ConfigRole::Reader,
    );
    spawn_bus_dependent_tasks(ctx.clone(), refresh.clone());

    // A receiver of our own, so a primary_device/hidden_devices edit is
    // picked up even between two TrayState publications. config_tx must
    // outlive the loop for the same reason run_tray keeps its config_tx
    // alive: it is the sole sender, and dropping it makes every source task's
    // config_rx.changed() resolve with an error, spinning that task's select
    // loop.
    cli::waybar::run(rx, config_tx.subscribe()).await;
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
        assert_eq!(
            parse_args(&s(&["settings"])),
            Invocation::Settings(settings::Tab::General)
        );
        assert_eq!(
            parse_args(&s(&["settings", "devices"])),
            Invocation::Settings(settings::Tab::Devices)
        );
        assert_eq!(
            parse_args(&s(&["settings", "general"])),
            Invocation::Settings(settings::Tab::General)
        );
        assert_eq!(
            parse_args(&s(&["settings", "appearance"])),
            Invocation::Settings(settings::Tab::Appearance)
        );
        assert_eq!(
            parse_args(&s(&["settings", "power"])),
            Invocation::Unknown("power".to_string())
        );
        assert_eq!(parse_args(&s(&["dashboard"])), Invocation::Dashboard);
        assert_eq!(parse_args(&s(&["doctor"])), Invocation::Doctor);
    }

    #[test]
    fn doctor_is_one_shot_and_rejects_mode_flags() {
        assert_eq!(LogProfile::from(&Invocation::Doctor), LogProfile::OneShot);
        assert_eq!(
            parse_args(&s(&["doctor", "--json"])),
            Invocation::UsageError("--json cannot be combined with `doctor`".to_string())
        );
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
            default_filter_level(LogProfile::from(&Invocation::Settings(
                settings::Tab::General
            ))),
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
            "doctor",
            "--json",
            "--waybar",
            "--wide",
        ] {
            assert!(USAGE.contains(token), "USAGE missing '{token}'");
        }
    }
}

#[cfg(test)]
mod featured_tests {
    use std::time::Duration;

    use crate::config::Config;
    use crate::domain::{
        BatteryReading, BootTime, ChargeState, DeviceInfo, DeviceKind, DeviceState, Estimate,
        Presence, PrimaryStatus, Transport, TrayState,
    };

    fn device(
        name: &str,
        transport: Transport,
        presence: Presence,
        percent: Option<u8>,
        seen: BootTime,
    ) -> DeviceState {
        DeviceState {
            info: DeviceInfo {
                name: name.to_owned(),
                kind: DeviceKind::Mouse,
                transport,
                locator: None,
            },
            last_reading: percent.map(|p| BatteryReading::new(p, ChargeState::Discharging)),
            last_seen: percent.map(|_| seen),
            presence,
            estimate: Estimate::Unknown,
        }
    }

    /// Two copies of one mouse, a sleeping keyboard and a dongle that never answered.
    fn roster(now: BootTime) -> TrayState {
        let seen = now.checked_sub(Duration::from_secs(300)).unwrap();
        TrayState {
            devices: vec![
                device(
                    "MX",
                    Transport::Sysfs,
                    Presence::Unreachable,
                    Some(70),
                    seen,
                ),
                device("MX", Transport::Bluetooth, Presence::Online, Some(40), now),
                device(
                    "Keys",
                    Transport::Hidraw,
                    Presence::Unreachable,
                    Some(88),
                    seen,
                ),
                device(
                    "Dongle",
                    Transport::Hidraw,
                    Presence::Unreachable,
                    None,
                    seen,
                ),
            ],
        }
    }

    #[test]
    fn waybar_and_the_tray_icon_feature_the_same_device_and_status() {
        let now = crate::clock::now();
        let state = roster(now);
        let cases = [
            (None, "MX", Transport::Bluetooth, 40),
            (Some("MX"), "MX", Transport::Bluetooth, 40),
            (Some("Keys"), "Keys", Transport::Hidraw, 88),
            (Some("Dongle"), "MX", Transport::Bluetooth, 40),
        ];
        for (primary, name, transport, percent) in cases {
            let cfg = Config {
                primary_device: primary.map(str::to_owned),
                ..Config::default()
            };
            let tray = crate::tray::resolve::resolve_for(None, &state, &cfg, now)
                .expect("the tray features a device");
            let (waybar, waybar_status) =
                crate::cli::waybar::waybar_featured(&state.devices, &cfg, now).expect("waybar too");
            assert_eq!(waybar.info.id(), tray.state.info.id(), "pin {primary:?}");
            assert_eq!(waybar_status, tray.status, "pin {primary:?}");
            assert_eq!(
                (
                    waybar.info.name.as_str(),
                    waybar.info.transport,
                    waybar_status
                ),
                (name, transport, PrimaryStatus::Ok { percent }),
                "pin {primary:?}"
            );

            let json = crate::cli::waybar::to_waybar(&state.devices, &cfg, now);
            assert_eq!(json["percentage"], percent, "pin {primary:?}");
        }
    }
}
