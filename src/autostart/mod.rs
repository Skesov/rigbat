use std::path::PathBuf;

use anyhow::Context;

/// `~/.config/autostart/rigbat.desktop`. Returns `None` if the config base dir
/// is unavailable (e.g. no home directory in the environment).
fn desktop_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("autostart").join("rigbat.desktop"))
}

/// Returns `true` if the autostart entry exists on disk.
pub fn is_enabled() -> bool {
    desktop_path().map(|p| p.exists()).unwrap_or(false)
}

/// Builds the `.desktop` file body. `exec` is the absolute path to the binary.
///
/// The `Exec` value is double-quoted so paths containing spaces remain valid
/// per the Desktop Entry Specification (section "Exec key").
///
/// Pure function — no I/O, fully unit-testable.
fn desktop_entry(exec: &str) -> String {
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

/// Writes the autostart entry pointing at the current executable.
///
/// Known limitation: if the binary is later moved the stale `Exec` path breaks
/// autostart. Toggling Startup off then on rewrites it with the new path.
pub fn enable() -> anyhow::Result<()> {
    let path = desktop_path().context("could not determine config directory")?;
    let exec = std::env::current_exe().context("could not resolve current executable path")?;
    let exec_str = exec
        .to_str()
        .context("executable path is not valid UTF-8")?;

    let dir = path.parent().context("autostart path has no parent")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create autostart directory: {}", dir.display()))?;

    std::fs::write(&path, desktop_entry(exec_str))
        .with_context(|| format!("failed to write autostart entry: {}", path.display()))?;

    Ok(())
}

/// Removes the autostart entry. A missing file is treated as success.
pub fn disable() -> anyhow::Result<()> {
    let path = desktop_path().context("could not determine config directory")?;

    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("failed to remove autostart entry: {}", path.display()))
        }
    }
}

/// Enables or disables autostart depending on `on`.
pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    if on { enable() } else { disable() }
}

#[cfg(test)]
mod tests {
    use super::desktop_entry;

    #[test]
    fn desktop_entry_contains_required_fields() {
        let entry = desktop_entry("/usr/bin/rigbat");
        assert!(entry.contains("Type=Application"));
        assert!(entry.contains("Name=rigbat"));
        assert!(entry.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn desktop_entry_exec_is_double_quoted() {
        let entry = desktop_entry("/usr/bin/rigbat");
        assert!(entry.contains("Exec=\"/usr/bin/rigbat\" tray"));
    }

    #[test]
    fn desktop_entry_path_with_spaces_is_double_quoted() {
        let entry = desktop_entry("/home/user/my apps/rigbat");
        assert!(entry.contains("Exec=\"/home/user/my apps/rigbat\" tray"));
    }
}
