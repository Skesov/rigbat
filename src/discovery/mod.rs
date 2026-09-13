pub mod registry;

use futures_util::future::join_all;

use crate::sources::BatterySource;

/// Queries all backends in parallel, returns a flat list of sources.
/// Order: as registered in `registry::backends()`.
pub async fn discover_all() -> Vec<Box<dyn BatterySource>> {
    let backends = registry::backends();
    let futures: Vec<_> = backends.iter().map(|b| b.discover()).collect();
    let results = join_all(futures).await;
    for (backend, sources) in backends.iter().zip(&results) {
        tracing::debug!(
            backend = backend.name(),
            count = sources.len(),
            "discovery pass"
        );
    }
    results.into_iter().flatten().collect()
}
