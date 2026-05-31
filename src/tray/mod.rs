pub mod icon;
#[allow(unused_imports)]
pub use icon::*;

use ksni::{MenuItem, ToolTip, Tray};
use tokio::sync::watch;

use crate::app::supervisor::TrayState;
use crate::appearance::ColorScheme;
use crate::config::DisplayMode;
use crate::domain::{BatteryReading, ChargeState, DeviceInfo, PrimaryStatus};
use crate::tray::icon::{IconRenderer, Theme};

pub struct TrayApp {
    pub rx: watch::Receiver<TrayState>,
    pub theme_rx: watch::Receiver<ColorScheme>,
    pub renderer: Box<dyn IconRenderer>,
}

impl TrayApp {
    pub fn new(
        rx: watch::Receiver<TrayState>,
        theme_rx: watch::Receiver<ColorScheme>,
        renderer: Box<dyn IconRenderer>,
    ) -> Self {
        Self {
            rx,
            theme_rx,
            renderer,
        }
    }
}

impl Tray for TrayApp {
    fn id(&self) -> String {
        "rigbat".into()
    }

    fn title(&self) -> String {
        "rigbat".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let theme = match *self.theme_rx.borrow() {
            ColorScheme::Dark => Theme::dark(),
            ColorScheme::Light => Theme::light(),
        };
        // TODO(U4a): replace DisplayMode::IconOnly with the value from loaded config.
        self.renderer.render(
            self.rx.borrow().primary_status,
            &theme,
            DisplayMode::IconOnly,
        )
    }

    fn tool_tip(&self) -> ToolTip {
        let state = self.rx.borrow();
        let title = format_primary_summary(&state);
        ToolTip {
            title,
            ..ToolTip::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = self.rx.borrow();
        let mut items: Vec<MenuItem<Self>> = state
            .devices
            .iter()
            .map(|(info, reading)| {
                let label = format_device_entry(info, *reading);
                MenuItem::Standard(ksni::menu::StandardItem {
                    label,
                    enabled: false,
                    ..ksni::menu::StandardItem::default()
                })
            })
            .collect();

        items.push(MenuItem::Separator);

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: "Quit".into(),
            activate: Box::new(|_| std::process::exit(0)),
            ..ksni::menu::StandardItem::default()
        }));

        items
    }
}

/// Formats a summary of the primary device for the tooltip.
pub fn format_primary_summary(state: &TrayState) -> String {
    match state.primary_status {
        PrimaryStatus::Offline => {
            if state.devices.is_empty() {
                "No devices".into()
            } else {
                // Show the name of the first device (primary index or 0)
                let idx = state.primary.unwrap_or(0);
                let name = state
                    .devices
                    .get(idx)
                    .map(|(info, _)| info.name.as_str())
                    .unwrap_or("unknown");
                format!("{name}: offline")
            }
        }
        PrimaryStatus::Ok { percent } => {
            let name = primary_device_name(state);
            format!("{name}: {percent}%")
        }
        PrimaryStatus::Low { percent } => {
            let name = primary_device_name(state);
            format!("{name}: {percent}% (low)")
        }
        PrimaryStatus::Charging { percent } => {
            let name = primary_device_name(state);
            format!("{name}: {percent}% ⚡")
        }
    }
}

fn primary_device_name(state: &TrayState) -> &str {
    let idx = state.primary.unwrap_or(0);
    state
        .devices
        .get(idx)
        .map(|(info, _)| info.name.as_str())
        .unwrap_or("unknown")
}

/// Formats a device entry string for a menu item.
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

    fn make_state(
        devices: Vec<(DeviceInfo, Option<BatteryReading>)>,
        primary: Option<usize>,
        primary_status: PrimaryStatus,
    ) -> TrayState {
        TrayState {
            devices,
            primary,
            primary_status,
        }
    }

    #[test]
    fn format_primary_summary_no_devices() {
        let state = make_state(vec![], None, PrimaryStatus::Offline);
        assert_eq!(format_primary_summary(&state), "No devices");
    }

    #[test]
    fn format_primary_summary_offline_with_device() {
        let state = make_state(
            vec![(device("mouse"), None)],
            Some(0),
            PrimaryStatus::Offline,
        );
        assert_eq!(format_primary_summary(&state), "mouse: offline");
    }

    #[test]
    fn format_primary_summary_ok() {
        let state = make_state(
            vec![(
                device("keyboard"),
                Some(BatteryReading::new(80, ChargeState::Discharging)),
            )],
            Some(0),
            PrimaryStatus::Ok { percent: 80 },
        );
        assert_eq!(format_primary_summary(&state), "keyboard: 80%");
    }

    #[test]
    fn format_primary_summary_low() {
        let state = make_state(
            vec![(
                device("headset"),
                Some(BatteryReading::new(15, ChargeState::Discharging)),
            )],
            Some(0),
            PrimaryStatus::Low { percent: 15 },
        );
        assert_eq!(format_primary_summary(&state), "headset: 15% (low)");
    }

    #[test]
    fn format_primary_summary_charging() {
        let state = make_state(
            vec![(
                device("mouse"),
                Some(BatteryReading::new(60, ChargeState::Charging)),
            )],
            Some(0),
            PrimaryStatus::Charging { percent: 60 },
        );
        assert_eq!(format_primary_summary(&state), "mouse: 60% ⚡");
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
