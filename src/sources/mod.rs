use crate::discovery::Context;
use crate::domain::{BatteryReading, DeviceInfo};

pub mod bluez;
pub mod eightbitdo;
pub mod hidraw;
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
///
/// `discover` returns `Err` when the sweep itself failed (a bus call errored,
/// a directory could not be read) — distinct from `Ok(vec![])`, which means
/// the backend looked and honestly found nothing. `discovery::discover_all`
/// and `app::supervisor::DeviceRegistry::reconcile` depend on that
/// distinction: a failed sweep must not be mistaken for every one of this
/// backend's devices having vanished.
#[async_trait::async_trait]
pub trait BatteryBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn discover(&self, ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>>;
}
