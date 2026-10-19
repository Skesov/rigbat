//! `TrayState` and `Config` builders shared by the tray's unit tests.

use std::time::Duration;

use crate::config::Config;
use crate::domain::{
    BatteryReading, ChargeState, DeviceId, DeviceInfo, DeviceState, Presence, Transport, TrayState,
};

pub(super) fn id(name: &str, transport: Transport, locator: Option<&str>) -> DeviceId {
    DeviceId {
        name: name.to_owned(),
        transport,
        locator: locator.map(str::to_owned),
    }
}

/// The `DeviceId` of a `DeviceInfo::test` device.
pub(super) fn key(name: &str) -> DeviceId {
    DeviceInfo::test(name).id()
}

pub(super) fn make_reading(percent: u8) -> BatteryReading {
    BatteryReading::new(percent, ChargeState::Discharging)
}

/// A device that is not `Online` but still remembers a reading taken
/// `age` ago — the state a sleeping Bluetooth peripheral is in, and the
/// one `Config::hide_offline_after` puts a shelf life on.
pub(super) fn retained(name: &str, percent: u8, age: Duration) -> DeviceState {
    DeviceState::test(name)
        .presence(Presence::Unreachable)
        .reading(make_reading(percent))
        .seen(crate::clock::now().checked_sub(age))
}

/// Builds a `TrayState` from (info, reading) pairs: `Some` reading means
/// `Online`, `None` means never seen (`Unreachable`, nothing retained).
pub(super) fn make_state(devices: Vec<(DeviceInfo, Option<BatteryReading>)>) -> TrayState {
    TrayState {
        devices: devices
            .into_iter()
            .map(|(info, reading)| DeviceState {
                info,
                ..match reading {
                    Some(reading) => DeviceState::test("")
                        .reading(reading)
                        .seen(crate::clock::now()),
                    None => DeviceState::test("").presence(Presence::Unreachable),
                }
            })
            .collect(),
    }
}

pub(super) fn cfg_with_primary(primary: Option<&str>) -> Config {
    Config {
        primary_device: primary.map(|s| s.to_string()),
        ..Config::default()
    }
}

/// One mouse seen over sysfs/HID++ and over Bluetooth: the project does
/// not dedup, so these are two devices.
pub(super) fn same_name_two_transports(bt_presence: Presence) -> TrayState {
    let mx = |transport, locator: &str, percent, presence| DeviceState {
        info: DeviceInfo {
            transport,
            locator: Some(locator.to_owned()),
            ..DeviceInfo::test("MX")
        },
        ..DeviceState::test("MX")
            .presence(presence)
            .reading(make_reading(percent))
            .seen(crate::clock::now())
    };
    TrayState {
        devices: vec![
            mx(Transport::Sysfs, "hidpp_0", 70, Presence::Unreachable),
            mx(Transport::Bluetooth, "AA:BB", 40, bt_presence),
        ],
    }
}

pub(super) fn no_access(name: &str) -> DeviceState {
    DeviceState::test(name).presence(Presence::NoAccess)
}

pub(super) fn saved(_: &Config) -> anyhow::Result<()> {
    Ok(())
}
