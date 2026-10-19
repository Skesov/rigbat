use super::{
    BatteryReading, BootTime, ChargeState, DeviceInfo, DeviceKind, DeviceState, Estimate, Presence,
    Transport,
};

impl DeviceInfo {
    pub fn test(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            kind: DeviceKind::Mouse,
            transport: Transport::Sysfs,
            locator: None,
        }
    }
}

impl DeviceState {
    pub fn test(name: &str) -> Self {
        Self {
            info: DeviceInfo::test(name),
            last_reading: None,
            last_seen: None,
            presence: Presence::Online,
            estimate: Estimate::Unknown,
        }
    }

    pub fn kind(mut self, kind: DeviceKind) -> Self {
        self.info.kind = kind;
        self
    }

    pub fn transport(mut self, transport: Transport) -> Self {
        self.info.transport = transport;
        self
    }

    pub fn presence(mut self, presence: Presence) -> Self {
        self.presence = presence;
        self
    }

    pub fn reading(mut self, reading: impl Into<Option<BatteryReading>>) -> Self {
        self.last_reading = reading.into();
        self
    }

    pub fn seen(mut self, seen: impl Into<Option<BootTime>>) -> Self {
        self.last_seen = seen.into();
        self
    }

    pub fn read(self, percent: u8, seen: BootTime) -> Self {
        self.reading(BatteryReading::new(percent, ChargeState::Discharging))
            .seen(seen)
    }

    pub fn estimate(mut self, estimate: Estimate) -> Self {
        self.estimate = estimate;
        self
    }
}
