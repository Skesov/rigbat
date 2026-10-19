use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::Context as _;

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, Transport, guess_kind};

use super::{BatteryBackend, BatterySource, Context, hidraw};

struct SysfsSource {
    info: DeviceInfo,
    base: PathBuf, // /sys/class/power_supply/<name>
}

fn parse_status(s: &str) -> ChargeState {
    match s.trim() {
        "Charging" => ChargeState::Charging,
        "Full" => ChargeState::Full,
        _ => ChargeState::Discharging,
    }
}

impl SysfsSource {
    fn enumerate() -> anyhow::Result<Vec<SysfsSource>> {
        Self::enumerate_in(Path::new("/sys/class/power_supply"))
    }

    fn enumerate_in(base: &Path) -> anyhow::Result<Vec<SysfsSource>> {
        let entries =
            std::fs::read_dir(base).with_context(|| format!("reading {}", base.display()))?;

        let mut sources = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("skipping power_supply entry: {e}");
                    continue;
                }
            };
            let entry_path = entry.path();

            let kind = read_attr(&entry_path, "type");
            let has_charge =
                entry_path.join("capacity").exists() || entry_path.join("capacity_level").exists();
            let scope = read_attr(&entry_path, "scope");
            if !should_include(kind.as_deref(), has_charge, scope.as_deref()) {
                continue;
            }

            let name = display_name(&entry_path, &entry.file_name());
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let locator = stable_locator(&entry_path, &dir_name);
            sources.push(SysfsSource {
                info: DeviceInfo {
                    kind: guess_kind(&name),
                    name,
                    transport: Transport::Sysfs,
                    locator: Some(locator),
                },
                base: entry_path,
            });
        }

        Ok(sources)
    }
}

/// An attribute's trimmed text; `None` when it is missing or unreadable.
fn read_attr(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_owned())
}

/// `model_name` when the driver sets a non-empty one, else the directory name.
fn display_name(dir: &Path, dir_name: &OsStr) -> String {
    read_attr(dir, "model_name")
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| dir_name.to_string_lossy().into_owned())
}

/// Whether a `/sys/class/power_supply/<name>` entry is a peripheral rigbat tracks.
/// Peripheral supplies (`hid-logitech-hidpp` and friends) set `scope` to `Device`;
/// a missing `scope` is excluded, because a peripheral missing from the tray costs
/// less than the host's own battery appearing as one.
fn should_include(kind: Option<&str>, has_charge: bool, scope: Option<&str>) -> bool {
    if kind == Some("Mains") {
        return false;
    }
    if !has_charge {
        return false;
    }
    scope == Some("Device")
}

/// A locator for a power_supply directory that survives a reconnect.
///
/// The directory name is not identity. `hid-logitech-hidpp` builds it from a
/// module-global counter — `n = atomic_inc_return(&battery_no) - 1;` then
/// `sprintf(battery->name, "hidpp_battery_%ld", n)` — so the same mouse gets
/// another name after a reconnect or reboot. The kernel registers the supply
/// with the HID device as its parent
/// (`devm_power_supply_register(&hidpp->hid_dev->dev, …)`), so
/// `<supply>/device/uevent` is that device's uevent and
/// [`hidraw::stable_locator`] applies. A supply with no readable HID uevent
/// keeps the directory name.
fn stable_locator(entry_path: &Path, dir_name: &str) -> String {
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

        // The driver decides whether a read touches the hardware (ACPI evaluates a method per read).
        let reading = tokio::task::spawn_blocking(move || read_reading(&base, &name))
            .await
            .context("spawn_blocking")??;

        tracing::debug!(
            device = %self.info.name,
            percent = reading.percent,
            coarse = reading.coarse,
            "sysfs poll"
        );
        Ok(reading)
    }
}

fn parse_capacity(s: &str) -> Result<u8, std::num::ParseIntError> {
    s.trim().parse()
}

/// Low sits at the default low threshold (20) so the driver's own "low" notifies.
fn parse_capacity_level(s: &str) -> Option<u8> {
    match s.trim() {
        "Critical" => Some(5),
        "Low" => Some(20),
        "Normal" => Some(60),
        "High" => Some(85),
        "Full" => Some(100),
        _ => None,
    }
}

/// Reads `capacity` (or, if absent, `capacity_level` as a coarse reading) and `status`.
fn read_reading(base: &Path, name: &str) -> anyhow::Result<BatteryReading> {
    let status_str = std::fs::read_to_string(base.join("status"))
        .with_context(|| format!("reading status for {name}"))?;
    let state = parse_status(&status_str);

    match std::fs::read_to_string(base.join("capacity")) {
        Ok(capacity_str) => {
            let percent = parse_capacity(&capacity_str)
                .with_context(|| format!("parsing capacity for {name}"))?;
            Ok(BatteryReading::new(percent, state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let level = std::fs::read_to_string(base.join("capacity_level"))
                .with_context(|| format!("reading capacity_level for {name}"))?;
            let percent = parse_capacity_level(&level)
                .with_context(|| format!("{name} reports capacity_level {:?}", level.trim()))?;
            Ok(BatteryReading::new_coarse(percent, state))
        }
        Err(e) => Err(e).with_context(|| format!("reading capacity for {name}")),
    }
}

pub struct SysfsBackend;

#[async_trait::async_trait]
impl BatteryBackend for SysfsBackend {
    fn name(&self) -> &'static str {
        "sysfs"
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        let sources = tokio::task::spawn_blocking(SysfsSource::enumerate)
            .await
            .context("spawn_blocking")??;
        Ok(sources
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
    /// Removed on drop, also when an assertion fails.
    struct ScratchDir(PathBuf);

    impl std::ops::Deref for ScratchDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch_dir(test_name: &str, files: &[(&str, &str)]) -> ScratchDir {
        let dir = std::env::temp_dir().join(format!(
            "rigbat-sysfs-test-{test_name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (file, content) in files {
            std::fs::write(dir.join(file), content).unwrap();
        }
        ScratchDir(dir)
    }

    fn scratch_supply(test_name: &str, capacity: &str, status: &str) -> ScratchDir {
        scratch_dir(test_name, &[("capacity", capacity), ("status", status)])
    }

    /// The attribute set `xpadneo` registers: no `capacity`, only `capacity_level`.
    fn xpadneo_files(level: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![
            ("type", "Battery\n"),
            ("scope", "Device\n"),
            ("model_name", "Xbox Wireless Controller\n"),
            ("capacity_level", level),
            ("status", "Discharging\n"),
        ]
    }

    #[test]
    fn read_reading_maps_capacity_level_to_a_coarse_reading() {
        let dir = scratch_dir("level-normal", &xpadneo_files("Normal\n"));

        let reading = read_reading(&dir, "pad").unwrap();
        assert_eq!(reading.percent, 60);
        assert_eq!(reading.state, ChargeState::Discharging);
        assert!(reading.coarse);
    }

    #[test]
    fn read_reading_prefers_capacity_over_capacity_level() {
        let dir = scratch_dir(
            "level-and-capacity",
            &[
                ("capacity", "73\n"),
                ("capacity_level", "Normal\n"),
                ("status", "Discharging\n"),
            ],
        );

        let reading = read_reading(&dir, "mouse").unwrap();
        assert_eq!(reading.percent, 73);
        assert!(!reading.coarse);
    }

    #[test]
    fn read_reading_unknown_level_is_no_reading() {
        let dir = scratch_dir("level-unknown", &xpadneo_files("Unknown\n"));

        let err = read_reading(&dir, "Xbox Wireless Controller").unwrap_err();
        assert!(err.to_string().contains("Xbox Wireless Controller"));
        assert!(
            !err.is::<crate::sources::AccessDenied>(),
            "a pad without a report yet is not a permission problem"
        );

        std::fs::write(dir.join("capacity_level"), "Normal\n").unwrap();
        let reading = read_reading(&dir, "Xbox Wireless Controller").unwrap();
        assert_eq!(reading.percent, 60);
        assert!(reading.coarse);
    }

    #[test]
    fn capacity_level_mapping_matches_the_low_threshold() {
        let low = crate::config::DEFAULT_LOW_THRESHOLD;
        let levels = ["Critical", "Low", "Normal", "High", "Full"].map(parse_capacity_level);
        assert_eq!(levels, [Some(5), Some(20), Some(60), Some(85), Some(100)]);
        assert!(
            levels[1].is_some_and(|p| p <= low),
            "Low must read as low by default"
        );
        assert!(
            levels[2].is_some_and(|p| p > low),
            "Normal must not read as low"
        );
        assert_eq!(parse_capacity_level("Unknown"), None);
        assert_eq!(parse_capacity_level("low"), None);
    }

    #[test]
    fn enumerate_includes_a_level_only_device_supply_and_keeps_scope_filtering() {
        let root = scratch_dir("enumerate-level", &[]);
        let make = |name: &str, files: &[(&str, &str)]| {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            for (file, content) in files {
                std::fs::write(d.join(file), content).unwrap();
            }
        };
        make("xpadneo_battery_0", &xpadneo_files("High\n"));
        let mut system = xpadneo_files("High\n");
        system[1] = ("scope", "System\n");
        make("BAT0", &system);
        let mut no_charge = xpadneo_files("High\n");
        no_charge.retain(|(f, _)| *f != "capacity_level");
        make("no_charge", &no_charge);

        let sources = SysfsSource::enumerate_in(&root).unwrap();
        let names: Vec<&str> = sources.iter().map(|s| s.info.name.as_str()).collect();
        assert_eq!(names, ["Xbox Wireless Controller"]);
        assert_eq!(sources[0].info.kind, crate::domain::DeviceKind::Controller);
        assert_eq!(
            read_reading(&sources[0].base, "pad").unwrap(),
            BatteryReading::new_coarse(85, ChargeState::Discharging)
        );
    }

    /// A HID-backed supply keys on the device's own serial, so the locator is
    /// the same string before and after the kernel renumbers the directory.
    #[test]
    fn stable_locator_prefers_the_hid_uevent() {
        let dir = scratch_supply("locator-hid", "50\n", "Discharging\n");
        std::fs::create_dir_all(dir.join("device")).unwrap();
        std::fs::write(
            dir.join("device/uevent"),
            "HID_ID=0003:0000046D:0000B02Y\nHID_PHYS=usb-0000:10:00.0-3/input2:1\nHID_UNIQ=00:00:5e:00:53:01\n",
        )
        .unwrap();

        assert_eq!(stable_locator(&dir, "hidpp_battery_6"), "00:00:5e:00:53:01");
    }

    /// A supply with no HID parent — anything that is not a HID device — keeps
    /// the directory name it always had.
    #[test]
    fn stable_locator_falls_back_to_the_directory_name() {
        let dir = scratch_supply("locator-plain", "50\n", "Discharging\n");

        assert_eq!(stable_locator(&dir, "some_battery"), "some_battery");
    }

    #[test]
    fn read_reading_parses_capacity_and_status() {
        let dir = scratch_supply("ok", "77\n", "Charging\n");

        let reading = read_reading(&dir, "test").unwrap();
        assert_eq!(reading.percent, 77);
        assert_eq!(reading.state, ChargeState::Charging);
    }

    #[test]
    fn read_reading_reports_which_device_failed() {
        let dir = scratch_supply("garbage", "not-a-number\n", "Discharging\n");

        let err = read_reading(&dir, "Wireless Mouse").unwrap_err();
        assert!(err.to_string().contains("Wireless Mouse"));
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

    #[test]
    fn should_include_accepts_device_scoped_entry() {
        assert!(should_include(Some("Battery"), true, Some("Device")));
    }

    #[test]
    fn should_include_rejects_system_scoped_entry() {
        // A laptop's BAT0 that explicitly reports the host scope.
        assert!(!should_include(Some("Battery"), true, Some("System")));
    }

    #[test]
    fn should_include_rejects_missing_scope() {
        assert!(!should_include(Some("Battery"), true, None));
    }

    #[test]
    fn should_include_rejects_mains() {
        assert!(!should_include(Some("Mains"), true, Some("Device")));
    }

    #[test]
    fn should_include_rejects_missing_charge() {
        assert!(!should_include(Some("Battery"), false, Some("Device")));
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn any_accepted_capacity_reads_in_range(capacity in any::<String>(), status in any::<String>()) {
                if let Ok(percent) = parse_capacity(&capacity) {
                    prop_assert!(BatteryReading::new(percent, parse_status(&status)).percent <= 100);
                }
            }

            #[test]
            fn capacity_round_trips_with_sysfs_whitespace(
                percent in 0u8..=100,
                lead in "[ \t]{0,2}",
                trail in "[ \t\n]{0,2}",
            ) {
                prop_assert_eq!(parse_capacity(&format!("{lead}{percent}{trail}")), Ok(percent));
            }

            #[test]
            fn capacity_rejects_non_numbers(capacity in "[^0-9]*") {
                prop_assert!(parse_capacity(&capacity).is_err());
            }

            #[test]
            fn capacity_level_round_trips_with_sysfs_whitespace(
                (text, percent) in prop_oneof![
                    Just(("Critical", 5u8)),
                    Just(("Low", 20)),
                    Just(("Normal", 60)),
                    Just(("High", 85)),
                    Just(("Full", 100)),
                ],
                trail in "[ \t\n]{0,2}",
            ) {
                prop_assert_eq!(parse_capacity_level(&format!("{text}{trail}")), Some(percent));
            }

            #[test]
            fn capacity_level_never_invents_a_reading(level in any::<String>()) {
                let known = ["Critical", "Low", "Normal", "High", "Full"];
                prop_assert_eq!(parse_capacity_level(&level).is_some(), known.contains(&level.trim()));
            }

            #[test]
            fn status_round_trips_with_sysfs_whitespace(
                (text, state) in prop_oneof![
                    Just(("Charging", ChargeState::Charging)),
                    Just(("Full", ChargeState::Full)),
                    Just(("Discharging", ChargeState::Discharging)),
                    Just(("Not charging", ChargeState::Discharging)),
                ],
                trail in "[ \t\n]{0,2}",
            ) {
                prop_assert_eq!(parse_status(&format!("{text}{trail}")), state);
            }
        }
    }
}
