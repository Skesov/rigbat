use std::path::PathBuf;

use anyhow::Context as _;

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, Transport, guess_kind};

use super::{BatteryBackend, BatterySource, hidraw};

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

            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let locator = stable_locator(&entry_path, &dir_name);
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

/// A locator for a power_supply directory that survives a reconnect.
///
/// The directory name is not identity. `hid-logitech-hidpp` builds it from a
/// module-global counter — `n = atomic_inc_return(&battery_no) - 1;` then
/// `sprintf(battery->name, "hidpp_battery_%ld", n)` — so the same mouse is
/// `hidpp_battery_6` now and something else after the next reconnect or
/// reboot. `DeviceId` treats the locator as identity, so using the directory
/// name made one device look like a new one each time: retained reading
/// dropped, inventory row duplicated with a fresh "first seen".
///
/// The kernel registers the supply with the HID device as its parent
/// (`devm_power_supply_register(&hidpp->hid_dev->dev, …)`), so
/// `<supply>/device/uevent` is that device's uevent, and the same
/// serial-then-USB-path preference the hidraw backends use applies here.
/// A supply with no readable HID uevent — a non-HID power source — keeps the
/// directory name, exactly as before.
fn stable_locator(entry_path: &std::path::Path, dir_name: &str) -> String {
    match std::fs::read_to_string(entry_path.join("device/uevent")) {
        Ok(uevent) => hidraw::stable_locator(&uevent, dir_name),
        Err(_) => dir_name.to_owned(),
    }
}

#[async_trait::async_trait]
impl BatterySource for SysfsSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let base = self.base.clone();
        let name = self.info.name.clone();

        // Blocking I/O, like the hidraw backends. A power_supply attribute is
        // served by its driver, and whether answering touches the hardware is
        // the driver's decision, not rigbat's: hid-logitech-hidpp answers from
        // a cache it refreshes on notifications, while an ACPI-backed supply
        // evaluates a method per read. The read is usually sub-millisecond, but
        // "usually" is not a property the runtime can rely on, and once per
        // poll interval the extra task costs nothing measurable.
        let reading = tokio::task::spawn_blocking(move || read_reading(&base, &name))
            .await
            .context("spawn_blocking")??;

        tracing::debug!(device = %self.info.name, percent = reading.percent, "sysfs poll");
        Ok(reading)
    }
}

/// Reads `capacity` and `status` from one power_supply directory.
fn read_reading(base: &std::path::Path, name: &str) -> anyhow::Result<BatteryReading> {
    let capacity_str = std::fs::read_to_string(base.join("capacity"))
        .with_context(|| format!("reading capacity for {name}"))?;

    let percent: u8 = capacity_str
        .trim()
        .parse()
        .with_context(|| format!("parsing capacity for {name}"))?;

    let status_str = std::fs::read_to_string(base.join("status"))
        .with_context(|| format!("reading status for {name}"))?;

    Ok(BatteryReading::new(percent, parse_status(&status_str)))
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

    /// A stand-in `/sys/class/power_supply/<name>` directory under the OS temp
    /// dir, so the test never depends on what this machine has plugged in.
    fn scratch_supply(test_name: &str, capacity: &str, status: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rigbat-sysfs-test-{test_name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("capacity"), capacity).unwrap();
        std::fs::write(dir.join("status"), status).unwrap();
        dir
    }

    /// A HID-backed supply keys on the device's own serial, so the locator is
    /// the same string before and after the kernel renumbers the directory.
    #[test]
    fn stable_locator_prefers_the_hid_uevent() {
        let dir = scratch_supply("locator-hid", "50\n", "Discharging\n");
        std::fs::create_dir_all(dir.join("device")).unwrap();
        std::fs::write(
            dir.join("device/uevent"),
            "HID_ID=0003:0000046D:0000B02Y\nHID_PHYS=usb-0000:10:00.0-3/input2:1\nHID_UNIQ=e8:1a:2c:3d:4e:5f\n",
        )
        .unwrap();

        assert_eq!(stable_locator(&dir, "hidpp_battery_6"), "e8:1a:2c:3d:4e:5f");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A supply with no HID parent — anything that is not a HID device — keeps
    /// the directory name it always had.
    #[test]
    fn stable_locator_falls_back_to_the_directory_name() {
        let dir = scratch_supply("locator-plain", "50\n", "Discharging\n");

        assert_eq!(stable_locator(&dir, "some_battery"), "some_battery");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_reading_parses_capacity_and_status() {
        let dir = scratch_supply("ok", "77\n", "Charging\n");

        let reading = read_reading(&dir, "test").unwrap();
        assert_eq!(reading.percent, 77);
        assert_eq!(reading.state, ChargeState::Charging);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_reading_reports_which_device_failed() {
        let dir = scratch_supply("garbage", "not-a-number\n", "Discharging\n");

        let err = read_reading(&dir, "Wireless Mouse").unwrap_err();
        assert!(err.to_string().contains("Wireless Mouse"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

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
