pub mod icon;
pub mod manager;

use crate::domain::{BatteryReading, ChargeState, DeviceInfo};

/// Formats a device entry string for a menu item or tooltip.
pub fn format_device_entry(info: &DeviceInfo, reading: Option<BatteryReading>) -> String {
    match reading {
        None => format!("{}: offline", info.name),
        Some(r) => {
            let state_str = match r.state {
                ChargeState::Charging => "charging",
                ChargeState::Discharging => "discharging",
                ChargeState::Full => "full",
            };
            format!("{}: {}%  {}", info.name, r.percent, state_str)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            kind: DeviceKind::Other,
        }
    }

    #[test]
    fn format_device_entry_offline() {
        assert_eq!(
            format_device_entry(&device("mouse"), None),
            "mouse: offline"
        );
    }

    #[test]
    fn format_device_entry_discharging() {
        let r = BatteryReading::new(75, ChargeState::Discharging);
        assert_eq!(
            format_device_entry(&device("keyboard"), Some(r)),
            "keyboard: 75%  discharging"
        );
    }

    #[test]
    fn format_device_entry_charging() {
        let r = BatteryReading::new(42, ChargeState::Charging);
        assert_eq!(
            format_device_entry(&device("headset"), Some(r)),
            "headset: 42%  charging"
        );
    }

    #[test]
    fn format_device_entry_full() {
        let r = BatteryReading::new(100, ChargeState::Full);
        assert_eq!(
            format_device_entry(&device("controller"), Some(r)),
            "controller: 100%  full"
        );
    }
}
