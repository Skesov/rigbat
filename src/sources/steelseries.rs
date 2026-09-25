//! SteelSeries HID backend — reads charge via a dedicated config interface.
//!
//! Protocol: vendor 0x1038, USB interface 3.
//! Write a full 64-byte output report prefixed by report ID `0x00` and starting
//! with `0xD2` (`0x92` battery query | `0x40` wireless flag), read response:
//! `resp[0] == 0xD2`, `resp[1]` bit 7 = charging, bits 0-6 = step (5% each),
//! `percent = (step - 1) * 5`, clamp to 0..=100. A dongle whose device is asleep
//! or off answers `40 ff` instead — the wireless flag with no data.
//!
//! Discovery: [`FAMILY`] through `hidraw::discover`, filtered to interface 3.

use std::{
    fs::File,
    io::{Read as _, Write as _},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

use super::{BatteryBackend, BatterySource, Context, hidraw};

// ── Protocol constants ────────────────────────────────────────────────────────

const BATTERY_QUERY: u8 = 0xD2;

/// The wireless flag on its own. A dongle whose mouse is asleep or switched off
/// answers `40 ff` instead of echoing the command, so this marks "the dongle is
/// there, the device is not".
const WIRELESS_FLAG: u8 = 0x40;

/// The level byte a dongle sends when it has no reading to report.
const LEVEL_UNAVAILABLE: u8 = 0xFF;

/// Payload size of the output report on the config interface, from its report
/// descriptor (`Report Size 8`, `Report Count 0x40`). The device STALLs a short write.
const OUTPUT_REPORT_LEN: usize = 64;

/// Response timeout from the device (milliseconds).
const POLL_TIMEOUT_MS: u16 = 1000;

// ── Device table ─────────────────────────────────────────────────────────────

/// A new model is +1 line in `models` only while it shares this family's wire
/// protocol: `BATTERY_QUERY`, `OUTPUT_REPORT_LEN` and the interface are
/// family-wide, not per-model. A wired variant carries a different product ID and
/// drops the `0x40` wireless flag from the query byte; a model whose config
/// interface declares another report length needs a shorter write. Check the
/// model's report descriptor before assuming one line is enough.
pub static FAMILY: hidraw::HidrawFamily = hidraw::HidrawFamily {
    vendor: 0x1038,
    models: &[hidraw::HidrawModel {
        product: 0x1852,
        name: "SteelSeries Aerox 5 Wireless",
        kind: DeviceKind::Mouse,
    }],
    interface: Some(3),
};

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct SteelSeriesBackend;

pub struct SteelSeriesSource {
    info: DeviceInfo,
    dev_path: PathBuf, // /dev/hidrawN
    identity: hidraw::NodeIdentity,
    /// Open `/dev/hidrawN`, kept for the source's lifetime. `None` before the first
    /// successful poll and after an I/O error invalidated it (the node is recreated
    /// with a new minor when the device re-enumerates, so a stale fd must be dropped).
    handle: Option<File>,
}

#[async_trait::async_trait]
impl BatteryBackend for SteelSeriesBackend {
    fn name(&self) -> &'static str {
        "steelseries"
    }

    fn hidraw_family(&self) -> Option<&'static hidraw::HidrawFamily> {
        Some(&FAMILY)
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        let devices = hidraw::discover(&FAMILY).await?;
        Ok(devices
            .into_iter()
            .map(|device| -> Box<dyn BatterySource> {
                Box::new(SteelSeriesSource {
                    info: device.info,
                    dev_path: device.dev_path,
                    identity: device.identity,
                    handle: None,
                })
            })
            .collect())
    }
}

// ── Source ───────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl BatterySource for SteelSeriesSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let path = self.dev_path.clone();
        let identity = self.identity.clone();
        let handle = self.handle.take();

        // Blocking I/O: nix::poll() parks the thread. Move the handle in and back out
        // so a healthy descriptor survives across polls.
        //
        // If DeviceRegistry::reconcile aborts this task while this closure is
        // in flight, AbortHandle::abort() cancels the enclosing task future
        // but not the spawn_blocking closure itself: the OS thread keeps
        // running poll_device to completion (bounded by POLL_TIMEOUT_MS)
        // before the descriptor is dropped.
        let (result, handle) = tokio::task::spawn_blocking(move || {
            let mut handle = handle;
            let result = poll_device(&path, &identity, &mut handle);
            (result, handle)
        })
        .await
        .context("spawn_blocking")?;

        self.handle = handle;
        if let Ok(reading) = &result {
            tracing::debug!(device = %self.info.name, percent = reading.percent, "steelseries poll");
        }
        result
    }
}

/// Synchronous polling of the device via /dev/hidrawN, reusing `handle` when present.
fn poll_device(
    dev_path: &Path,
    identity: &hidraw::NodeIdentity,
    handle: &mut Option<File>,
) -> anyhow::Result<BatteryReading> {
    let result = poll_device_inner(dev_path, identity, handle);
    // hidraw minor numbers are not stable across re-enumeration, so a cached fd for a
    // device that came back is pointing at a dead character device — clear it and let
    // the next poll reopen by node name instead of retrying a stale descriptor forever.
    clear_handle_on_error(handle, &result);
    result
}

fn poll_device_inner(
    dev_path: &Path,
    identity: &hidraw::NodeIdentity,
    handle: &mut Option<File>,
) -> anyhow::Result<BatteryReading> {
    use std::os::fd::AsFd as _;

    if handle.is_none() {
        let file = hidraw::open_verified(
            Path::new(hidraw::SYSFS_HIDRAW),
            dev_path,
            identity,
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK),
        )?;
        *handle = Some(file);
    }

    let file = handle
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("handle unexpectedly empty after open"))?;

    file.write_all(&battery_query_report())
        .context("writing battery query")?;

    // Drain the buffer until we get the expected response with an overall timeout.
    let mut buf = [0u8; 64];
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(u64::from(POLL_TIMEOUT_MS));

    loop {
        let remaining_ms = deadline
            .checked_duration_since(std::time::Instant::now())
            .map(|d| d.as_millis().min(u64::from(u16::MAX) as u128) as u16)
            .unwrap_or(0);

        if remaining_ms == 0 {
            anyhow::bail!("timeout waiting for response from {}", dev_path.display());
        }

        let mut poll_fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
        let ready = poll(&mut poll_fds, PollTimeout::from(remaining_ms)).context("poll()")?;

        if ready == 0 {
            anyhow::bail!("timeout waiting for response from {}", dev_path.display());
        }

        let revents = poll_fds[0].revents().unwrap_or(PollFlags::empty());
        if !revents.contains(PollFlags::POLLIN) {
            anyhow::bail!(
                "unexpected poll events {:?} from {}",
                revents,
                dev_path.display()
            );
        }

        let n = file.read(&mut buf).context("reading hidraw response")?;
        if n == 0 {
            anyhow::bail!("EOF reading from {}", dev_path.display());
        }

        match classify_response(&buf[..n]) {
            Response::Reading(reading) => return Ok(reading),
            Response::DeviceUnreachable => {
                anyhow::bail!("device is asleep or off (dongle reported no battery data)")
            }
            Response::Unrelated => {}
        }
    }
}

/// Clears `handle` when `result` is `Err`, so a poll failure always drops the descriptor
/// instead of relying on each error path to remember to do it.
fn clear_handle_on_error<T, U>(handle: &mut Option<T>, result: &anyhow::Result<U>) {
    if result.is_err() {
        *handle = None;
    }
}

// ── Pure functions ────────────────────────────────────────────────────────────

/// Parses HID response: `buf[0] == 0xD2`, `buf[1]` bit 7 = charging, bits 0-6 = step.
///
/// `percent = (step - 1) * 5`, clamped to 0..=100.
/// Builds the battery query write: the report-ID byte `0x00` (the config
/// interface's collection declares no report IDs) followed by the full 64-byte
/// output report the descriptor declares (`Output (usage 0xF1), Report Size 8,
/// Report Count 0x40`). A short write is STALLed with `EPIPE`.
pub fn battery_query_report() -> [u8; 1 + OUTPUT_REPORT_LEN] {
    let mut request = [0u8; 1 + OUTPUT_REPORT_LEN];
    request[1] = BATTERY_QUERY;
    request
}

/// What a report read from the config interface turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    Reading(BatteryReading),
    /// The dongle answered, but the device behind it has no data to give.
    /// Distinguished from `Unrelated` so the caller stops instead of draining
    /// until the timeout: a sleeping mouse is the common case, and waiting a
    /// full `POLL_TIMEOUT_MS` for it blocks a `spawn_blocking` thread for a
    /// second on every poll.
    DeviceUnreachable,
    /// Some other report on the same interface. Keep reading.
    Unrelated,
}

pub fn classify_response(buf: &[u8]) -> Response {
    if buf.len() >= 2 && buf[0] == WIRELESS_FLAG && buf[1] == LEVEL_UNAVAILABLE {
        return Response::DeviceUnreachable;
    }
    match parse_battery_response(buf) {
        Some(reading) => Response::Reading(reading),
        None => Response::Unrelated,
    }
}

pub fn parse_battery_response(buf: &[u8]) -> Option<BatteryReading> {
    if buf.len() < 2 || buf[0] != BATTERY_QUERY {
        return None;
    }

    let raw = buf[1];
    let charging = (raw & 0x80) != 0;
    let step = raw & 0x7F;

    let percent = step.saturating_sub(1).saturating_mul(5).min(100);

    let state = if charging {
        ChargeState::Charging
    } else {
        ChargeState::Discharging
    };

    Some(BatteryReading::new(percent, state))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // SteelSeriesSource construction

    #[test]
    fn new_source_has_no_open_handle() {
        let source = SteelSeriesSource {
            info: DeviceInfo {
                name: "test".to_owned(),
                kind: DeviceKind::Mouse,
                transport: crate::domain::Transport::Hidraw,
                locator: Some("hidraw0".to_owned()),
            },
            dev_path: PathBuf::from("/dev/hidraw0"),
            identity: hidraw::NodeIdentity {
                vendor: FAMILY.vendor,
                product: 0x1852,
                locator: "hidraw0".to_owned(),
            },
            handle: None,
        };
        assert!(source.handle.is_none());
    }

    // clear_handle_on_error

    #[test]
    fn clear_handle_on_error_keeps_handle_on_ok() {
        let mut handle = Some(42);
        clear_handle_on_error(&mut handle, &Ok(()));
        assert_eq!(handle, Some(42));
    }

    #[test]
    fn clear_handle_on_error_clears_handle_on_err() {
        let mut handle = Some(42);
        clear_handle_on_error(&mut handle, &Err::<(), _>(anyhow::anyhow!("boom")));
        assert_eq!(handle, None);
    }

    // parse_battery_response

    #[test]
    fn classify_response_reports_a_sleeping_device_instead_of_draining() {
        // Observed from an Aerox 5 Wireless dongle with the mouse switched off.
        let mut buf = [0u8; 64];
        buf[0] = WIRELESS_FLAG;
        buf[1] = LEVEL_UNAVAILABLE;
        assert_eq!(classify_response(&buf), Response::DeviceUnreachable);
    }

    #[test]
    fn classify_response_reads_an_awake_device() {
        // Observed from the same dongle at 90%: step 19 -> (19 - 1) * 5.
        let buf = [BATTERY_QUERY, 0x13];
        assert_eq!(
            classify_response(&buf),
            Response::Reading(BatteryReading::new(90, ChargeState::Discharging))
        );
    }

    #[test]
    fn classify_response_keeps_draining_on_an_unrelated_report() {
        assert_eq!(classify_response(&[0x01, 0x02, 0x03]), Response::Unrelated);
        assert_eq!(classify_response(&[]), Response::Unrelated);
    }

    #[test]
    fn classify_response_does_not_mistake_a_real_reading_for_unreachable() {
        // 0xFF in the level byte only means "no data" behind the bare wireless
        // flag; behind the command echo it would be a (nonsensical) reading, and
        // must not short-circuit the drain loop on the wrong byte.
        let buf = [BATTERY_QUERY, LEVEL_UNAVAILABLE];
        assert_ne!(classify_response(&buf), Response::DeviceUnreachable);
    }

    #[test]
    fn battery_query_report_carries_the_full_output_report() {
        let request = battery_query_report();
        assert_eq!(request.len(), 65, "1 report-ID byte + a 64-byte payload");
        assert_eq!(request[0], 0x00, "report ID for a collection with no IDs");
        assert_eq!(request[1], BATTERY_QUERY);
        assert!(
            request[2..].iter().all(|&b| b == 0),
            "the rest of the report must be zero padding"
        );
    }

    #[test]
    fn parse_battery_response_step1_is_0_percent() {
        // step=1 → (1-1)*5 = 0%
        let buf = [0xD2, 0x01, 0x00];
        let r = parse_battery_response(&buf).unwrap();
        assert_eq!(r.percent, 0);
        assert_eq!(r.state, ChargeState::Discharging);
    }

    #[test]
    fn parse_battery_response_step21_is_100_percent() {
        // step=21 → (21-1)*5 = 100%
        let buf = [0xD2, 21, 0x00];
        let r = parse_battery_response(&buf).unwrap();
        assert_eq!(r.percent, 100);
        assert_eq!(r.state, ChargeState::Discharging);
    }

    #[test]
    fn parse_battery_response_charging_bit() {
        // step=10 → 45%, charging bit set
        let buf = [0xD2, 0x80 | 10, 0x00];
        let r = parse_battery_response(&buf).unwrap();
        assert_eq!(r.percent, 45);
        assert_eq!(r.state, ChargeState::Charging);
    }

    #[test]
    fn parse_battery_response_wrong_report_id_returns_none() {
        let buf = [0x00, 0x15, 0x00];
        assert!(parse_battery_response(&buf).is_none());
    }

    #[test]
    fn parse_battery_response_empty_returns_none() {
        assert!(parse_battery_response(&[]).is_none());
    }

    #[test]
    fn parse_battery_response_clamps_over_100() {
        // step=255 & 0x7F = 127 → (127-1)*5 = 630 → clamp 100
        let buf = [0xD2, 0xFF, 0x00];
        let r = parse_battery_response(&buf).unwrap();
        assert_eq!(r.percent, 100);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn parse_battery_response_accepts_only_the_echo_and_stays_in_range(
                buf in prop::collection::vec(any::<u8>(), 0..70),
            ) {
                let parsed = parse_battery_response(&buf);
                prop_assert_eq!(parsed.is_some(), buf.len() >= 2 && buf[0] == BATTERY_QUERY);
                if let Some(r) = parsed {
                    prop_assert!(r.percent <= 100);
                    let charging = buf[1] & 0x80 != 0;
                    prop_assert_eq!(r.state == ChargeState::Charging, charging);
                }
            }

            #[test]
            fn parse_battery_response_round_trips_every_step(
                step in 1u8..=21,
                charging in any::<bool>(),
                tail in prop::collection::vec(any::<u8>(), 0..62),
            ) {
                let mut buf = vec![BATTERY_QUERY, step | if charging { 0x80 } else { 0 }];
                buf.extend(tail);
                let state = if charging { ChargeState::Charging } else { ChargeState::Discharging };
                prop_assert_eq!(
                    parse_battery_response(&buf),
                    Some(BatteryReading::new((step - 1) * 5, state))
                );
            }

            #[test]
            fn classify_response_agrees_with_the_parser(
                buf in prop::collection::vec(any::<u8>(), 0..70),
            ) {
                let expected = if buf.starts_with(&[WIRELESS_FLAG, LEVEL_UNAVAILABLE]) {
                    Response::DeviceUnreachable
                } else {
                    parse_battery_response(&buf).map_or(Response::Unrelated, Response::Reading)
                };
                prop_assert_eq!(classify_response(&buf), expected);
            }
        }
    }
}
