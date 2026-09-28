//! The version a build reports: the package version, commits past the
//! release tag and the commit hash, as `build.rs` recorded them.

use std::fmt;

#[cfg(test)]
mod vcs;

/// What `rigbat -V` and the settings footer show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    package: &'static str,
    commits_since_tag: Option<&'static str>,
    pub commit: Option<&'static str>,
}

impl Build {
    pub const fn current() -> Self {
        Self {
            package: env!("CARGO_PKG_VERSION"),
            commits_since_tag: option_env!("RIGBAT_COMMITS_SINCE_TAG"),
            commit: option_env!("RIGBAT_COMMIT"),
        }
    }

    /// "0.4.0", or "0.4.0+3" for a build three commits past the release tag.
    pub fn version(&self) -> String {
        match self.commits_since_tag {
            Some(count) => format!("{}+{count}", self.package),
            None => self.package.to_owned(),
        }
    }
}

/// "0.4.0+3 (abc1234)", or "0.4.0" without a hash.
impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.version())?;
        match self.commit {
            Some(commit) => write!(f, " ({commit})"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::vcs::{commits_since_tag, short_sha_from_vcs_info};
    use super::*;

    fn build(commits_since_tag: Option<&'static str>, commit: Option<&'static str>) -> Build {
        Build {
            package: "0.4.0",
            commits_since_tag,
            commit,
        }
    }

    #[test]
    fn a_release_build_shows_the_package_version_and_its_hash() {
        assert_eq!(build(None, Some("e3cd47f")).to_string(), "0.4.0 (e3cd47f)");
    }

    #[test]
    fn a_build_past_the_tag_counts_its_commits() {
        let past = build(Some("3"), Some("abc1234"));
        assert_eq!(past.to_string(), "0.4.0+3 (abc1234)");
        assert_eq!(past.version(), "0.4.0+3");
    }

    #[test]
    fn without_a_hash_only_the_version_is_shown() {
        assert_eq!(build(None, None).to_string(), "0.4.0");
    }

    #[test]
    fn the_hash_comes_from_cargo_vcs_info() {
        let json = "{\n  \"git\": {\n    \"sha1\": \"e3cd47f0a1b2c3d4e5f60718293a4b5c6d7e8f90\"\n  },\n  \"path_in_vcs\": \"\"\n}";
        assert_eq!(short_sha_from_vcs_info(json).as_deref(), Some("e3cd47f"));
        let dirty = r#"{"git":{"sha1":"abc1234def","dirty":true},"path_in_vcs":""}"#;
        assert_eq!(short_sha_from_vcs_info(dirty).as_deref(), Some("abc1234"));
    }

    #[test]
    fn a_vcs_info_without_a_usable_hash_gives_none() {
        for json in [
            "",
            r#"{"path_in_vcs":""}"#,
            r#"{"git":{"sha1":"abc"}}"#,
            r#"{"git":{"sha1":12345678}}"#,
        ] {
            assert_eq!(short_sha_from_vcs_info(json), None, "{json}");
        }
    }

    #[test]
    fn describe_counts_commits_past_the_tag() {
        assert_eq!(commits_since_tag("v0.4.0-3-gabc1234\n"), Some(3));
        assert_eq!(commits_since_tag("v1.0.0-rc.1-12-gabc1234"), Some(12));
        assert_eq!(commits_since_tag("v0.4.0-0-ge3cd47f"), None);
        assert_eq!(commits_since_tag("abc1234"), None);
    }
}
