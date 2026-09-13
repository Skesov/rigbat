use std::path::PathBuf;

use anyhow::Context;

/// `~/.config/autostart/rigbat.desktop`. Returns `None` if the config base dir
/// is unavailable (e.g. no home directory in the environment).
fn desktop_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("autostart").join("rigbat.desktop"))
}

/// Returns `true` if the autostart entry exists on disk.
pub fn is_enabled() -> bool {
    desktop_path().map(|p| is_enabled_at(&p)).unwrap_or(false)
}

/// Returns `true` if a file exists at `path`.
fn is_enabled_at(path: &std::path::Path) -> bool {
    path.exists()
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

/// Builds the `.desktop` file body. `exec` is the absolute path to the binary.
///
/// The `Exec` value is double-quoted so paths containing spaces remain valid
/// per the Desktop Entry Specification (section "Exec key").
///
/// Pure function — no I/O, fully unit-testable.
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

/// Writes the autostart entry pointing at the current executable.
///
/// Known limitation: if the binary is later moved the stale `Exec` path breaks
/// autostart. Toggling Startup off then on rewrites it with the new path.
pub fn enable() -> anyhow::Result<()> {
    let path = desktop_path().context("could not determine config directory")?;
    enable_at(&path)
}

/// Removes the autostart entry. A missing file is treated as success.
pub fn disable() -> anyhow::Result<()> {
    let path = desktop_path().context("could not determine config directory")?;
    disable_at(&path)
}

/// Writes the autostart entry at `path`, creating the parent directory if needed.
fn enable_at(path: &std::path::Path) -> anyhow::Result<()> {
    let exec = std::env::current_exe().context("could not resolve current executable path")?;
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

/// Removes the file at `path`. A missing file is treated as success.
fn disable_at(path: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
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
    use super::{desktop_entry, disable_at, enable_at, escape_exec_arg, is_enabled_at};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

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

    #[test]
    fn escape_exec_arg_escapes_double_quote() {
        assert_eq!(escape_exec_arg("a\"b"), "a\\\"b");
    }

    #[test]
    fn escape_exec_arg_escapes_backtick() {
        assert_eq!(escape_exec_arg("a`b"), "a\\`b");
    }

    #[test]
    fn escape_exec_arg_escapes_dollar_sign() {
        assert_eq!(escape_exec_arg("a$b"), "a\\$b");
    }

    #[test]
    fn escape_exec_arg_escapes_backslash() {
        assert_eq!(escape_exec_arg("a\\b"), "a\\\\b");
    }

    #[test]
    fn escape_exec_arg_passes_through_unaffected_chars() {
        assert_eq!(escape_exec_arg("/usr/bin/rigbat"), "/usr/bin/rigbat");
    }

    #[test]
    fn escape_exec_arg_leaves_spaces_unescaped() {
        assert_eq!(
            escape_exec_arg("/home/user/my apps/rigbat"),
            "/home/user/my apps/rigbat"
        );
    }

    /// Unique scratch directory under the OS temp dir for one test.
    /// Removed at the end of the test regardless of outcome.
    fn scratch_dir(test_name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rigbat-autostart-test-{test_name}-{}-{n}",
            std::process::id()
        ))
    }

    #[test]
    fn enable_at_creates_missing_parent_and_writes_entry() {
        let dir = scratch_dir("creates-parent");
        let path = dir.join("autostart").join("rigbat.desktop");

        enable_at(&path).unwrap();
        let exec = std::env::current_exe().unwrap();
        let expected = desktop_entry(exec.to_str().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn is_enabled_at_reflects_file_presence() {
        let dir = scratch_dir("reflects-presence");
        let path = dir.join("rigbat.desktop");

        assert!(!is_enabled_at(&path));
        enable_at(&path).unwrap();
        assert!(is_enabled_at(&path));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn disable_at_removes_entry() {
        let dir = scratch_dir("removes-entry");
        let path = dir.join("rigbat.desktop");

        enable_at(&path).unwrap();
        assert!(is_enabled_at(&path));
        disable_at(&path).unwrap();
        assert!(!is_enabled_at(&path));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn disable_at_on_absent_file_succeeds() {
        let dir = scratch_dir("absent-file");
        let path = dir.join("rigbat.desktop");

        assert!(!path.exists());
        disable_at(&path).unwrap();

        // dir itself was never created by disable_at; nothing to clean up.
    }

    #[test]
    fn enable_at_overwrites_existing_file() {
        let dir = scratch_dir("overwrites");
        let path = dir.join("rigbat.desktop");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "stale contents").unwrap();

        enable_at(&path).unwrap();
        let exec = std::env::current_exe().unwrap();
        let expected = desktop_entry(exec.to_str().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
