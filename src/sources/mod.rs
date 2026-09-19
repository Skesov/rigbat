use crate::discovery::Context;
use crate::domain::{BatteryReading, DeviceInfo};

pub mod bluez;
pub mod eightbitdo;
pub mod steelseries;
pub mod sysfs;

#[async_trait::async_trait]
pub trait BatterySource: Send {
    fn device(&self) -> &DeviceInfo;
    /// Polls the device. Err indicates the device is unresponsive or unavailable
    /// (the caller treats this as offline).
    async fn poll(&mut self) -> anyhow::Result<BatteryReading>;
}

/// Transport layer: discovers devices and creates battery sources.
#[async_trait::async_trait]
pub trait BatteryBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn discover(&self, ctx: &Context) -> Vec<Box<dyn BatterySource>>;
}
