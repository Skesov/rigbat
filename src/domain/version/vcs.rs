// Included by build.rs; compiled into the crate only for its tests.

/// The 7-character short hash from `.cargo_vcs_info.json`, which `cargo
/// package` writes as `{"git": {"sha1": "…"}, …}`.
pub fn short_sha_from_vcs_info(json: &str) -> Option<String> {
    let value = json
        .split_once("\"sha1\"")?
        .1
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let sha: String = value
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .take(7)
        .collect();
    (sha.len() == 7).then_some(sha)
}

/// Commits past the tag in `git describe --long` output
/// (`v0.4.0-3-gabc1234` → 3); `None` on the tagged commit itself.
pub fn commits_since_tag(describe: &str) -> Option<u32> {
    let mut parts = describe.trim().rsplitn(3, '-');
    parts.next()?.strip_prefix('g')?;
    let count = parts.next()?.parse().ok()?;
    parts.next()?;
    (count > 0).then_some(count)
}
