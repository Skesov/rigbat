pub mod refresh;
pub mod supervisor;

use tokio::task::JoinSet;

use crate::domain::{BatteryReading, DeviceInfo};
use crate::sources::BatterySource;

/// Polls all sources in parallel. Error from a source becomes None (offline).
/// Result is sorted by device name for stable output.
pub async fn poll_once(
    sources: Vec<Box<dyn BatterySource>>,
) -> Vec<(DeviceInfo, Option<BatteryReading>)> {
    let mut set: JoinSet<(DeviceInfo, Option<BatteryReading>)> = JoinSet::new();
    // A panicking source loses the reading it was producing; without this map
    // it would also lose the device, which would then be missing from the
    // table entirely instead of reading `offline` like every other failure.
    let mut spawned: std::collections::HashMap<tokio::task::Id, DeviceInfo> =
        std::collections::HashMap::new();

    for mut s in sources {
        let info = s.device().clone();
        let handle = set.spawn(async move {
            let reading = match s.poll().await {
                Ok(r) => Some(r),
                Err(e) => {
                    // `{e:#}` prints the whole anyhow context chain. Without this the
                    // reason a device reads as offline — no permission, a STALLed
                    // write, a D-Bus error, a timeout — is indistinguishable to a user.
                    tracing::warn!(device = %s.device().name, "poll failed: {e:#}");
                    None
                }
            };
            (s.device().clone(), reading)
        });
        spawned.insert(handle.id(), info);
    }

    let mut rows = Vec::new();
    while let Some(result) = set.join_next().await {
        match result {
            Ok(row) => rows.push(row),
            Err(e) => match spawned.get(&e.id()) {
                Some(info) => {
                    tracing::error!(device = %info.name, "polling task ended unexpectedly: {e}");
                    rows.push((info.clone(), None));
                }
                None => tracing::error!("a polling task ended unexpectedly: {e}"),
            },
        }
    }

    rows.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};
    use crate::sources::BatterySource;

    struct OkSource {
        info: DeviceInfo,
        reading: BatteryReading,
    }

    struct ErrSource {
        info: DeviceInfo,
    }

    /// A source whose poll panics, to prove the device still reaches the table
    /// as offline instead of disappearing from it.
    struct PanicSource {
        info: DeviceInfo,
    }

    #[async_trait::async_trait]
    impl BatterySource for OkSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            Ok(self.reading)
        }
    }

    #[async_trait::async_trait]
    impl BatterySource for ErrSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            anyhow::bail!("device unavailable")
        }
    }

    #[async_trait::async_trait]
    impl BatterySource for PanicSource {
        fn device(&self) -> &DeviceInfo {
            &self.info
        }

        // clippy::panic has no allow-in-tests config (unlike unwrap_used/expect_used);
        // this panic is the failure being simulated.
        #[expect(clippy::panic)]
        async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
            panic!("source exploded")
        }
    }

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            kind: DeviceKind::Other,
            transport: crate::domain::Transport::Sysfs,
            locator: None,
        }
    }

    fn reading(percent: u8) -> BatteryReading {
        BatteryReading::new(percent, ChargeState::Discharging)
    }

    #[tokio::test]
    async fn err_source_maps_to_none() {
        let sources: Vec<Box<dyn BatterySource>> = vec![Box::new(ErrSource {
            info: device("mouse"),
        })];

        let rows = poll_once(sources).await;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.name, "mouse");
        assert!(rows[0].1.is_none());
    }

    /// A panicking source used to vanish from the output: `join_next` yields
    /// `Err` and the row was dropped, so the table was one device short with
    /// no explanation. The doc contract is that a failure reads as offline.
    #[tokio::test]
    async fn panicking_source_is_reported_offline_not_dropped() {
        let sources: Vec<Box<dyn BatterySource>> = vec![
            Box::new(PanicSource {
                info: device("mouse"),
            }),
            Box::new(OkSource {
                info: device("keyboard"),
                reading: reading(80),
            }),
        ];

        let rows = poll_once(sources).await;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.name, "keyboard");
        assert_eq!(rows[1].0.name, "mouse");
        assert!(rows[1].1.is_none(), "a panicking source reads as offline");
    }

    #[tokio::test]
    async fn all_devices_present() {
        let sources: Vec<Box<dyn BatterySource>> = vec![
            Box::new(OkSource {
                info: device("keyboard"),
                reading: reading(80),
            }),
            Box::new(ErrSource {
                info: device("mouse"),
            }),
        ];

        let rows = poll_once(sources).await;

        assert_eq!(rows.len(), 2);
        let names: Vec<&str> = rows.iter().map(|r| r.0.name.as_str()).collect();
        assert!(names.contains(&"keyboard"));
        assert!(names.contains(&"mouse"));
    }

    #[tokio::test]
    async fn sorted_by_name() {
        let sources: Vec<Box<dyn BatterySource>> = vec![
            Box::new(OkSource {
                info: device("zebra"),
                reading: reading(50),
            }),
            Box::new(OkSource {
                info: device("alpha"),
                reading: reading(90),
            }),
            Box::new(OkSource {
                info: device("mouse"),
                reading: reading(70),
            }),
        ];

        let rows = poll_once(sources).await;

        assert_eq!(rows[0].0.name, "alpha");
        assert_eq!(rows[1].0.name, "mouse");
        assert_eq!(rows[2].0.name, "zebra");
    }
}
