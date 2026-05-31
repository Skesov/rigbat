#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Mouse,
    Keyboard,
    Headset,
    Controller,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Discharging,
    Charging,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub percent: u8, // инвариант: 0..=100
    pub state: ChargeState,
}

impl BatteryReading {
    /// Создаёт показание, ограничивая percent диапазоном 0..=100.
    pub fn new(percent: u8, state: ChargeState) -> Self {
        Self {
            percent: percent.min(100),
            state,
        }
    }
}
