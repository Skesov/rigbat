mod migration;
pub mod supervisor;

use tokio::task::JoinSet;

use crate::domain::{DeviceInfo, PollOutcome, roster_order};
use crate::sources::{AccessDenied, BatterySource};

/// Polls all sources in parallel. An error from a source becomes `Failed`, or
/// `NoAccess` when it was a permission denial. Sorted by device name.
pub async fn poll_once(sources: Vec<Box<dyn BatterySource>>) -> Vec<(DeviceInfo, PollOutcome)> {
    let mut set: JoinSet<(DeviceInfo, PollOutcome)> = JoinSet::new();
    // A panicking source loses the reading it was producing; without this map
    // it would also lose the device, which would then be missing from the
    // table entirely instead of reading `offline` like every other failure.
    let mut spawned: std::collections::HashMap<tokio::task::Id, DeviceInfo> =
        std::collections::HashMap::new();

    for mut s in sources {
        let info = s.device().clone();
        let handle = set.spawn(async move {
            let outcome = match s.poll().await {
                Ok(r) => PollOutcome::Reading(r),
                Err(e) => {
                    // `{e:#}` prints the whole anyhow context chain. Without this the
                    // reason a device reads as offline — no permission, a STALLed
                    // write, a D-Bus error, a timeout — is indistinguishable to a user.
                    tracing::warn!(device = %s.device().name, "poll failed: {e:#}");
                    if e.is::<AccessDenied>() {
                        PollOutcome::NoAccess
                    } else {
                        PollOutcome::Failed
                    }
                }
            };
            (s.device().clone(), outcome)
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
                    rows.push((info.clone(), PollOutcome::Failed));
                }
                None => tracing::error!("a polling task ended unexpectedly: {e}"),
            },
        }
    }

    rows.sort_by_cached_key(|(info, outcome)| roster_order(&info.name, outcome.presence()));
    rows
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::{denied, device, err, ok, panics};
    use super::*;

    #[tokio::test]
    async fn err_source_maps_to_failed() {
        let rows = poll_once(vec![err(&device("mouse"))]).await;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.name, "mouse");
        assert_eq!(rows[0].1, PollOutcome::Failed);
    }

    #[tokio::test]
    async fn access_denied_maps_to_no_access() {
        let rows = poll_once(vec![denied(&device("mouse"))]).await;

        assert_eq!(rows[0].1, PollOutcome::NoAccess);
    }

    #[tokio::test]
    async fn panicking_source_is_reported_offline_not_dropped() {
        let rows = poll_once(vec![panics(&device("mouse")), ok(&device("keyboard"), 80)]).await;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.name, "keyboard");
        assert_eq!(rows[1].0.name, "mouse");
        assert_eq!(
            rows[1].1,
            PollOutcome::Failed,
            "a panicking source reads as offline"
        );
    }

    #[tokio::test]
    async fn online_devices_come_first_then_by_name() {
        let rows = poll_once(vec![
            err(&device("alpha")),
            ok(&device("zebra"), 50),
            ok(&device("Mouse"), 70),
        ])
        .await;

        let names: Vec<&str> = rows.iter().map(|r| r.0.name.as_str()).collect();
        assert_eq!(names, ["Mouse", "zebra", "alpha"]);
    }
}
