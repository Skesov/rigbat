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

    for mut s in sources {
        set.spawn(async move {
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
    }

    let mut rows = Vec::new();
    while let Some(result) = set.join_next().await {
        if let Ok(row) = result {
            rows.push(row);
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
