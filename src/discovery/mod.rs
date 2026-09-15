pub mod context;
pub mod registry;

pub use context::Context;

use futures_util::future::join_all;

use crate::sources::BatterySource;

/// Queries all backends in parallel, returns a flat list of sources.
/// Order: as registered in `registry::backends()`.
pub async fn discover_all(ctx: &Context) -> Vec<Box<dyn BatterySource>> {
    let backends = registry::backends();
    let futures: Vec<_> = backends.iter().map(|b| b.discover(ctx)).collect();
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
