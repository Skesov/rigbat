pub mod registry;

use futures_util::future::join_all;

use crate::sources::BatterySource;

/// Queries all backends in parallel, returns a flat list of sources.
/// Order: as registered (sysfs first, then bluez).
pub async fn discover_all() -> Vec<Box<dyn BatterySource>> {
    let backends = registry::backends();
    let futures: Vec<_> = backends.iter().map(|b| b.discover()).collect();
    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}
