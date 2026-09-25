//! `TrayState` and `Config` builders shared by the tray's unit tests.

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::domain::{
    BatteryReading, ChargeState, DeviceId, DeviceInfo, DeviceKind, DeviceState, Presence,
    Transport, TrayState,
};

pub(super) fn id(name: &str, transport: Transport, locator: Option<&str>) -> DeviceId {
    DeviceId {
        name: name.to_owned(),
        transport,
        locator: locator.map(str::to_owned),
    }
}

/// The `DeviceId` of a `make_info` device.
pub(super) fn key(name: &str) -> DeviceId {
    make_info(name).id()
}

pub(super) fn make_info(name: &str) -> DeviceInfo {
    DeviceInfo {
        name: name.to_string(),
        kind: DeviceKind::Mouse,
        transport: Transport::Sysfs,
        locator: None,
    }
}

pub(super) fn make_reading(percent: u8) -> BatteryReading {
    BatteryReading::new(percent, ChargeState::Discharging)
}

/// A device that is not `Online` but still remembers a reading taken
/// `age` ago — the state a sleeping Bluetooth peripheral is in, and the
/// one `RETAINED_ICON_MAX_AGE` puts a shelf life on.
pub(super) fn retained(name: &str, percent: u8, age: Duration) -> DeviceState {
    DeviceState {
        info: make_info(name),
        last_reading: Some(make_reading(percent)),
        last_seen: Instant::now().checked_sub(age),
        presence: Presence::Unreachable,
        estimate: crate::domain::Estimate::Unknown,
    }
}

/// Builds a `TrayState` from (info, reading) pairs: `Some` reading means
/// `Online`, `None` means never seen (`Unreachable`, nothing retained) —
/// matching what these tests exercised before presence existed.
pub(super) fn make_state(devices: Vec<(DeviceInfo, Option<BatteryReading>)>) -> TrayState {
    TrayState {
        devices: devices
            .into_iter()
            .map(|(info, reading)| DeviceState {
                info,
                last_reading: reading,
                last_seen: reading.map(|_| Instant::now()),
                presence: if reading.is_some() {
                    Presence::Online
                } else {
                    Presence::Unreachable
                },
                estimate: crate::domain::Estimate::Unknown,
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
            name: "MX".to_owned(),
            kind: DeviceKind::Mouse,
            transport,
            locator: Some(locator.to_owned()),
        },
        last_reading: Some(make_reading(percent)),
        last_seen: Some(Instant::now()),
        presence,
        estimate: crate::domain::Estimate::Unknown,
    };
    TrayState {
        devices: vec![
            mx(Transport::Sysfs, "hidpp_0", 70, Presence::Unreachable),
            mx(Transport::Bluetooth, "AA:BB", 40, bt_presence),
        ],
    }
}

pub(super) fn no_access(name: &str) -> DeviceState {
    DeviceState {
        info: make_info(name),
        last_reading: None,
        last_seen: None,
        presence: Presence::NoAccess,
        estimate: crate::domain::Estimate::Unknown,
    }
}

pub(super) fn saved(_: &Config) -> anyhow::Result<()> {
    Ok(())
}
