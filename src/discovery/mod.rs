pub mod registry;

use futures_util::future::join_all;

use crate::sources::{BatteryBackend, BatterySource, Context};

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
    sweep(registry::backends(), ctx).await
}

async fn sweep(backends: Vec<Box<dyn BatteryBackend>>, ctx: &Context) -> Vec<BackendSweep> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BatteryReading, DeviceInfo, DeviceKind, Transport};

    struct Found(DeviceInfo);

    #[async_trait::async_trait]
    impl BatterySource for Found {
        fn device(&self) -> &DeviceInfo {
            &self.0
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            anyhow::bail!("not polled in these tests")
        }
    }

    /// Finds `devices`, or fails the sweep when `None`.
    struct Fake {
        name: &'static str,
        devices: Option<&'static [&'static str]>,
    }

    #[async_trait::async_trait]
    impl BatteryBackend for Fake {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn discover(&self, _: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
            let Some(devices) = self.devices else {
                anyhow::bail!("bus call failed");
            };
            Ok(devices
                .iter()
                .map(|name| {
                    Box::new(Found(DeviceInfo {
                        name: (*name).to_owned(),
                        kind: DeviceKind::Mouse,
                        transport: Transport::Hidraw,
                        locator: None,
                    })) as Box<dyn BatterySource>
                })
                .collect())
        }
    }

    fn backends() -> Vec<Box<dyn BatteryBackend>> {
        vec![
            Box::new(Fake {
                name: "empty",
                devices: Some(&[]),
            }),
            Box::new(Fake {
                name: "failing",
                devices: None,
            }),
            Box::new(Fake {
                name: "found",
                devices: Some(&["mouse", "headset"]),
            }),
        ]
    }

    /// The supervisor retires a device only when its backend's sweep succeeded,
    /// so a failure must not reach it as an empty result.
    #[tokio::test]
    async fn a_failing_backend_is_reported_apart_from_an_empty_one() {
        let sweeps = sweep(backends(), &Context::new()).await;

        let outcomes: Vec<(&str, Option<usize>)> = sweeps
            .iter()
            .map(|s| (s.name, s.result.as_ref().ok().map(Vec::len)))
            .collect();
        assert_eq!(
            outcomes,
            [("empty", Some(0)), ("failing", None), ("found", Some(2))]
        );
    }

    #[tokio::test]
    async fn flatten_keeps_every_source_a_successful_backend_found() {
        let sweeps = sweep(backends(), &Context::new()).await;

        let names: Vec<String> = flatten(sweeps)
            .iter()
            .map(|s| s.device().name.clone())
            .collect();
        assert_eq!(names, ["mouse", "headset"]);
    }
}
