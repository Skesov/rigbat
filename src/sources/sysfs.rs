use std::path::PathBuf;

use anyhow::Context as _;

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, Transport, guess_kind};

use super::{BatteryBackend, BatterySource};

pub struct SysfsSource {
    info: DeviceInfo,
    base: PathBuf, // /sys/class/power_supply/<name>
}

pub fn parse_status(s: &str) -> ChargeState {
    match s.trim() {
        "Charging" => ChargeState::Charging,
        "Full" => ChargeState::Full,
        _ => ChargeState::Discharging,
    }
}

impl SysfsSource {
    pub fn enumerate() -> anyhow::Result<Vec<SysfsSource>> {
        let base = PathBuf::from("/sys/class/power_supply");

        let entries =
            std::fs::read_dir(&base).with_context(|| format!("reading {}", base.display()))?;

        let mut sources = Vec::new();

        for entry in entries.flatten() {
            let entry_path = entry.path();

            let kind = std::fs::read_to_string(entry_path.join("type"))
                .ok()
                .map(|s| s.trim().to_owned());
            let has_capacity = entry_path.join("capacity").exists();
            let scope = std::fs::read_to_string(entry_path.join("scope"))
                .ok()
                .map(|s| s.trim().to_owned());

            if !should_include(kind.as_deref(), has_capacity, scope.as_deref()) {
                continue;
            }

            // Name: from model_name if the file exists and is non-empty; otherwise use the directory name
            let name = {
                let model_path = entry_path.join("model_name");
                let model = std::fs::read_to_string(&model_path)
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_default();

                if model.is_empty() {
                    entry.file_name().to_string_lossy().into_owned()
                } else {
                    model
                }
            };

            let locator = entry.file_name().to_string_lossy().into_owned();
            let info = DeviceInfo {
                kind: guess_kind(&name),
                name,
                transport: Transport::Sysfs,
                locator: Some(locator),
            };

            sources.push(SysfsSource {
                info,
                base: entry_path,
            });
        }

        Ok(sources)
    }
}

/// Decides whether a `/sys/class/power_supply/<name>` entry is a gaming
/// peripheral rigbat should track, from the three sysfs facts that settle it.
/// Mains (AC adapters) and anything without a `capacity` file were always
/// excluded; `scope` now decides the harder case — a laptop's own battery,
/// which otherwise passes both older checks (C3: it has `type == "Battery"`
/// and a `capacity` file, so it used to be listed as if it were a
/// peripheral).
///
/// Peripheral power supplies (`hid-logitech-hidpp` and friends) set `scope`
/// to `Device` — verified directly against this project's own hardware: the
/// maintainer's MX Anywhere 3, exposed as `hidpp_battery_5`, reads
/// `scope: Device` on the machine this fix was written on. No ACPI-battery
/// hardware was available there to confirm what the host's own battery
/// reports when it omits `scope` entirely (a laptop with `BAT0`, not
/// present on that machine), so a missing or unreadable `scope` defaults to
/// **excluded**: a peripheral silently missing from the tray is a smaller
/// problem than the host's own battery reappearing as one (the exact defect
/// this filter exists to close).
fn should_include(kind: Option<&str>, has_capacity: bool, scope: Option<&str>) -> bool {
    if kind == Some("Mains") {
        return false;
    }
    if !has_capacity {
        return false;
    }
    scope == Some("Device")
}

#[async_trait::async_trait]
impl BatterySource for SysfsSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let capacity_str = std::fs::read_to_string(self.base.join("capacity"))
            .with_context(|| format!("reading capacity for {}", self.info.name))?;

        let percent: u8 = capacity_str
            .trim()
            .parse()
            .with_context(|| format!("parsing capacity for {}", self.info.name))?;

        let status_str = std::fs::read_to_string(self.base.join("status"))
            .with_context(|| format!("reading status for {}", self.info.name))?;

        let state = parse_status(&status_str);

        tracing::debug!(device = %self.info.name, percent, "sysfs poll");
        Ok(BatteryReading::new(percent, state))
    }
}

pub struct SysfsBackend;

#[async_trait::async_trait]
impl BatteryBackend for SysfsBackend {
    fn name(&self) -> &'static str {
        "sysfs"
    }

    async fn discover(
        &self,
        _ctx: &crate::discovery::Context,
    ) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        Ok(SysfsSource::enumerate()?
            .into_iter()
            .map(|s| Box::new(s) as Box<dyn BatterySource>)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_charging() {
        assert_eq!(parse_status("Charging"), ChargeState::Charging);
    }

    #[test]
    fn parse_status_full() {
        assert_eq!(parse_status("Full"), ChargeState::Full);
    }

    #[test]
    fn parse_status_discharging() {
        assert_eq!(parse_status("Discharging"), ChargeState::Discharging);
    }

    #[test]
    fn parse_status_not_charging() {
        assert_eq!(parse_status("Not charging"), ChargeState::Discharging);
    }

    #[test]
    fn parse_status_unknown() {
        assert_eq!(parse_status("Unknown"), ChargeState::Discharging);
    }

    #[test]
    fn parse_status_garbage() {
        assert_eq!(parse_status("foobar"), ChargeState::Discharging);
    }

    #[test]
    fn parse_status_trims_whitespace() {
        assert_eq!(parse_status("Charging\n"), ChargeState::Charging);
        assert_eq!(parse_status("Full\n"), ChargeState::Full);
    }

    // should_include (C3): a laptop's own battery must not be listed as a
    // gaming peripheral.

    #[test]
    fn should_include_accepts_device_scoped_entry() {
        // Matches the maintainer's MX Anywhere 3, verified live as
        // `hidpp_battery_5`: type Battery, has capacity, scope Device.
        assert!(should_include(Some("Battery"), true, Some("Device")));
    }

    #[test]
    fn should_include_rejects_system_scoped_entry() {
        // A laptop's BAT0 that explicitly reports the host scope.
        assert!(!should_include(Some("Battery"), true, Some("System")));
    }

    #[test]
    fn should_include_rejects_missing_scope() {
        // Chosen default for hardware this project could not verify locally
        // (no ACPI battery on the dev machine) — see `should_include`'s doc
        // comment for the reasoning.
        assert!(!should_include(Some("Battery"), true, None));
    }

    #[test]
    fn should_include_rejects_mains() {
        assert!(!should_include(Some("Mains"), true, Some("Device")));
    }

    #[test]
    fn should_include_rejects_missing_capacity() {
        assert!(!should_include(Some("Battery"), false, Some("Device")));
    }
}
