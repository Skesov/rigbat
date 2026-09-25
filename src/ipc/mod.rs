//! The session-bus contract between `rigbat tray`, which owns device state,
//! and the windows that only show it: `rigbat dashboard` and `rigbat settings`.

pub mod single_instance;

use serde::{Deserialize, Serialize};

use crate::domain::{ChargeState, DeviceKind, DisplayMode, Presence, PrimaryStatus, Transport};

pub const TRAY_NAME: &str = "org.rigbat.Tray";
pub const TRAY_PATH: &str = "/org/rigbat/Tray";
pub const DASHBOARD_NAME: &str = "org.rigbat.Dashboard";
pub const DASHBOARD_PATH: &str = "/org/rigbat/Dashboard";

/// What `org.rigbat.Tray1.State` returns, as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub display_mode: DisplayMode,
    /// The shown devices.
    pub devices: Vec<DeviceCard>,
    /// Devices in `hidden_devices`, for the settings window to list.
    #[serde(default)]
    pub hidden: Vec<DeviceCard>,
}

/// One device, already classified the way its tray icon is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCard {
    pub name: String,
    pub kind: DeviceKind,
    pub transport: Transport,
    pub locator: Option<String>,
    pub presence: Presence,
    pub percent: Option<u8>,
    pub charge: Option<ChargeState>,
    pub status: PrimaryStatus,
    pub stale: bool,
    pub seen_secs_ago: Option<u64>,
    pub remaining_secs: Option<u64>,
    /// Whether a tray icon currently shows this device.
    pub in_tray: bool,
}

#[zbus::proxy(
    interface = "org.rigbat.Tray1",
    default_service = "org.rigbat.Tray",
    default_path = "/org/rigbat/Tray"
)]
pub trait Tray1 {
    fn state(&self) -> zbus::Result<String>;
    fn refresh(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn state_changed(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.rigbat.Dashboard1",
    default_service = "org.rigbat.Dashboard",
    default_path = "/org/rigbat/Dashboard"
)]
pub trait Dashboard1 {
    fn close(&self) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = Snapshot {
            display_mode: DisplayMode::PercentInIcon,
            devices: vec![DeviceCard {
                name: "MX Anywhere 3".to_owned(),
                kind: DeviceKind::Mouse,
                transport: Transport::Bluetooth,
                locator: Some("00:00:5E:00:53:01".to_owned()),
                presence: Presence::Unreachable,
                percent: Some(62),
                charge: Some(ChargeState::Discharging),
                status: PrimaryStatus::Ok { percent: 62 },
                stale: true,
                seen_secs_ago: Some(300),
                remaining_secs: Some(7 * 3600),
                in_tray: true,
            }],
            hidden: Vec::new(),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&json).unwrap(), snapshot);
    }

    /// A tray that predates `hidden` and `locator` still answers readably.
    #[test]
    fn a_snapshot_without_the_later_fields_still_reads() {
        let json = r#"{"display_mode":"icon_only","devices":[{"name":"MX Anywhere 3",
            "kind":"mouse","transport":"bluetooth","presence":"online","percent":62,
            "charge":"discharging","status":{"ok":{"percent":62}},"stale":false,
            "seen_secs_ago":1,"remaining_secs":null,"in_tray":true}]}"#;
        let snapshot: Snapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.devices[0].locator, None);
        assert!(snapshot.hidden.is_empty());
    }

    /// The JSON spelling must be the same wire value `--json` prints.
    #[test]
    fn enum_spellings_match_the_cli_wire_values() {
        for kind in [
            DeviceKind::Mouse,
            DeviceKind::Keyboard,
            DeviceKind::Headset,
            DeviceKind::Controller,
            DeviceKind::Other,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
        }
        for transport in [Transport::Sysfs, Transport::Bluetooth, Transport::Hidraw] {
            assert_eq!(serde_json::to_value(transport).unwrap(), transport.as_str());
        }
    }
}
