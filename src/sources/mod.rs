#![allow(dead_code)]

use crate::domain::{BatteryReading, DeviceInfo};

pub mod bluez;
pub mod sysfs;

#[async_trait::async_trait]
pub trait BatterySource: Send {
    fn device(&self) -> &DeviceInfo;
    /// Опрашивает устройство. Err — устройство не отвечает/недоступно
    /// (вызывающий трактует как offline).
    async fn poll(&mut self) -> anyhow::Result<BatteryReading>;
}

/// Транспортный слой: находит устройства и создаёт источники.
#[async_trait::async_trait]
pub trait BatteryBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn discover(&self) -> Vec<Box<dyn BatterySource>>;
}
