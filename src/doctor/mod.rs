//! `rigbat doctor`: one-shot checks that explain why rigbat shows nothing or
//! "offline", each problem paired with the command that fixes it.
//!
//! Probes gather facts; pure functions turn facts into [`Check`]s, so every
//! decision and its wording is testable without a bus or a device.

use std::path::{Path, PathBuf};
use std::time::Duration;

use zbus::fdo::DBusProxy;
use zbus::names::BusName;

use crate::autostart;
use crate::discovery::registry;
use crate::ipc;
use crate::state;

const SNI_WATCHER: &str = "org.kde.StatusNotifierWatcher";
const BLUEZ: &str = "org.bluez";
const UDEV_RULE: &str = "70-rigbat.rules";
const UDEV_RULE_DIRS: [&str; 2] = ["/etc/udev/rules.d", "/usr/lib/udev/rules.d"];
const BUS_TIMEOUT: Duration = Duration::from_secs(3);
/// The portal may be D-Bus activated on first use, which takes longer than a
/// plain name lookup.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub status: Status,
    pub summary: String,
    pub fix: Option<String>,
}

impl Check {
    fn ok(summary: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            summary: summary.into(),
            fix: None,
        }
    }

    fn warn(summary: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            summary: summary.into(),
            fix: Some(fix.into()),
        }
    }

    fn fail(summary: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            summary: summary.into(),
            fix: Some(fix.into()),
        }
    }
}

/// How rigbat is set up to start at login.
#[derive(Debug, Clone, Copy, Default)]
pub struct Startup {
    pub unit_installed: bool,
    pub unit_enabled: bool,
    pub autostart_entry: bool,
}

/// Session-bus names that decide whether a tray icon can appear.
#[derive(Debug, Clone, Copy)]
pub struct SessionFacts {
    pub tray_host: bool,
    pub tray_running: bool,
}

/// One supported device's hidraw node and the outcome of opening it read-write.
#[derive(Debug)]
pub struct NodeProbe {
    pub device: String,
    pub dev_path: PathBuf,
    pub open: std::io::Result<()>,
}

// ── Decisions ────────────────────────────────────────────────────────────────

pub fn session_checks(session: anyhow::Result<SessionFacts>, startup: Startup) -> Vec<Check> {
    let facts = match session {
        Ok(facts) => facts,
        Err(e) => {
            return vec![Check::fail(
                format!("session D-Bus unreachable: {e:#}"),
                "run rigbat inside a desktop session (DBUS_SESSION_BUS_ADDRESS must be set)",
            )];
        }
    };
    let host = if facts.tray_host {
        Check::ok(format!("tray host present ({SNI_WATCHER} is owned)"))
    } else {
        Check::fail(
            format!("no tray host: {SNI_WATCHER} has no owner, so tray icons cannot appear"),
            "enable your panel's system tray (StatusNotifierItem) applet; on an XEmbed-only tray run `snixembed`",
        )
    };
    let tray = if facts.tray_running {
        Check::ok(format!(
            "rigbat tray is running ({} is owned)",
            ipc::TRAY_NAME
        ))
    } else {
        Check::warn("rigbat tray is not running", tray_start_fix(startup))
    };
    vec![Check::ok("session D-Bus reachable"), host, tray]
}

fn tray_start_fix(startup: Startup) -> String {
    let unit = autostart::SYSTEMD_UNIT_NAME;
    if startup.unit_enabled {
        format!(
            "systemctl --user restart {unit}; if it stops again: journalctl --user -u {unit} -e"
        )
    } else if startup.unit_installed {
        format!("systemctl --user enable --now {unit}")
    } else if startup.autostart_entry {
        "it starts at the next login; to start it now: setsid -f rigbat tray".to_owned()
    } else {
        "make service enable (from the source tree), or run: setsid -f rigbat tray".to_owned()
    }
}

pub fn startup_check(startup: Startup) -> Check {
    match (startup.unit_enabled, startup.autostart_entry) {
        (true, true) => Check::warn(
            "both the systemd unit and the autostart entry start rigbat tray; the second one exits",
            format!(
                "keep one: systemctl --user disable --now {} or rm ~/.config/autostart/rigbat.desktop",
                autostart::SYSTEMD_UNIT_NAME
            ),
        ),
        (true, false) => Check::ok(format!(
            "starts at login via systemd ({})",
            autostart::SYSTEMD_UNIT_NAME
        )),
        (false, true) => Check::ok("starts at login via ~/.config/autostart/rigbat.desktop"),
        (false, false) => Check::ok("not started at login (optional)"),
    }
}

pub fn bluez_check(bluez: anyhow::Result<bool>) -> Check {
    match bluez {
        Ok(true) => Check::ok(format!("BlueZ present ({BLUEZ} on the system bus)")),
        Ok(false) => Check::warn(
            format!("{BLUEZ} has no owner: Bluetooth devices will not show"),
            "sudo systemctl enable --now bluetooth.service",
        ),
        Err(e) => Check::warn(
            format!("system D-Bus unreachable: {e:#}; Bluetooth devices will not show"),
            "check that the system dbus service is running: systemctl status dbus.service",
        ),
    }
}

pub fn portal_check(portal: anyhow::Result<()>) -> Check {
    match portal {
        Ok(()) => Check::ok("xdg-desktop-portal Settings answers"),
        Err(e) => Check::warn(
            format!("xdg-desktop-portal Settings unavailable: {e:#}; the theme falls back to dark"),
            "install xdg-desktop-portal and your desktop's portal backend, then log in again",
        ),
    }
}

pub fn hidraw_checks(nodes: &[NodeProbe], installed_rule: Option<&Path>) -> Vec<Check> {
    if nodes.is_empty() {
        return vec![Check::ok("no supported USB HID device connected")];
    }
    nodes
        .iter()
        .map(|node| {
            let what = format!("{} ({})", node.device, node.dev_path.display());
            match &node.open {
                Ok(()) => Check::ok(format!("{what} opens read-write")),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Check::fail(
                    format!("{what}: permission denied, the device will show offline"),
                    udev_fix(installed_rule),
                ),
                Err(e) => Check::warn(
                    format!("{what}: cannot open: {e}"),
                    "replug the device and run rigbat doctor again",
                ),
            }
        })
        .collect()
}

fn udev_fix(installed_rule: Option<&Path>) -> String {
    match installed_rule {
        Some(rule) => format!(
            "{} is installed but not applied: sudo udevadm control --reload-rules && sudo udevadm trigger, then replug the device",
            rule.display()
        ),
        None => format!(
            "{UDEV_RULE} is not installed in {}: sudo make udev-install (from the source tree), then replug the device",
            UDEV_RULE_DIRS.join(" or ")
        ),
    }
}

pub fn find_udev_rule(dirs: &[&Path]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(UDEV_RULE))
        .find(|path| path.exists())
}

/// `config` is `config::read`'s result: `Ok(false)` for no file yet.
pub fn config_check(path: Option<&Path>, config: anyhow::Result<bool>) -> Check {
    let Some(path) = path else {
        return Check::warn(
            "no config directory (HOME unset?); settings cannot be saved",
            "set HOME or XDG_CONFIG_HOME",
        );
    };
    match config {
        Ok(true) => Check::ok(format!("config parses ({})", path.display())),
        Ok(false) => Check::ok(format!(
            "no config file yet, defaults apply ({})",
            path.display()
        )),
        Err(e) => Check::fail(
            format!("config ignored, defaults apply: {e:#}"),
            format!("fix the JSON in {0}, or reset it: rm {0}", path.display()),
        ),
    }
}

pub fn state_check(path: Option<&Path>, store: anyhow::Result<()>) -> Check {
    let Some(path) = path else {
        return Check::warn(
            "no state directory (HOME unset?); device inventory and history are off",
            "set HOME or XDG_STATE_HOME",
        );
    };
    match store {
        Ok(()) => Check::ok(format!("state database opens ({})", path.display())),
        Err(e) => Check::warn(
            format!("state database unusable, inventory and history are off: {e:#}"),
            format!(
                "move the file aside, rigbat recreates it: mv {0} {0}.bak",
                path.display()
            ),
        ),
    }
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        out.push_str(&format!(
            "{:<4}  {}\n",
            check.status.as_str(),
            check.summary
        ));
        if let Some(fix) = &check.fix {
            out.push_str(&format!("      fix: {fix}\n"));
        }
    }
    out
}

pub fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.status == Status::Fail))
}

// ── Probes ───────────────────────────────────────────────────────────────────

async fn with_timeout<T>(
    limit: Duration,
    probe: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(limit, probe)
        .await
        .map_err(|_| anyhow::anyhow!("no answer within {}s", limit.as_secs()))?
}

async fn has_owner(dbus: &DBusProxy<'_>, name: &str) -> anyhow::Result<bool> {
    Ok(dbus.name_has_owner(BusName::try_from(name)?).await?)
}

async fn probe_session() -> anyhow::Result<SessionFacts> {
    let conn = zbus::Connection::session().await?;
    let dbus = DBusProxy::new(&conn).await?;
    Ok(SessionFacts {
        tray_host: has_owner(&dbus, SNI_WATCHER).await?,
        tray_running: has_owner(&dbus, ipc::TRAY_NAME).await?,
    })
}

async fn probe_bluez() -> anyhow::Result<bool> {
    let conn = zbus::Connection::system().await?;
    let dbus = DBusProxy::new(&conn).await?;
    has_owner(&dbus, BLUEZ).await
}

async fn probe_portal() -> anyhow::Result<()> {
    let settings = ashpd::desktop::settings::Settings::new().await?;
    settings.color_scheme().await?;
    Ok(())
}

fn probe_hidraw_nodes() -> Vec<NodeProbe> {
    let Ok(entries) = std::fs::read_dir("/sys/class/hidraw") else {
        return Vec::new();
    };
    let matchers = registry::hidraw_matchers();
    let mut nodes: Vec<NodeProbe> = entries
        .flatten()
        .filter_map(|entry| {
            let node = entry.file_name().to_string_lossy().into_owned();
            matchers.iter().find_map(|matches| matches(&node).ok())
        })
        .map(|dev| NodeProbe {
            open: std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&dev.dev_path)
                .map(drop),
            device: dev.info.name,
            dev_path: dev.dev_path,
        })
        .collect();
    nodes.sort_by(|a, b| a.dev_path.cmp(&b.dev_path));
    nodes
}

async fn gather() -> Vec<Check> {
    let startup = Startup {
        unit_installed: autostart::systemd_unit_installed(),
        unit_enabled: autostart::systemd_service_enabled(),
        autostart_entry: autostart::is_enabled(),
    };
    let mut checks = session_checks(with_timeout(BUS_TIMEOUT, probe_session()).await, startup);
    checks.push(startup_check(startup));
    checks.push(bluez_check(with_timeout(BUS_TIMEOUT, probe_bluez()).await));
    checks.push(portal_check(
        with_timeout(PORTAL_TIMEOUT, probe_portal()).await,
    ));

    let rule_dirs = UDEV_RULE_DIRS.map(Path::new);
    checks.extend(hidraw_checks(
        &probe_hidraw_nodes(),
        find_udev_rule(&rule_dirs).as_deref(),
    ));

    let config_path = crate::config::config_path();
    let config = config_path
        .as_deref()
        .map(|p| crate::config::read(p).map(|c| c.is_some()));
    checks.push(match config {
        Some(result) => config_check(config_path.as_deref(), result),
        None => config_check(None, Ok(false)),
    });

    let db_path = state::db_path();
    let store = db_path
        .as_deref()
        .map(|p| state::SqliteStore::open(p).map(drop));
    checks.push(match store {
        Some(result) => state_check(db_path.as_deref(), result),
        None => state_check(None, Ok(())),
    });
    checks
}

// The report is the command's output on stdout, not a diagnostic.
#[expect(clippy::print_stdout)]
pub async fn run() -> i32 {
    let checks = gather().await;
    print!("{}", render(&checks));
    exit_code(&checks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn statuses(checks: &[Check]) -> Vec<Status> {
        checks.iter().map(|c| c.status).collect()
    }

    fn node(open: std::io::Result<()>) -> NodeProbe {
        NodeProbe {
            device: "SteelSeries Aerox 5 Wireless".to_owned(),
            dev_path: PathBuf::from("/dev/hidraw5"),
            open,
        }
    }

    fn scratch_dir(test_name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rigbat-doctor-test-{test_name}-{}-{n}",
            std::process::id()
        ))
    }

    #[test]
    fn render_prints_status_summary_and_fix() {
        let checks = [
            Check::ok("session D-Bus reachable"),
            Check::fail("no tray host", "run snixembed"),
        ];
        assert_eq!(
            render(&checks),
            "ok    session D-Bus reachable\n\
             fail  no tray host\n      fix: run snixembed\n"
        );
    }

    #[test]
    fn warnings_do_not_fail_the_exit_code() {
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(exit_code(&[Check::ok("a"), Check::warn("b", "c")]), 0);
        assert_eq!(
            exit_code(&[Check::warn("b", "c"), Check::fail("d", "e")]),
            1
        );
    }

    #[test]
    fn unreachable_session_bus_is_one_failure() {
        let checks = session_checks(Err(anyhow::anyhow!("no socket")), Startup::default());
        assert_eq!(statuses(&checks), [Status::Fail]);
        assert!(checks[0].summary.contains("no socket"));
    }

    #[test]
    fn missing_tray_host_fails() {
        let facts = SessionFacts {
            tray_host: false,
            tray_running: true,
        };
        let checks = session_checks(Ok(facts), Startup::default());
        assert_eq!(statuses(&checks), [Status::Ok, Status::Fail, Status::Ok]);
        assert!(checks[1].fix.as_deref().unwrap().contains("snixembed"));
    }

    #[test]
    fn stopped_tray_fix_follows_how_it_is_meant_to_start() {
        let facts = SessionFacts {
            tray_host: true,
            tray_running: false,
        };
        let fix = |startup| {
            let checks = session_checks(Ok(facts), startup);
            assert_eq!(checks[2].status, Status::Warn);
            checks[2].fix.clone().unwrap()
        };
        let enabled = fix(Startup {
            unit_installed: true,
            unit_enabled: true,
            autostart_entry: false,
        });
        assert!(enabled.starts_with("systemctl --user restart rigbat.service"));
        let installed = fix(Startup {
            unit_installed: true,
            ..Startup::default()
        });
        assert_eq!(installed, "systemctl --user enable --now rigbat.service");
        assert!(fix(Startup::default()).contains("make service enable"));
    }

    #[test]
    fn both_startup_methods_warn() {
        let both = startup_check(Startup {
            unit_installed: true,
            unit_enabled: true,
            autostart_entry: true,
        });
        assert_eq!(both.status, Status::Warn);
        assert!(both.fix.unwrap().contains("systemctl --user disable --now"));
        for startup in [
            Startup::default(),
            Startup {
                autostart_entry: true,
                ..Startup::default()
            },
            Startup {
                unit_installed: true,
                unit_enabled: true,
                autostart_entry: false,
            },
        ] {
            assert_eq!(startup_check(startup).status, Status::Ok);
        }
    }

    #[test]
    fn bluez_problems_only_warn() {
        assert_eq!(bluez_check(Ok(true)).status, Status::Ok);
        let absent = bluez_check(Ok(false));
        assert_eq!(absent.status, Status::Warn);
        assert_eq!(
            absent.fix.as_deref(),
            Some("sudo systemctl enable --now bluetooth.service")
        );
        assert_eq!(bluez_check(Err(anyhow::anyhow!("x"))).status, Status::Warn);
    }

    #[test]
    fn missing_portal_only_warns() {
        assert_eq!(portal_check(Ok(())).status, Status::Ok);
        let missing = portal_check(Err(anyhow::anyhow!("not provided")));
        assert_eq!(missing.status, Status::Warn);
        assert!(missing.summary.contains("falls back"));
    }

    #[test]
    fn no_supported_hidraw_device_is_ok() {
        assert_eq!(statuses(&hidraw_checks(&[], None)), [Status::Ok]);
    }

    #[test]
    fn denied_node_without_rule_fails_with_the_install_command() {
        let checks = hidraw_checks(&[node(Err(ErrorKind::PermissionDenied.into()))], None);
        assert_eq!(statuses(&checks), [Status::Fail]);
        assert!(checks[0].summary.contains("/dev/hidraw5"));
        let fix = checks[0].fix.as_deref().unwrap();
        assert!(fix.contains("sudo make udev-install"), "{fix}");
        assert!(fix.contains("/etc/udev/rules.d"), "{fix}");
    }

    #[test]
    fn denied_node_with_rule_fails_with_the_reload_command() {
        let rule = Path::new("/etc/udev/rules.d/70-rigbat.rules");
        let checks = hidraw_checks(&[node(Err(ErrorKind::PermissionDenied.into()))], Some(rule));
        let fix = checks[0].fix.as_deref().unwrap();
        assert!(
            fix.starts_with("/etc/udev/rules.d/70-rigbat.rules is installed"),
            "{fix}"
        );
        assert!(fix.contains("udevadm trigger"), "{fix}");
    }

    #[test]
    fn open_node_is_ok_and_other_errors_warn() {
        let checks = hidraw_checks(
            &[node(Ok(())), node(Err(Error::other("device busy")))],
            None,
        );
        assert_eq!(statuses(&checks), [Status::Ok, Status::Warn]);
    }

    #[test]
    fn find_udev_rule_searches_every_dir() {
        let root = scratch_dir("udev");
        let etc = root.join("etc");
        let lib = root.join("lib");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        assert_eq!(find_udev_rule(&[&etc, &lib]), None);
        std::fs::write(lib.join(UDEV_RULE), "").unwrap();
        assert_eq!(find_udev_rule(&[&etc, &lib]), Some(lib.join(UDEV_RULE)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn broken_config_fails_and_missing_config_is_ok() {
        let path = Path::new("/home/u/.config/rigbat/config.json");
        assert_eq!(config_check(Some(path), Ok(true)).status, Status::Ok);
        let missing = config_check(Some(path), Ok(false));
        assert_eq!(missing.status, Status::Ok);
        assert!(missing.summary.contains("defaults"));
        let broken = config_check(Some(path), Err(anyhow::anyhow!("expected value")));
        assert_eq!(broken.status, Status::Fail);
        assert!(
            broken
                .fix
                .unwrap()
                .contains("rm /home/u/.config/rigbat/config.json")
        );
    }

    #[test]
    fn unusable_state_database_only_warns() {
        let path = Path::new("/home/u/.local/state/rigbat/rigbat.db");
        let ok = state_check(Some(path), Ok(()));
        assert_eq!(ok.status, Status::Ok);
        assert!(ok.summary.contains("rigbat.db"));
        assert_eq!(
            state_check(Some(path), Err(anyhow::anyhow!("corrupt"))).status,
            Status::Warn
        );
        assert_eq!(state_check(None, Ok(())).status, Status::Warn);
    }

    #[tokio::test]
    #[ignore = "probes the live session and system buses, portal and /dev/hidraw*"]
    async fn gather_reports_every_area() {
        let checks = gather().await;
        let report = render(&checks);
        // session (1 or 3) + startup + bluez + portal + hidraw (>= 1) + config + state
        assert!(checks.len() >= 7, "{report}");
        for needle in ["portal", "config", "state database"] {
            assert!(report.contains(needle), "missing {needle}: {report}");
        }
    }
}
