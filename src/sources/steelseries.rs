//! SteelSeries HID backend — заряд по выделенному config-интерфейсу.
//!
//! Протокол: vendor 0x1038, USB-интерфейс 3.
//! Write output report `[0x00, 0xD2]`, read ответ:
//! `resp[0] == 0xD2`, `resp[1]` бит7 = charging, биты0-6 = шаг (5% каждый),
//! `percent = (step - 1) * 5`, клампить 0..=100.
//!
//! Обнаружение: `/sys/class/hidraw/hidrawN/device/uevent` содержит
//! `HID_ID=0003:VVVVVVVV:PPPPPPPP`; canonicalize device → сегмент `:1.N` → интерфейс N.
//!
//! Реверс-референс: исходный Python-драйвер universal-battery-tray.

use std::{
    io::{Read as _, Write as _},
    os::unix::fs::OpenOptionsExt as _,
    path::PathBuf,
};

use anyhow::Context as _;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

use super::{BatteryBackend, BatterySource};

// ── Константы протокола ──────────────────────────────────────────────────────

const VENDOR_ID: u16 = 0x1038;
const BATTERY_INTERFACE: u8 = 3;
const BATTERY_QUERY: u8 = 0xD2;

/// Таймаут ожидания ответа от устройства (миллисекунды).
const POLL_TIMEOUT_MS: u16 = 1000;

// ── Таблица устройств ────────────────────────────────────────────────────────

/// Описание поддержанного устройства. Новая модель = +1 строка в `DEVICES`.
struct SteelSeriesDevice {
    product_id: u16,
    name: &'static str,
    kind: DeviceKind,
}

const DEVICES: &[SteelSeriesDevice] = &[SteelSeriesDevice {
    product_id: 0x1852,
    name: "SteelSeries Aerox 5 Wireless",
    kind: DeviceKind::Mouse,
}];

// ── Backend ──────────────────────────────────────────────────────────────────

pub struct SteelSeriesBackend;

pub struct SteelSeriesSource {
    info: DeviceInfo,
    dev_path: PathBuf, // /dev/hidrawN
}

#[async_trait::async_trait]
impl BatteryBackend for SteelSeriesBackend {
    fn name(&self) -> &'static str {
        "steelseries"
    }

    async fn discover(&self) -> Vec<Box<dyn BatterySource>> {
        discover_inner().unwrap_or_else(|e| {
            eprintln!("rigbat steelseries: {e:#}");
            Vec::new()
        })
    }
}

fn discover_inner() -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
    let hidraw_root = std::path::Path::new("/sys/class/hidraw");

    if !hidraw_root.exists() {
        return Ok(Vec::new());
    }

    let mut sources: Vec<Box<dyn BatterySource>> = Vec::new();

    let entries = std::fs::read_dir(hidraw_root).context("reading /sys/class/hidraw")?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("rigbat steelseries: skipping hidraw entry: {e}");
                continue;
            }
        };

        // try_node возвращает Err для несовпадающих узлов — это штатно, молча пропускаем.
        let _ = try_node(&entry.file_name().to_string_lossy(), &mut sources);
    }

    Ok(sources)
}

/// Пробует добавить узел hidrawN в список источников.
/// Возвращает Err, если узел не подходит или возникла ошибка — вызывающий пропускает.
fn try_node(node_name: &str, sources: &mut Vec<Box<dyn BatterySource>>) -> anyhow::Result<()> {
    let uevent_path = format!("/sys/class/hidraw/{node_name}/device/uevent");
    let uevent =
        std::fs::read_to_string(&uevent_path).with_context(|| format!("reading {uevent_path}"))?;

    let hid_id_line = uevent
        .lines()
        .find(|l| l.starts_with("HID_ID="))
        .with_context(|| format!("HID_ID not found in {uevent_path}"))?;

    let hid_id_value = hid_id_line
        .strip_prefix("HID_ID=")
        .context("stripping HID_ID= prefix")?;

    let (vendor, product) =
        parse_hid_id(hid_id_value).with_context(|| format!("parsing HID_ID={hid_id_value}"))?;

    if vendor != VENDOR_ID {
        anyhow::bail!("vendor 0x{vendor:04X} != 0x{VENDOR_ID:04X}");
    }

    let device_desc = DEVICES
        .iter()
        .find(|d| d.product_id == product)
        .with_context(|| format!("product 0x{product:04X} not in device table"))?;

    let device_sys_path = format!("/sys/class/hidraw/{node_name}/device");
    let real = std::fs::canonicalize(&device_sys_path)
        .with_context(|| format!("canonicalizing {device_sys_path}"))?;

    let real_str = real.to_string_lossy();
    let iface = parse_usb_interface(&real_str)
        .with_context(|| format!("parsing USB interface from {real_str}"))?;

    if iface != BATTERY_INTERFACE {
        anyhow::bail!("interface {iface} != {BATTERY_INTERFACE} (battery interface)");
    }

    let dev_path = PathBuf::from(format!("/dev/{node_name}"));

    sources.push(Box::new(SteelSeriesSource {
        info: DeviceInfo {
            name: device_desc.name.to_owned(),
            kind: device_desc.kind,
        },
        dev_path,
    }));

    Ok(())
}

// ── Source ───────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl BatterySource for SteelSeriesSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        let path = self.dev_path.clone();
        // Блокирующий I/O выносим из async-контекста.
        tokio::task::spawn_blocking(move || poll_device(&path))
            .await
            .context("spawn_blocking")?
    }
}

/// Синхронный опрос устройства через /dev/hidrawN.
fn poll_device(dev_path: &std::path::Path) -> anyhow::Result<BatteryReading> {
    use std::os::fd::AsFd as _;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dev_path)
        .with_context(|| format!("opening {}", dev_path.display()))?;

    // Отправить запрос батареи.
    file.write_all(&[0x00, BATTERY_QUERY])
        .context("writing battery query")?;

    // Дренировать буфер до нужного ответа с общим таймаутом.
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

        if let Some(reading) = parse_battery_response(&buf[..n]) {
            return Ok(reading);
        }
        // Ответ не тот — продолжаем дренировать.
    }
}

// ── Чистые функции ───────────────────────────────────────────────────────────

/// Парсит `HID_ID=bus:vendor:product` → `(vendor, product)`.
///
/// Пример: `"0003:00001038:00001852"` → `(0x1038, 0x1852)`.
pub fn parse_hid_id(s: &str) -> Option<(u16, u16)> {
    let mut parts = s.splitn(3, ':');
    let _bus = parts.next()?;
    let vendor_str = parts.next()?;
    let product_str = parts.next()?;

    let vendor = u32::from_str_radix(vendor_str.trim(), 16).ok()? as u16;
    let product = u32::from_str_radix(product_str.trim(), 16).ok()? as u16;

    Some((vendor, product))
}

/// Извлекает номер USB-интерфейса из реального пути sysfs.
///
/// Ищет последний сегмент вида `:1.N` и возвращает N.
/// Пример: `"/sys/devices/…/7-1.1:1.3/…"` → `Some(3)`.
pub fn parse_usb_interface(real_path: &str) -> Option<u8> {
    real_path.split('/').rev().find_map(|seg| {
        // Сегмент вида "7-1.1:1.3" — ищем часть после последнего ':'
        let after_colon = seg.rsplit(':').next()?;
        // after_colon должен быть "1.N"
        let n_str = after_colon.strip_prefix("1.")?;
        n_str.parse::<u8>().ok()
    })
}

/// Парсит ответ HID: `buf[0] == 0xD2`, `buf[1]` бит7 = charging, биты0-6 = шаг.
///
/// `percent = (step - 1) * 5`, клампить 0..=100.
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

// ── Тесты ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // parse_hid_id

    #[test]
    fn parse_hid_id_valid() {
        assert_eq!(
            parse_hid_id("0003:00001038:00001852"),
            Some((0x1038, 0x1852))
        );
    }

    #[test]
    fn parse_hid_id_garbage_returns_none() {
        assert_eq!(parse_hid_id("not-a-hid-id"), None);
    }

    #[test]
    fn parse_hid_id_too_few_parts_returns_none() {
        assert_eq!(parse_hid_id("0003:00001038"), None);
    }

    #[test]
    fn parse_hid_id_invalid_hex_returns_none() {
        assert_eq!(parse_hid_id("0003:ZZZZZZZZ:00001852"), None);
    }

    // parse_usb_interface

    #[test]
    fn parse_usb_interface_extracts_interface_3() {
        assert_eq!(
            parse_usb_interface(
                "/sys/devices/pci0000:00/0000:00:14.0/usb7/7-1/7-1.1/7-1.1:1.3/0003:1038:1852.0018/hidraw/hidraw0"
            ),
            Some(3)
        );
    }

    #[test]
    fn parse_usb_interface_extracts_interface_0() {
        assert_eq!(
            parse_usb_interface(
                "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/hidraw/hidraw1"
            ),
            Some(0)
        );
    }

    #[test]
    fn parse_usb_interface_no_segment_returns_none() {
        assert_eq!(parse_usb_interface("/sys/devices/platform/hidraw2"), None);
    }

    // parse_battery_response

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
}
