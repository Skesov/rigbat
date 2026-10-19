//! One-time conversion of a legacy `shown_devices` whitelist into
//! `hidden_devices`, against the device names the supervisor's sweeps
//! discovered — the only roster it may depend on.
//!
//! A device absent from that roster is unknowable and stays shown: not
//! lossless, only the best one roster allows. Existing `hidden_devices` are
//! unioned in, so a value written before a downgrade and re-upgrade survives.
//! The result always clears `shown_devices`, so a repeat call is a no-op
//! whatever the roster grows to.

use tokio::sync::watch;

use crate::config::Config;

/// Sweeps to wait for a roster that accounts for every listed name: a few
/// minutes at `DISCOVERY_INTERVAL`, long enough for Bluetooth peripherals to
/// enumerate after login, short enough that a sold device does not defer the
/// conversion forever.
const MIGRATION_MAX_SWEEPS: u32 = 10;

#[derive(Debug, PartialEq, Eq)]
enum Migration {
    /// No legacy whitelist to convert.
    NotNeeded,
    /// Converting now would write a wrong answer that can never be corrected,
    /// because clearing `shown_devices` is what stops the conversion running
    /// again.
    Defer {
        missing: Vec<String>,
    },
    Ready,
}

/// Whether the whitelist can be converted yet. What to hide can only be
/// derived from the devices seen: a sweep taken before Bluetooth peripherals
/// enumerate finds nothing to hide and would clear the whitelist, destroying
/// the user's choices behind a log line that reads `hidden=[]`.
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
    if missing.is_empty() {
        Migration::Ready
    } else {
        Migration::Defer { missing }
    }
}

/// The converted config, and the names it moved into `hidden_devices`: those
/// discovered but neither listed nor already hidden.
fn converted(cfg: &Config, discovered: &[String]) -> (Config, Vec<String>) {
    let mut moved: Vec<String> = Vec::new();
    for name in discovered {
        if !cfg.shown_devices.contains(name)
            && !cfg.hidden_devices.contains(name)
            && !moved.contains(name)
        {
            moved.push(name.clone());
        }
    }
    let mut migrated = cfg.clone();
    migrated.hidden_devices.extend(moved.iter().cloned());
    migrated.shown_devices = Vec::new();
    (migrated, moved)
}

/// Converts once the roster accounts for every listed name, or at the
/// deadline; `true` once nothing is left to do. Reads the config with
/// `load_config`, not `config_tx`'s value: the settings window, a separate
/// process, may have saved a newer one since this process started.
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
    match migrate_shown_devices(&cfg, discovered) {
        Migration::NotNeeded => return true,
        Migration::Defer { missing } if sweeps < MIGRATION_MAX_SWEEPS => {
            tracing::debug!(
                ?missing,
                sweeps,
                "deferring shown_devices conversion until the roster accounts for every listed device"
            );
            return false;
        }
        // Converts with what it has and names the devices never seen, so a
        // wrong outcome is diagnosable rather than silent.
        Migration::Defer { missing } => tracing::warn!(
            never_seen = ?missing,
            "converting shown_devices after {MIGRATION_MAX_SWEEPS} sweeps without seeing every listed device; \
             those devices will show until hidden again"
        ),
        Migration::Ready => {}
    }
    let (migrated, moved) = converted(&cfg, discovered);

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

/// Retried once per sweep until it converts or gives up, then never again in
/// this process, so a device connecting later cannot retrigger it. Counted
/// per sweep, not per reading: readings would burn the deadline in seconds.
pub(super) struct ShownDevicesMigration {
    sweeps: u32,
    done: bool,
}

impl ShownDevicesMigration {
    /// `enabled` is false in a process that does not own `config.json`.
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            sweeps: 0,
            done: !enabled,
        }
    }

    pub(super) fn attempt<L, S>(
        &mut self,
        discovered: &[String],
        config_tx: &watch::Sender<Config>,
        load_config: &L,
        save_config: &S,
    ) where
        L: Fn() -> Config,
        S: Fn(&Config) -> anyhow::Result<()>,
    {
        if self.done {
            return;
        }
        self.done = migrate_shown_devices_once(
            discovered,
            config_tx,
            load_config,
            save_config,
            self.sweeps,
        );
        self.sweeps = self.sweeps.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_shown(names: &[&str]) -> Config {
        Config {
            shown_devices: names_of(names),
            ..Config::default()
        }
    }

    fn names_of(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn ready(cfg: &Config, discovered: &[&str]) -> (Config, Vec<String>) {
        let discovered = names_of(discovered);
        assert_eq!(migrate_shown_devices(cfg, &discovered), Migration::Ready);
        converted(cfg, &discovered)
    }

    #[test]
    fn migrate_shown_devices_converts_absent_names_to_hidden() {
        let (migrated, moved) = ready(&cfg_shown(&["a"]), &["a", "b"]);
        assert_eq!(migrated.hidden_devices, names_of(&["b"]));
        assert!(migrated.shown_devices.is_empty());
        assert_eq!(moved, names_of(&["b"]));
    }

    #[test]
    fn migrate_shown_devices_empty_shown_is_noop() {
        assert_eq!(
            migrate_shown_devices(&Config::default(), &names_of(&["a", "b"])),
            Migration::NotNeeded
        );
    }

    /// A partial roster would clear the whitelist while finding nothing to
    /// hide, and the cleared whitelist stops the conversion for good.
    #[test]
    fn migrate_shown_devices_defers_while_a_listed_device_is_unseen() {
        assert_eq!(
            migrate_shown_devices(&cfg_shown(&["mouse", "keyboard"]), &names_of(&["mouse"])),
            Migration::Defer {
                missing: names_of(&["keyboard"])
            }
        );
    }

    #[test]
    fn migrate_shown_devices_ready_once_every_listed_device_is_seen() {
        let (migrated, moved) = ready(
            &cfg_shown(&["mouse", "keyboard"]),
            &["mouse", "keyboard", "dupe"],
        );
        assert_eq!(migrated.hidden_devices, names_of(&["dupe"]));
        assert_eq!(moved, names_of(&["dupe"]));
    }

    /// The deadline path: what the caller falls back to once
    /// `MIGRATION_MAX_SWEEPS` sweeps have passed without a complete roster.
    #[test]
    fn converted_hides_only_what_was_actually_seen() {
        let (migrated, _) = converted(
            &cfg_shown(&["mouse", "sold-headset"]),
            &names_of(&["mouse", "dupe"]),
        );
        assert_eq!(migrated.hidden_devices, names_of(&["dupe"]));
        assert!(migrated.shown_devices.is_empty());
    }

    #[test]
    fn migrate_shown_devices_second_call_with_larger_roster_adds_nothing() {
        let (migrated, _) = ready(&cfg_shown(&["a"]), &["a"]);
        assert!(migrated.hidden_devices.is_empty());

        assert_eq!(
            migrate_shown_devices(&migrated, &names_of(&["a", "b"])),
            Migration::NotNeeded
        );
    }

    #[test]
    fn migrate_shown_devices_unions_existing_hidden_devices() {
        let cfg = Config {
            hidden_devices: names_of(&["headset"]),
            ..cfg_shown(&["a"])
        };
        let (migrated, moved) = ready(&cfg, &["a", "b"]);
        assert_eq!(migrated.hidden_devices, names_of(&["headset", "b"]));
        assert_eq!(moved, names_of(&["b"]));
    }
}
