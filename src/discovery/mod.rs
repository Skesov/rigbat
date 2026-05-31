pub mod registry;

use futures_util::future::join_all;

use crate::sources::BatterySource;

/// Опрашивает все бэкенды параллельно, возвращает плоский список источников.
/// Порядок: по реестру (sysfs первый, потом bluez).
pub async fn discover_all() -> Vec<Box<dyn BatterySource>> {
    let backends = registry::backends();
    let futures: Vec<_> = backends.iter().map(|b| b.discover()).collect();
    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}
