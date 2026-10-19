//! SteelSeries HID backend — reads charge via a dedicated config interface.
//!
//! Protocol: vendor 0x1038, USB interface 3. Write a full 64-byte output
//! report prefixed by report ID `0x00` and starting with `0xD2` (`0x92`
//! battery query | `0x40` wireless flag), read the echo (see
//! [`parse_battery_response`]). A dongle whose device is asleep or off
//! answers `40 ff` instead — the wireless flag with no data.
//!
//! Discovery: [`FAMILY`] through `hidraw::discover`, filtered to interface 3.

use std::fs::File;

use crate::domain::{BatteryReading, ChargeState, DeviceKind};

use super::{
    BatteryBackend, BatterySource, Context,
    hidraw::{self, HandleSource, ReadBattery, Verdict},
};

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

// ── Device table ─────────────────────────────────────────────────────────────

/// The protocol constants above are family-wide: a model with another wire
/// protocol needs more than a row here.
pub static FAMILY: hidraw::HidrawFamily = hidraw::HidrawFamily {
    models: &[hidraw::HidrawModel {
        vendor: 0x1038,
        product: 0x1852,
        name: "SteelSeries Aerox 5 Wireless",
        kind: DeviceKind::Mouse,
    }],
    interface: Some(3),
};

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct SteelSeriesBackend;

#[async_trait::async_trait]
impl BatteryBackend for SteelSeriesBackend {
    fn name(&self) -> &'static str {
        "steelseries"
    }

    fn hidraw_family(&self) -> Option<&'static hidraw::HidrawFamily> {
        Some(&FAMILY)
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        Ok(hidraw::discover(&FAMILY)
            .await?
            .into_iter()
            .map(|device| HandleSource::boxed(device, SteelSeriesMouse))
            .collect())
    }
}

#[derive(Clone)]
struct SteelSeriesMouse;

impl ReadBattery for SteelSeriesMouse {
    type Handle = File;

    fn read(&self, file: &mut File) -> anyhow::Result<BatteryReading> {
        hidraw::exchange(file, &battery_query_report(), |report| {
            classify_response(report).into()
        })
    }
}

// ── Pure functions ────────────────────────────────────────────────────────────

/// Builds the battery query write: the report-ID byte `0x00` (the config
/// interface's collection declares no report IDs) followed by the full 64-byte
/// output report the descriptor declares (`Output (usage 0xF1), Report Size 8,
/// Report Count 0x40`). A short write is STALLed with `EPIPE`.
fn battery_query_report() -> [u8; 1 + OUTPUT_REPORT_LEN] {
    let mut request = [0u8; 1 + OUTPUT_REPORT_LEN];
    request[1] = BATTERY_QUERY;
    request
}

/// What a report read from the config interface turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum Response {
    Reading(BatteryReading),
    /// The dongle answered, but the device behind it has no data to give.
    /// Distinguished from `Unrelated` so the exchange stops instead of
    /// waiting out its timeout: a sleeping mouse is the common case.
    DeviceUnreachable,
    /// Some other report on the same interface. Keep reading.
    Unrelated,
}

impl From<Response> for Verdict<BatteryReading> {
    fn from(response: Response) -> Self {
        match response {
            Response::Reading(reading) => Self::Answer(reading),
            Response::DeviceUnreachable => Self::Reject(anyhow::anyhow!(
                "device is asleep or off (dongle reported no battery data)"
            )),
            Response::Unrelated => Self::Unrelated,
        }
    }
}

fn classify_response(buf: &[u8]) -> Response {
    if buf.len() >= 2 && buf[0] == WIRELESS_FLAG && buf[1] == LEVEL_UNAVAILABLE {
        return Response::DeviceUnreachable;
    }
    match parse_battery_response(buf) {
        Some(reading) => Response::Reading(reading),
        None => Response::Unrelated,
    }
}

/// Parses the echo: `buf[0] == 0xD2`, `buf[1]` bit 7 = charging, bits 0-6 = step.
///
/// `percent = (step - 1) * 5`, clamped to 0..=100.
fn parse_battery_response(buf: &[u8]) -> Option<BatteryReading> {
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
