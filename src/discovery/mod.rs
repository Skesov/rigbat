pub mod context;
pub mod registry;

pub use context::Context;

use futures_util::future::join_all;

use crate::sources::BatterySource;

/// One backend's outcome for a single discovery sweep: the sources it found,
/// or the error that stopped it from finding anything this round. Kept
/// separate per backend rather than flattened into one list, so a caller can
/// tell "this backend failed" apart from "this backend honestly found
/// nothing" — `app::supervisor::DeviceRegistry::reconcile` retires a device
/// only when the backend that owns it reported success this sweep.
pub struct BackendSweep {
    pub name: &'static str,
    pub result: anyhow::Result<Vec<Box<dyn BatterySource>>>,
}

/// Queries all backends in parallel, returns one outcome per backend.
/// Order: as registered in `registry::backends()`. Logs each failure once
/// here — the single place a backend's error is observed — so callers never
/// need to log it again per device.
pub async fn discover_all(ctx: &Context) -> Vec<BackendSweep> {
    let backends = registry::backends();
    let futures: Vec<_> = backends.iter().map(|b| b.discover(ctx)).collect();
    let results = join_all(futures).await;
    backends
        .into_iter()
        .zip(results)
        .map(|(backend, result)| {
            match &result {
                Ok(sources) => tracing::debug!(
                    backend = backend.name(),
                    count = sources.len(),
                    "discovery pass"
                ),
                Err(e) => tracing::warn!(backend = backend.name(), "discovery sweep failed: {e:#}"),
            }
            BackendSweep {
                name: backend.name(),
                result,
            }
        })
        .collect()
}

/// Flattens per-backend outcomes into one list of sources, for callers that
/// only want "everything currently discoverable" and have no retirement
/// decision to make (the one-shot CLI paths). A failed backend simply
/// contributes no sources — its error was already logged once by
/// `discover_all` above.
pub fn flatten(sweeps: Vec<BackendSweep>) -> Vec<Box<dyn BatterySource>> {
    sweeps
        .into_iter()
        .filter_map(|sweep| sweep.result.ok())
        .flatten()
        .collect()
}
