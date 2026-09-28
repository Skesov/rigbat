//! Records the commit a build comes from for `rigbat -V` and the settings
//! footer: `RIGBAT_COMMIT` (short hash) and `RIGBAT_COMMITS_SINCE_TAG`. Each
//! is left unset when it cannot be found; the build never fails over them.

use std::path::{Path, PathBuf};
use std::process::Command;

include!("src/domain/version/vcs.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return;
    };

    // A packaged crate carries its own record; a repository that happens to
    // enclose the unpacked crate is not its history.
    let vcs_info = dir.join(".cargo_vcs_info.json");
    if let Ok(json) = std::fs::read_to_string(&vcs_info) {
        println!("cargo:rerun-if-changed={}", vcs_info.display());
        if let Some(sha) = short_sha_from_vcs_info(&json) {
            println!("cargo:rustc-env=RIGBAT_COMMIT={sha}");
        }
        return;
    }

    let Some(commit) = git(&dir, &["rev-parse", "--short=7", "HEAD"]) else {
        return;
    };
    for path in git_watch_paths(&dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rustc-env=RIGBAT_COMMIT={commit}");
    let describe = git(&dir, &["describe", "--tags", "--long", "--match", "v*"]);
    if let Some(count) = describe.as_deref().and_then(commits_since_tag) {
        println!("cargo:rustc-env=RIGBAT_COMMITS_SINCE_TAG={count}");
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The files a new commit touches: HEAD, the branch it points to, and
/// `packed-refs`, resolved by git so a linked worktree works too. A missing
/// file is skipped — cargo would rerun the script on every build for it.
fn git_watch_paths(dir: &Path) -> Vec<PathBuf> {
    let mut names = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    names.extend(git(dir, &["symbolic-ref", "-q", "HEAD"]));
    names
        .iter()
        .filter_map(|name| git(dir, &["rev-parse", "--git-path", name]))
        .map(|path| dir.join(path))
        .filter(|path| path.exists())
        .collect()
}
