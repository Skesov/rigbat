//! Fake sources shared by the `app` tests.

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind, Transport};
use crate::sources::hidraw::NodeReassigned;
use crate::sources::{AccessDenied, BatterySource};

type Poll = Box<dyn FnMut() -> anyhow::Result<BatteryReading> + Send>;

/// A source that answers every poll with whatever `poll` returns.
pub struct FakeSource {
    info: DeviceInfo,
    poll: Poll,
}

#[async_trait::async_trait]
impl BatterySource for FakeSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        (self.poll)()
    }
}

pub fn fake(
    info: &DeviceInfo,
    poll: impl FnMut() -> anyhow::Result<BatteryReading> + Send + 'static,
) -> Box<dyn BatterySource> {
    Box::new(FakeSource {
        info: info.clone(),
        poll: Box::new(poll),
    })
}

pub fn ok(info: &DeviceInfo, percent: u8) -> Box<dyn BatterySource> {
    fake(info, move || Ok(reading(percent)))
}

pub fn err(info: &DeviceInfo) -> Box<dyn BatterySource> {
    fake(info, || anyhow::bail!("device unavailable"))
}

pub fn denied(info: &DeviceInfo) -> Box<dyn BatterySource> {
    fake(info, || {
        Err(anyhow::Error::new(AccessDenied {
            path: "/dev/hidraw7".into(),
        }))
    })
}

pub fn reassigned(info: &DeviceInfo) -> Box<dyn BatterySource> {
    fake(info, || {
        Err(anyhow::Error::new(NodeReassigned {
            node: "hidraw7".to_owned(),
        }))
    })
}

/// A source whose task crashes on its first poll.
pub fn panics(info: &DeviceInfo) -> Box<dyn BatterySource> {
    // clippy::panic has no allow-in-tests config (unlike unwrap_used/expect_used).
    #[expect(clippy::panic)]
    fn explode() -> anyhow::Result<BatteryReading> {
        panic!("simulated source panic")
    }
    fake(info, explode)
}

pub fn device(name: &str) -> DeviceInfo {
    DeviceInfo {
        name: name.to_owned(),
        kind: DeviceKind::Other,
        transport: Transport::Sysfs,
        locator: Some(name.to_owned()),
    }
}

pub fn reading(percent: u8) -> BatteryReading {
    BatteryReading::new(percent, ChargeState::Discharging)
}
