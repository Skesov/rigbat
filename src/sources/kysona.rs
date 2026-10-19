//! Kysona-protocol mice over `/dev/hidraw`: Kysona M600, VXE Dragonfly R1 Pro,
//! ATK VXE R1 SE+, Pulsar.
//!
//! Request/response on USB interface 1: write a 17-byte output report, read
//! input reports until the answer. The source holds its handle for its lifetime
//! and discards queued input before each request. Ids, requests, reply layout
//! and the source of each fact: `docs/kysona.md`.
//!
//! A node whose HID device already carries a kernel `power_supply` is skipped:
//! the sysfs backend reports that mouse.

use std::fs::File;

use crate::domain::{BatteryReading, ChargeState, DeviceKind};

use super::{
    BatteryBackend, BatterySource, Context,
    hidraw::{self, HandleSource, HidrawFamily, HidrawModel, ReadBattery, Verdict},
};

const MOUSE_OFF: &str = "mouse is asleep or off (dongle reported it offline)";

// ── Device table ─────────────────────────────────────────────────────────────

const KYSONA: u16 = 0x3554;
const ATK_DONGLE: u16 = 0x373B;

const fn mouse(vendor: u16, product: u16, name: &'static str) -> HidrawModel {
    HidrawModel {
        vendor,
        product,
        name,
        kind: DeviceKind::Mouse,
    }
}

/// Cable and dongle of one mouse share a name; the locator tells them apart.
pub static FAMILY: HidrawFamily = HidrawFamily {
    models: &[
        mouse(KYSONA, 0xF507, "Pulsar Mouse"),
        mouse(KYSONA, 0xF508, "Pulsar Mouse"),
        mouse(KYSONA, 0xF509, "Pulsar Mouse"),
        mouse(KYSONA, 0xF57C, "Kysona M600"),
        mouse(KYSONA, 0xF57D, "Kysona M600"),
        mouse(KYSONA, 0xF58A, "VXE Dragonfly R1 Pro"),
        mouse(KYSONA, 0xF58C, "VXE Dragonfly R1 Pro"),
        mouse(KYSONA, 0xF58F, "ATK VXE R1 SE+"),
        mouse(ATK_DONGLE, 0x1085, "ATK VXE R1 SE+"),
    ],
    interface: Some(1),
};

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct KysonaBackend;

#[async_trait::async_trait]
impl BatteryBackend for KysonaBackend {
    fn name(&self) -> &'static str {
        "kysona"
    }

    fn hidraw_family(&self) -> Option<&'static HidrawFamily> {
        Some(&FAMILY)
    }

    async fn discover(&self, _ctx: &Context) -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
        Ok(hidraw::discover_unclaimed(&FAMILY)
            .await?
            .into_iter()
            .map(|device| HandleSource::boxed(device, KysonaMouse))
            .collect())
    }
}

#[derive(Clone)]
struct KysonaMouse;

impl ReadBattery for KysonaMouse {
    type Handle = File;

    fn read(&self, file: &mut File) -> anyhow::Result<BatteryReading> {
        hidraw::exchange(file, &request(Command::Online), |report| {
            classify_online(report).into()
        })?;
        hidraw::exchange(file, &request(Command::Battery), |report| {
            classify_battery(report).into()
        })
    }
}

// ── Protocol (pure) ──────────────────────────────────────────────────────────

const REPORT_ID: u8 = 0x08;
const REPORT_LEN: usize = 17;
const CHECKSUM_BASE: u8 = 0x55;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Online,
    Battery,
}

impl Command {
    const fn byte(self) -> u8 {
        match self {
            Self::Online => 0x03,
            Self::Battery => 0x04,
        }
    }
}

/// What one input report means to the exchange waiting on it.
#[derive(Debug, PartialEq, Eq)]
enum Reply<T> {
    Answer(T),
    /// The dongle answered, the mouse behind it is not connected.
    MouseOff,
    /// The answer to this request, but its bytes cannot be a reading.
    Invalid(&'static str),
    /// Another report on the same interface. Keep reading.
    Unrelated,
}

impl<T> Reply<T> {
    fn and_then<U>(self, f: impl FnOnce(T) -> Reply<U>) -> Reply<U> {
        match self {
            Self::Answer(answer) => f(answer),
            Self::MouseOff => Reply::MouseOff,
            Self::Invalid(reason) => Reply::Invalid(reason),
            Self::Unrelated => Reply::Unrelated,
        }
    }
}

impl<T> From<Reply<T>> for Verdict<T> {
    fn from(reply: Reply<T>) -> Self {
        match reply {
            Reply::Answer(answer) => Self::Answer(answer),
            Reply::MouseOff => Self::Reject(anyhow::anyhow!(MOUSE_OFF)),
            Reply::Invalid(reason) => Self::Reject(anyhow::anyhow!("invalid reply: {reason}")),
            Reply::Unrelated => Self::Unrelated,
        }
    }
}

/// `0x55` minus the sum of every byte but the last, modulo 256.
fn checksum(report: &[u8; REPORT_LEN]) -> u8 {
    report
        .iter()
        .take(REPORT_LEN - 1)
        .fold(CHECKSUM_BASE, |sum, &byte| sum.wrapping_sub(byte))
}

fn request(command: Command) -> [u8; REPORT_LEN] {
    let mut report = [0u8; REPORT_LEN];
    report[0] = REPORT_ID;
    report[1] = command.byte();
    report[REPORT_LEN - 1] = checksum(&report);
    report
}

/// The reply to `command` with its checksum checked.
fn checked_reply(report: &[u8], command: Command) -> Reply<[u8; REPORT_LEN]> {
    let Ok(report) = <[u8; REPORT_LEN]>::try_from(report) else {
        return Reply::Unrelated;
    };
    if report[0] != REPORT_ID || report[1] != command.byte() {
        return Reply::Unrelated;
    }
    if report[REPORT_LEN - 1] != checksum(&report) {
        return Reply::Invalid("checksum mismatch");
    }
    Reply::Answer(report)
}

fn classify_online(report: &[u8]) -> Reply<()> {
    checked_reply(report, Command::Online).and_then(|report| match report[6] {
        1 => Reply::Answer(()),
        0 => Reply::MouseOff,
        _ => Reply::Invalid("online flag out of range"),
    })
}

fn classify_battery(report: &[u8]) -> Reply<BatteryReading> {
    checked_reply(report, Command::Battery).and_then(|report| {
        let state = match report[7] {
            0 => ChargeState::Discharging,
            1 => ChargeState::Charging,
            _ => return Reply::Invalid("charging flag out of range"),
        };
        match report[6] {
            percent @ 0..=100 => Reply::Answer(BatteryReading::new(percent, state)),
            _ => Reply::Invalid("percent out of range"),
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply to `command` with `payload` from byte 6 on and a valid checksum.
    fn reply(command: Command, payload: &[u8]) -> [u8; REPORT_LEN] {
        let mut report = [0u8; REPORT_LEN];
        report[0] = REPORT_ID;
        report[1] = command.byte();
        report[6..6 + payload.len()].copy_from_slice(payload);
        report[REPORT_LEN - 1] = checksum(&report);
        report
    }

    // Requests

    #[test]
    fn requests_match_the_kernel_constants() {
        let mut battery = [0u8; REPORT_LEN];
        battery[..2].copy_from_slice(&[0x08, 0x04]);
        battery[16] = 0x49;
        assert_eq!(request(Command::Battery), battery);

        let mut online = [0u8; REPORT_LEN];
        online[..2].copy_from_slice(&[0x08, 0x03]);
        online[16] = 0x4A;
        assert_eq!(request(Command::Online), online);
    }

    // Replies

    #[test]
    fn battery_reads_percent_and_charging() {
        assert_eq!(
            classify_battery(&reply(Command::Battery, &[73, 0, 0x0F, 0xA0])),
            Reply::Answer(BatteryReading::new(73, ChargeState::Discharging))
        );
        assert_eq!(
            classify_battery(&reply(Command::Battery, &[40, 1])),
            Reply::Answer(BatteryReading::new(40, ChargeState::Charging))
        );
    }

    #[test]
    fn a_checksum_mismatch_is_not_a_reading() {
        let mut report = reply(Command::Battery, &[73, 0]);
        report[16] ^= 0x01;
        assert_eq!(
            classify_battery(&report),
            Reply::Invalid("checksum mismatch")
        );
        let mut report = reply(Command::Online, &[1]);
        report[6] = 0;
        assert_eq!(
            classify_online(&report),
            Reply::Invalid("checksum mismatch")
        );
    }

    #[test]
    fn battery_rejects_out_of_range_bytes() {
        assert!(matches!(
            classify_battery(&reply(Command::Battery, &[101, 0])),
            Reply::Invalid(_)
        ));
        assert!(matches!(
            classify_battery(&reply(Command::Battery, &[50, 2])),
            Reply::Invalid(_)
        ));
    }

    #[test]
    fn online_flag_means_connected_or_off() {
        assert_eq!(
            classify_online(&reply(Command::Online, &[1])),
            Reply::Answer(())
        );
        assert_eq!(
            classify_online(&reply(Command::Online, &[0])),
            Reply::MouseOff
        );
        assert!(matches!(
            classify_online(&reply(Command::Online, &[2])),
            Reply::Invalid(_)
        ));
    }

    #[test]
    fn other_reports_are_unrelated() {
        let mut event = reply(Command::Battery, &[0x40]);
        event[1] = 0x0A;
        assert_eq!(classify_battery(&event), Reply::Unrelated);
        assert_eq!(
            classify_battery(&reply(Command::Online, &[1])),
            Reply::Unrelated
        );
        assert_eq!(
            classify_online(&reply(Command::Battery, &[50, 0])),
            Reply::Unrelated
        );
        let short = reply(Command::Battery, &[50, 0]);
        assert_eq!(classify_battery(&short[..16]), Reply::Unrelated);
        let mut other_id = reply(Command::Battery, &[50, 0]);
        other_id[0] = 0x07;
        assert_eq!(classify_battery(&other_id), Reply::Unrelated);
    }

    // Device table

    #[test]
    fn every_model_is_a_mouse_on_interface_1() {
        assert_eq!(FAMILY.interface, Some(1));
        for model in FAMILY.models {
            assert_eq!(model.kind, DeviceKind::Mouse, "{}", model.name);
        }
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn checksum_round_trips(
                body in any::<[u8; REPORT_LEN - 1]>(),
                wrong in 1u8..=u8::MAX,
            ) {
                let mut report = [0u8; REPORT_LEN];
                report[..REPORT_LEN - 1].copy_from_slice(&body);
                report[0] = REPORT_ID;
                report[1] = Command::Battery.byte();
                report[REPORT_LEN - 1] = checksum(&report);
                prop_assert_eq!(checked_reply(&report, Command::Battery), Reply::Answer(report));
                report[REPORT_LEN - 1] = report[REPORT_LEN - 1].wrapping_add(wrong);
                prop_assert_eq!(
                    checked_reply(&report, Command::Battery),
                    Reply::Invalid("checksum mismatch")
                );
            }

            #[test]
            fn parsers_never_panic_on_a_full_report(report in any::<[u8; REPORT_LEN]>()) {
                if let Reply::Answer(reading) = classify_battery(&report) {
                    prop_assert!(reading.percent <= 100);
                }
                let _ = classify_online(&report);
            }

            #[test]
            fn parsers_never_panic_on_any_length(report in prop::collection::vec(any::<u8>(), 0..70)) {
                let _ = classify_battery(&report);
                let _ = classify_online(&report);
            }
        }
    }
}
