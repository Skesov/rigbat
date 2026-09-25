//! One-time conversion of a pre-T33 `shown_devices` whitelist into
//! `hidden_devices`, given the device names `discovered` by the supervisor's
//! sweeps — the only roster this conversion is allowed to depend on.
//!
//! A device absent from `discovered` (offline during that one sweep) is
//! unknowable and defaults to shown, same as an unlisted device did under
//! the old empty-means-all-shown rule; this is not lossless, only the best a
//! roster captured once allows. `hidden_devices` already on the config is
//! unioned in, not replaced, so a value this version wrote before a
//! downgrade and re-upgrade is not lost. Because the returned config always
//! clears `shown_devices`, feeding that returned config back in — as the
//! caller's own reload before its next save will — makes a repeat call a
//! no-op regardless of what `discovered` grows to.

use tokio::sync::watch;

use crate::config::Config;

/// How many discovery sweeps the conversion will wait for a roster that
/// accounts for every name in the legacy whitelist. At `DISCOVERY_INTERVAL`
/// this is a few minutes — long enough for Bluetooth peripherals to finish
/// enumerating after login, short enough that a whitelist naming a device the
/// user has since sold does not defer the conversion forever.
const MIGRATION_MAX_SWEEPS: u32 = 10;

/// The outcome of inspecting a legacy `shown_devices` whitelist against the
/// devices discovered so far.
#[derive(Debug, PartialEq, Eq)]
enum Migration {
    /// No legacy whitelist to convert.
    NotNeeded,
    /// The roster does not yet account for every name the user listed, so it
    /// cannot say which devices they meant to hide. Converting now would write
    /// a wrong answer that can never be corrected, because clearing
    /// `shown_devices` is what stops the conversion running again.
    Defer { missing: Vec<String> },
    /// Converted config, plus the names moved into `hidden_devices`.
    Ready(Box<Config>, Vec<String>),
}

/// Converts the legacy "show exactly these" whitelist into the "hide these"
/// list, given the devices discovered so far.
///
/// The conversion is only sound when the roster is complete enough: the
/// whitelist records what to *show*, so what to hide can only be derived from
/// the devices actually seen. A sweep taken before Bluetooth peripherals have
/// enumerated sees few devices, finds nothing to hide, and would clear the
/// whitelist — destroying the user's choices with a log line reading
/// `hidden=[]`, which looks like "nothing needed hiding" rather than "could
/// not tell". The caller therefore defers until every listed name has been
/// seen, or until `MIGRATION_MAX_SWEEPS` sweeps have passed.
fn migrate_shown_devices(cfg: &Config, discovered: &[String]) -> Migration {
    if cfg.shown_devices.is_empty() {
        return Migration::NotNeeded;
    }
    let missing: Vec<String> = cfg
        .shown_devices
        .iter()
        .filter(|name| !discovered.contains(name))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Migration::Defer { missing };
    }
    Migration::Ready(
        Box::new(converted(cfg, discovered)),
        moved_names(cfg, discovered),
    )
}

/// The same conversion without the completeness check, for the deadline case.
fn converted(cfg: &Config, discovered: &[String]) -> Config {
    let mut migrated = cfg.clone();
    migrated.hidden_devices = cfg.hidden_devices.clone();
    for name in moved_names(cfg, discovered) {
        migrated.hidden_devices.push(name);
    }
    migrated.shown_devices = Vec::new();
    migrated
}

/// Names discovered but absent from the whitelist, i.e. the ones to hide.
fn moved_names(cfg: &Config, discovered: &[String]) -> Vec<String> {
    let mut moved = Vec::new();
    for name in discovered {
        if !cfg.shown_devices.contains(name)
            && !cfg.hidden_devices.contains(name)
            && !moved.contains(name)
        {
            moved.push(name.clone());
        }
    }
    moved
}

/// Runs `migrate_shown_devices` once, right after the very first discovery
/// sweep — the earliest point a full device roster exists. `config::load`
/// cannot perform this conversion itself: it never sees a device list.
///
/// Calls `load_config` instead of trusting `config_tx`'s current value: the
/// settings window is a separate process (`settings::run`) that can write a
/// newer config between this process's startup and this point, and
/// `SettingsApp::persist` defends against the same staleness by re-reading
/// immediately before its own save — this mirrors that. `load_config`/
/// `save_config` are parameters (not `crate::config::load`/`save` called
/// directly) so tests can point this at a temporary file instead of the
/// real `config::config_path()`.
pub(super) fn migrate_shown_devices_once<L, S>(
    discovered: &[String],
    config_tx: &watch::Sender<Config>,
    load_config: &L,
    save_config: &S,
    sweeps: u32,
) -> bool
where
    L: Fn() -> Config,
    S: Fn(&Config) -> anyhow::Result<()>,
{
    let cfg = load_config();
    let (migrated, moved) = match migrate_shown_devices(&cfg, discovered) {
        Migration::NotNeeded => return true,
        Migration::Defer { missing } => {
            if sweeps < MIGRATION_MAX_SWEEPS {
                tracing::debug!(
                    ?missing,
                    sweeps,
                    "deferring shown_devices conversion until the roster accounts for every listed device"
                );
                return false;
            }
            // Deadline reached. Convert with what we have and say plainly which
            // devices were never seen, so a wrong outcome is diagnosable rather
            // than silent.
            tracing::warn!(
                never_seen = ?missing,
                "converting shown_devices after {MIGRATION_MAX_SWEEPS} sweeps without seeing every listed device; \
                 those devices will show until hidden again"
            );
            let moved = moved_names(&cfg, discovered);
            (converted(&cfg, discovered), moved)
        }
        Migration::Ready(migrated, moved) => (*migrated, moved),
    };

    match save_config(&migrated) {
        Ok(()) => {
            tracing::info!(
                hidden = ?moved,
                "converted legacy shown_devices whitelist to hidden_devices"
            );
            let _ = config_tx.send(migrated);
            true
        }
        Err(e) => {
            tracing::error!("failed to save migrated shown_devices whitelist: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(cfg: &Config, discovered: &[&str]) -> (Config, Vec<String>) {
        let names: Vec<String> = discovered.iter().map(|s| (*s).to_string()).collect();
        match migrate_shown_devices(cfg, &names) {
            Migration::Ready(migrated, moved) => Some((*migrated, moved)),
            _ => None,
        }
        .expect("expected Migration::Ready")
    }

    #[test]
    fn migrate_shown_devices_converts_absent_names_to_hidden() {
        let cfg = Config {
            shown_devices: vec!["a".to_string()],
            ..Config::default()
        };
        let (migrated, moved) = ready(&cfg, &["a", "b"]);
        assert_eq!(migrated.hidden_devices, vec!["b".to_string()]);
        assert!(migrated.shown_devices.is_empty());
        assert_eq!(moved, vec!["b".to_string()]);
    }

    #[test]
    fn migrate_shown_devices_empty_shown_is_noop() {
        let cfg = Config::default();
        assert_eq!(
            migrate_shown_devices(&cfg, &["a".to_string(), "b".to_string()]),
            Migration::NotNeeded
        );
    }

    /// A sweep taken before Bluetooth peripherals enumerate sees a partial
    /// roster. Converting then would clear the whitelist while finding
    /// nothing to hide, and a cleared whitelist is what stops the conversion
    /// running again — so the loss would be permanent.
    #[test]
    fn migrate_shown_devices_defers_while_a_listed_device_is_unseen() {
        let cfg = Config {
            shown_devices: vec!["mouse".to_string(), "keyboard".to_string()],
            ..Config::default()
        };
        assert_eq!(
            migrate_shown_devices(&cfg, &["mouse".to_string()]),
            Migration::Defer {
                missing: vec!["keyboard".to_string()]
            }
        );
    }

    #[test]
    fn migrate_shown_devices_ready_once_every_listed_device_is_seen() {
        let cfg = Config {
            shown_devices: vec!["mouse".to_string(), "keyboard".to_string()],
            ..Config::default()
        };
        let (migrated, moved) = ready(&cfg, &["mouse", "keyboard", "dupe"]);
        assert_eq!(migrated.hidden_devices, vec!["dupe".to_string()]);
        assert_eq!(moved, vec!["dupe".to_string()]);
    }

    /// The deadline path: `converted` is what the caller falls back to once
    /// `MIGRATION_MAX_SWEEPS` sweeps have passed without a complete roster.
    #[test]
    fn converted_hides_only_what_was_actually_seen() {
        let cfg = Config {
            shown_devices: vec!["mouse".to_string(), "sold-headset".to_string()],
            ..Config::default()
        };
        let migrated = converted(&cfg, &["mouse".to_string(), "dupe".to_string()]);
        assert_eq!(migrated.hidden_devices, vec!["dupe".to_string()]);
        assert!(migrated.shown_devices.is_empty());
    }

    /// The "runs once" guarantee: the already-converted config has an empty
    /// whitelist, so a later sweep with a larger roster cannot hide a device
    /// retroactively.
    #[test]
    fn migrate_shown_devices_second_call_with_larger_roster_adds_nothing() {
        let cfg = Config {
            shown_devices: vec!["a".to_string()],
            ..Config::default()
        };
        let (migrated, _) = ready(&cfg, &["a"]);
        assert!(migrated.hidden_devices.is_empty());

        assert_eq!(
            migrate_shown_devices(&migrated, &["a".to_string(), "b".to_string()]),
            Migration::NotNeeded
        );
    }

    #[test]
    fn migrate_shown_devices_unions_existing_hidden_devices() {
        let cfg = Config {
            shown_devices: vec!["a".to_string()],
            hidden_devices: vec!["headset".to_string()],
            ..Config::default()
        };
        let (migrated, moved) = ready(&cfg, &["a", "b"]);
        assert_eq!(
            migrated.hidden_devices,
            vec!["headset".to_string(), "b".to_string()]
        );
        assert_eq!(moved, vec!["b".to_string()]);
    }
}
