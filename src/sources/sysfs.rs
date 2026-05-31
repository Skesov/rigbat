use std::path::PathBuf;

use anyhow::Context as _;

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

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
    pub fn enumerate() -> Vec<SysfsSource> {
        let base = PathBuf::from("/sys/class/power_supply");

        let Ok(entries) = std::fs::read_dir(&base) else {
            return Vec::new();
        };

        let mut sources = Vec::new();

        for entry in entries.flatten() {
            let entry_path = entry.path();

            // Пропустить Mains (AC adapter)
            let type_path = entry_path.join("type");
            if let Ok(kind_str) = std::fs::read_to_string(&type_path)
                && kind_str.trim() == "Mains"
            {
                continue;
            }

            // Требовать наличие файла capacity
            if !entry_path.join("capacity").exists() {
                continue;
            }

            // Имя: из model_name, если файл есть и непустой; иначе имя записи
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

            let info = DeviceInfo {
                name,
                kind: DeviceKind::Other,
            };

            sources.push(SysfsSource {
                info,
                base: entry_path,
            });
        }

        sources
    }
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

        Ok(BatteryReading::new(percent, state))
    }
}

pub struct SysfsBackend;

#[async_trait::async_trait]
impl BatteryBackend for SysfsBackend {
    fn name(&self) -> &'static str {
        "sysfs"
    }

    async fn discover(&self) -> Vec<Box<dyn BatterySource>> {
        SysfsSource::enumerate()
            .into_iter()
            .map(|s| Box::new(s) as Box<dyn BatterySource>)
            .collect()
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
}
