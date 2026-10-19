use std::path::{Path, PathBuf};

use anyhow::Context;

/// `~/.config/autostart/rigbat.desktop`. Returns `None` if the config base dir
/// is unavailable (e.g. no home directory in the environment).
pub fn desktop_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("autostart").join("rigbat.desktop"))
}

/// Returns `true` if the autostart entry exists on disk.
pub fn is_enabled() -> bool {
    desktop_path().is_some_and(|p| p.exists())
}

/// Escapes `"`, `` ` ``, `$` and `\` with a preceding backslash, per the
/// Desktop Entry Specification's rule for quoting arguments in the `Exec`
/// key: "escaping the double quote character, backtick character, dollar
/// sign and backslash character by preceding it with an additional
/// backslash character."
fn escape_exec_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Builds the `.desktop` file body. `exec` is the absolute path to the binary,
/// double-quoted so a path with spaces stays valid.
fn desktop_entry(exec: &str) -> String {
    let exec = escape_exec_arg(exec);
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=rigbat\n\
         Comment=Battery monitor for gaming peripherals\n\
         Exec=\"{exec}\" tray\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

/// Writes or removes the autostart entry. The entry points at the current
/// executable (`launch::executable`: never a ` (deleted)` path left by an
/// upgrade); if the binary later moves, toggling it off and on rewrites it.
pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    let path = desktop_path().context("could not determine config directory")?;
    if on {
        enable_at(&path)
    } else {
        disable_at(&path)
    }
}

/// Writes the autostart entry at `path`, creating the parent directory if needed.
fn enable_at(path: &Path) -> anyhow::Result<()> {
    let exec = crate::launch::executable();
    let exec_str = exec
        .to_str()
        .context("executable path is not valid UTF-8")?;

    let dir = path.parent().context("autostart path has no parent")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create autostart directory: {}", dir.display()))?;

    std::fs::write(path, desktop_entry(exec_str))
        .with_context(|| format!("failed to write autostart entry: {}", path.display()))?;

    Ok(())
}

/// A missing file is treated as success.
fn disable_at(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("failed to remove autostart entry: {}", path.display()))
        }
    }
}

/// Starts `rigbat tray` now, outliving this process, for a switch turned on
/// mid-session that would otherwise show no icon until the next login.
pub fn start_tray() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt as _;

    // Its own process group: a Ctrl-C on a terminal-launched settings window must not reach it.
    let mut child = std::process::Command::new(crate::launch::executable())
        .arg("tray")
        .stdin(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("failed to start rigbat tray")?;
    // A dead child stays a zombie until reaped, and the wait blocks for the tray's lifetime.
    std::thread::spawn(move || {
        if let Err(e) = child.wait() {
            tracing::warn!("tray process could not be reaped: {e}");
        }
    });
    Ok(())
}

/// systemd targets `systemctl --user enable` links a unit into; which one
/// depends on the unit's `WantedBy=`, so either counts.
const SYSTEMD_WANTS_TARGETS: [&str; 2] = ["default.target.wants", "graphical-session.target.wants"];

/// The unit name `make service` installs and `systemctl --user enable`
/// operates on.
pub const SYSTEMD_UNIT_NAME: &str = "rigbat.service";

/// `$XDG_CONFIG_HOME/systemd/user`, falling back to `~/.config/systemd/user`.
fn systemd_user_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("systemd").join("user"))
}

/// Whether `systemctl --user enable` linked `rigbat.service` into a target
/// under `unit_dir`: a filesystem check, no systemd dependency.
fn systemd_service_enabled_at(unit_dir: &Path) -> bool {
    SYSTEMD_WANTS_TARGETS
        .iter()
        .any(|target| unit_dir.join(target).join(SYSTEMD_UNIT_NAME).exists())
}

/// `false` if the config directory cannot be determined.
pub fn systemd_service_enabled() -> bool {
    systemd_user_dir()
        .map(|dir| systemd_service_enabled_at(&dir))
        .unwrap_or(false)
}

/// Returns `true` if `make service` installed the unit file, enabled or not.
pub fn systemd_unit_installed() -> bool {
    systemd_user_dir()
        .map(|dir| dir.join(SYSTEMD_UNIT_NAME).exists())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        desktop_entry, disable_at, enable_at, escape_exec_arg, systemd_service_enabled_at,
    };
    use crate::test_support::ScratchDir;

    #[test]
    fn desktop_entry_has_the_required_fields_and_a_quoted_exec() {
        let entry = desktop_entry("/usr/bin/rigbat");
        assert!(entry.contains("Type=Application"));
        assert!(entry.contains("Name=rigbat"));
        assert!(entry.contains("X-GNOME-Autostart-enabled=true"));
        assert!(entry.contains("Exec=\"/usr/bin/rigbat\" tray"));

        let entry = desktop_entry("/home/user/my apps/rigbat");
        assert!(entry.contains("Exec=\"/home/user/my apps/rigbat\" tray"));
    }

    #[test]
    fn escape_exec_arg_escapes_only_the_reserved_characters() {
        for (raw, escaped) in [
            ("a\"b", "a\\\"b"),
            ("a`b", "a\\`b"),
            ("a$b", "a\\$b"),
            ("a\\b", "a\\\\b"),
            ("/usr/bin/rigbat", "/usr/bin/rigbat"),
            ("/home/user/my apps/rigbat", "/home/user/my apps/rigbat"),
        ] {
            assert_eq!(escape_exec_arg(raw), escaped, "{raw}");
        }
    }

    #[test]
    fn enable_at_creates_missing_parent_and_writes_entry() {
        let scratch = ScratchDir::new("creates-parent");
        let path = scratch.join("autostart").join("rigbat.desktop");

        enable_at(&path).unwrap();
        let exec = std::env::current_exe().unwrap();
        let expected = desktop_entry(exec.to_str().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn enable_and_disable_toggle_the_entry() {
        let scratch = ScratchDir::new("toggles");
        let path = scratch.join("rigbat.desktop");

        assert!(!path.exists());
        enable_at(&path).unwrap();
        assert!(path.exists());
        disable_at(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn disable_at_on_absent_file_succeeds() {
        let scratch = ScratchDir::new("absent-file");
        let path = scratch.join("rigbat.desktop");

        assert!(!path.exists());
        disable_at(&path).unwrap();
    }

    #[test]
    fn enable_at_overwrites_existing_file() {
        let scratch = ScratchDir::new("overwrites");
        let path = scratch.join("rigbat.desktop");
        std::fs::write(&path, "stale contents").unwrap();

        enable_at(&path).unwrap();
        let exec = std::env::current_exe().unwrap();
        let expected = desktop_entry(exec.to_str().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn systemd_service_enabled_at_false_without_the_link() {
        let scratch = ScratchDir::new("not-enabled");
        assert!(!systemd_service_enabled_at(&scratch.join("absent")));

        let wants = scratch.join("default.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        assert!(!systemd_service_enabled_at(scratch.path()));

        std::fs::write(wants.join("other.service"), "").unwrap();
        assert!(!systemd_service_enabled_at(scratch.path()));
    }

    #[test]
    fn systemd_service_enabled_at_true_for_either_target() {
        for target in ["default.target.wants", "graphical-session.target.wants"] {
            let scratch = ScratchDir::new(target);
            let wants = scratch.join(target);
            std::fs::create_dir_all(&wants).unwrap();
            std::fs::write(wants.join("rigbat.service"), "").unwrap();
            assert!(systemd_service_enabled_at(scratch.path()), "{target}");
        }
    }
}
