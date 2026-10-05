//! The rigbat binary to run for a window process or an autostart entry.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// What Linux appends to `/proc/self/exe` once the running binary is unlinked —
/// which `cargo install`, dpkg, rpm and pacman all do when they replace it.
const DELETED_SUFFIX: &[u8] = b" (deleted)";

/// Looked up on `PATH` when this process's own path no longer names a file.
const ON_PATH: &str = "rigbat";

/// This process's binary, or the one that replaced it at the same path, or
/// `rigbat` from `PATH` when neither exists.
pub fn executable() -> PathBuf {
    resolve(std::env::current_exe(), Path::exists)
}

fn resolve(current: std::io::Result<PathBuf>, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let Ok(path) = current else {
        return PathBuf::from(ON_PATH);
    };
    let path = match path.as_os_str().as_bytes().strip_suffix(DELETED_SUFFIX) {
        Some(stripped) => PathBuf::from(OsStr::from_bytes(stripped)),
        None => path,
    };
    if exists(&path) {
        path
    } else {
        PathBuf::from(ON_PATH)
    }
}

/// Starts `rigbat <subcommand>` as a process of its own.
pub fn spawn(subcommand: &'static str) {
    match std::process::Command::new(executable())
        .arg(subcommand)
        .spawn()
    {
        // `Child` does not reap on drop: without the wait, every closed window
        // stays a zombie for the parent's lifetime. The wait blocks, so it gets a
        // thread of its own.
        Ok(mut child) => {
            std::thread::spawn(move || {
                if let Err(e) = child.wait() {
                    tracing::warn!("{subcommand} process could not be reaped: {e}");
                }
            });
        }
        Err(e) => tracing::error!("failed to launch rigbat {subcommand}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_binary_resolves_to_the_file_now_at_its_path() {
        let exe = resolve(Ok(PathBuf::from("/usr/bin/rigbat (deleted)")), |p| {
            p == Path::new("/usr/bin/rigbat")
        });
        assert_eq!(exe, PathBuf::from("/usr/bin/rigbat"));
    }

    #[test]
    fn a_removed_binary_falls_back_to_path_lookup() {
        let exe = resolve(Ok(PathBuf::from("/opt/rigbat/rigbat (deleted)")), |_| false);
        assert_eq!(exe, PathBuf::from("rigbat"));
        let unknown = resolve(Err(std::io::ErrorKind::NotFound.into()), |_| true);
        assert_eq!(unknown, PathBuf::from("rigbat"));
    }

    #[test]
    fn a_live_binary_keeps_its_own_path() {
        let exe = resolve(Ok(PathBuf::from("/home/u/.cargo/bin/rigbat")), |_| true);
        assert_eq!(exe, PathBuf::from("/home/u/.cargo/bin/rigbat"));
    }

    #[test]
    fn the_running_test_binary_resolves_to_itself() {
        let own = std::env::current_exe().unwrap();
        assert_eq!(executable(), own);
    }
}
