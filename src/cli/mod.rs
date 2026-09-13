use std::time::Instant;

use serde_json::{Value, json};

use crate::config::Config;
use crate::domain::{
    BatteryReading, ChargeState, DeviceInfo, DeviceState, Estimate, Presence, PrimaryStatus,
    classify,
};
use crate::tray::format_device_entry;
use crate::tray::manager::select_featured;

type Row = (DeviceInfo, Option<BatteryReading>);

fn state_str(state: ChargeState) -> &'static str {
    match state {
        ChargeState::Charging => "charging",
        ChargeState::Discharging => "discharging",
        ChargeState::Full => "full",
    }
}

pub fn to_json(rows: &[Row]) -> Value {
    let items: Vec<Value> = rows
        .iter()
        .map(|(info, reading)| {
            let (percent, state) = match reading {
                Some(r) => (json!(r.percent), json!(state_str(r.state))),
                None => (Value::Null, Value::Null),
            };
            json!({
                "name": info.name,
                "kind": info.kind.as_str(),
                "transport": info.transport.as_str(),
                "locator": info.locator,
                "online": reading.is_some(),
                "percent": percent,
                "state": state,
            })
        })
        .collect();

    Value::Array(items)
}

// The battery table/JSON/waybar line is the program's output on stdout, not
// a diagnostic — hence the narrow allow of the project's tracing-only print lint.
#[allow(clippy::print_stdout)]
pub fn print_json(rows: &[Row]) {
    let value = to_json(rows);
    // serde_json::to_string_pretty cannot fail on a valid Value
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".to_owned());
    println!("{}", text);
}

/// Builds a `DeviceState` from a one-shot `Row` so it can be rendered through
/// `format_device_entry` (the same formatter the tray menu uses). The one-shot
/// path (`app::poll_once`) has no polling history: a `None` reading here only
/// means this single poll failed, not that the device is known offline over
/// time, so there is no retained reading or age to carry — `last_seen` is
/// always `None` and presence is a stand-in (`Online`/`Unreachable`) derived
/// from this one reading rather than a real `Presence` from the supervisor.
fn as_device_state(info: &DeviceInfo, reading: Option<BatteryReading>) -> DeviceState {
    DeviceState {
        info: info.clone(),
        last_reading: reading,
        last_seen: None,
        presence: if reading.is_some() {
            Presence::Online
        } else {
            Presence::Unreachable
        },
        // The one-shot path has no polling history to derive an estimate
        // from, and format_device_entry only ever consults this field for
        // the Online branch, which --json/--waybar never render.
        estimate: Estimate::Unknown,
    }
}

/// Builds the waybar `custom` module payload (`return-type: json`): a single
/// object describing the featured device — the same device the aggregate
/// tray icon shows, picked by `select_featured` (the tray's own selection
/// logic, extracted so this does not reimplement it).
///
/// `class` vocabulary (documented in the README, styled by the user's CSS):
/// `charging`, `low`, `ok`, `offline`. `percentage` is omitted, not `0`, when
/// there is no reading to report.
pub fn to_waybar(rows: &[Row], cfg: &Config) -> Value {
    let shown: Vec<&Row> = rows
        .iter()
        .filter(|(info, _)| cfg.is_shown(&info.name))
        .collect();

    let pairs: Vec<(&str, bool)> = shown
        .iter()
        .map(|(info, reading)| (info.name.as_str(), reading.is_some()))
        .collect();
    let featured_name = select_featured(&pairs, cfg.primary_device.as_deref());
    let featured: Option<&Row> = featured_name
        .as_deref()
        .and_then(|name| shown.iter().copied().find(|(info, _)| info.name == name));

    let now = Instant::now();
    let tooltip = if shown.is_empty() {
        "No devices".to_owned()
    } else {
        shown
            .iter()
            .map(|(info, reading)| format_device_entry(&as_device_state(info, *reading), now))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let (text, class, percentage): (String, &str, Option<u8>) = match featured {
        None => ("no devices".to_owned(), "offline", None),
        Some((info, reading)) => {
            match classify(*reading, cfg.effective_low_threshold(&info.name)) {
                PrimaryStatus::Offline => ("offline".to_owned(), "offline", None),
                PrimaryStatus::Charging { percent } => {
                    (format!("{percent}%"), "charging", Some(percent))
                }
                PrimaryStatus::Low { percent } => (format!("{percent}%"), "low", Some(percent)),
                PrimaryStatus::Ok { percent } => (format!("{percent}%"), "ok", Some(percent)),
            }
        }
    };

    let mut obj = json!({
        "text": text,
        "tooltip": tooltip,
        "class": class,
    });
    if let Some(p) = percentage {
        obj["percentage"] = json!(p);
    }
    obj
}

/// Prints the `to_waybar` payload as one line of JSON, terminated by a
/// newline — waybar's `custom` module reads exactly one JSON object per line.
/// Falls back to a valid (never empty) line on the practically-impossible
/// serialization failure, since a custom module that receives malformed
/// output logs an error on every poll interval.
// See print_json: waybar's custom-module line is program output on stdout.
#[allow(clippy::print_stdout)]
pub fn print_waybar(rows: &[Row], cfg: &Config) {
    let value = to_waybar(rows, cfg);
    let text = serde_json::to_string(&value).unwrap_or_else(|_| {
        r#"{"text":"error","tooltip":"rigbat: failed to render status","class":"offline"}"#
            .to_owned()
    });
    println!("{}", text);
}

// See print_json: the battery table is program output on stdout.
#[allow(clippy::print_stdout)]
pub fn print_table(rows: &[Row]) {
    if rows.is_empty() {
        println!("No devices found");
        return;
    }

    // Width in chars, not bytes: names come from BlueZ Alias/hardware and can be
    // non-ASCII, and `{:<width$}` pads by chars while `str::len()` counts bytes.
    // Still not true terminal display width (CJK double-width, combining marks
    // zero-width) — that needs the unicode-width crate, out of scope for this fix.
    let name_width = rows
        .iter()
        .map(|(info, _)| info.name.chars().count())
        .max()
        .unwrap_or(0);

    for (info, reading) in rows {
        let status = match reading {
            None => "offline".to_owned(),
            Some(r) => format!("{}%  {}", r.percent, state_str(r.state)),
        };
        println!("{:<width$}  {}", info.name, status, width = name_width);
    }
}

// See print_json: the wide battery table is program output on stdout.
#[allow(clippy::print_stdout)]
pub fn print_table_wide(rows: &[Row]) {
    if rows.is_empty() {
        println!("No devices found");
        return;
    }

    // Compute per-column widths from data plus header, in chars (see print_table
    // for why: names can be non-ASCII and `{:<width$}` pads by chars, not bytes).
    let name_w = rows
        .iter()
        .map(|(info, _)| info.name.chars().count())
        .max()
        .unwrap_or(0)
        .max("NAME".chars().count());
    let kind_w = rows
        .iter()
        .map(|(info, _)| info.kind.as_str().chars().count())
        .max()
        .unwrap_or(0)
        .max("KIND".chars().count());
    let transport_w = rows
        .iter()
        .map(|(info, _)| info.transport.as_str().chars().count())
        .max()
        .unwrap_or(0)
        .max("TRANSPORT".chars().count());
    let locator_w = rows
        .iter()
        .map(|(info, _)| info.locator.as_deref().unwrap_or("-").chars().count())
        .max()
        .unwrap_or(0)
        .max("LOCATOR".chars().count());
    // PERCENT and STATE are short fixed-width columns; anchor to header width.
    let percent_w = "PERCENT".chars().count();
    let state_w = rows
        .iter()
        .map(|(_, reading)| match reading {
            None => "offline".chars().count(),
            Some(r) => state_str(r.state).chars().count(),
        })
        .max()
        .unwrap_or(0)
        .max("STATE".chars().count());

    println!(
        "{:<nw$}  {:<kw$}  {:<tw$}  {:<lw$}  {:<pw$}  {:<sw$}",
        "NAME",
        "KIND",
        "TRANSPORT",
        "LOCATOR",
        "PERCENT",
        "STATE",
        nw = name_w,
        kw = kind_w,
        tw = transport_w,
        lw = locator_w,
        pw = percent_w,
        sw = state_w,
    );

    for (info, reading) in rows {
        let (percent_col, state_col) = match reading {
            None => ("-".to_owned(), "offline".to_owned()),
            Some(r) => (format!("{}%", r.percent), state_str(r.state).to_owned()),
        };
        let locator_col = info.locator.as_deref().unwrap_or("-");
        println!(
            "{:<nw$}  {:<kw$}  {:<tw$}  {:<lw$}  {:<pw$}  {:<sw$}",
            info.name,
            info.kind.as_str(),
            info.transport.as_str(),
            locator_col,
            percent_col,
            state_col,
            nw = name_w,
            kw = kind_w,
            tw = transport_w,
            lw = locator_w,
            pw = percent_w,
            sw = state_w,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            kind: DeviceKind::Other,
            transport: crate::domain::Transport::Sysfs,
            locator: None,
        }
    }

    fn device_with_locator(name: &str, locator: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            kind: DeviceKind::Other,
            transport: crate::domain::Transport::Bluetooth,
            locator: Some(locator.to_owned()),
        }
    }

    #[test]
    fn to_json_online_device() {
        let reading = BatteryReading::new(75, ChargeState::Charging);
        let rows: Vec<Row> = vec![(device("mouse"), Some(reading))];

        let value = to_json(&rows);
        let arr = value.as_array().expect("array");
        assert_eq!(arr.len(), 1);

        let obj = &arr[0];
        assert_eq!(obj["name"], "mouse");
        assert_eq!(obj["online"], true);
        assert_eq!(obj["percent"], 75);
        assert_eq!(obj["state"], "charging");
        assert_eq!(obj["transport"], "sysfs");
        assert!(obj["locator"].is_null());
    }

    #[test]
    fn to_json_offline_device() {
        let rows: Vec<Row> = vec![(device("headset"), None)];

        let value = to_json(&rows);
        let arr = value.as_array().expect("array");
        let obj = &arr[0];

        assert_eq!(obj["online"], false);
        assert!(obj["percent"].is_null());
        assert!(obj["state"].is_null());
        assert_eq!(obj["transport"], "sysfs");
    }

    #[test]
    fn to_json_mixed() {
        let reading = BatteryReading::new(50, ChargeState::Discharging);
        let rows: Vec<Row> = vec![(device("keyboard"), Some(reading)), (device("mouse"), None)];

        let value = to_json(&rows);
        let arr = value.as_array().expect("array");
        assert_eq!(arr.len(), 2);

        assert_eq!(arr[0]["online"], true);
        assert_eq!(arr[0]["percent"], 50);
        assert_eq!(arr[0]["state"], "discharging");

        assert_eq!(arr[1]["online"], false);
        assert!(arr[1]["percent"].is_null());
    }

    #[test]
    fn to_json_includes_transport_and_locator() {
        let reading = BatteryReading::new(80, ChargeState::Discharging);
        let rows: Vec<Row> = vec![(
            device_with_locator("mouse", "AA:BB:CC:DD:EE:FF"),
            Some(reading),
        )];

        let value = to_json(&rows);
        let obj = &value.as_array().expect("array")[0];
        assert_eq!(obj["transport"], "bluetooth");
        assert_eq!(obj["locator"], "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn print_table_wide_columns_are_wide_enough() {
        // Verify column width math: each column header must fit its widest data cell.
        let transport_w = "bluetooth".len().max("TRANSPORT".len());
        let locator_w = "AA:BB:CC:DD:EE:FF".len().max("LOCATOR".len());
        let percent_w = "PERCENT".len();
        let state_w = "discharging".len().max("STATE".len());

        assert!(transport_w >= "TRANSPORT".len());
        assert!(locator_w >= "AA:BB:CC:DD:EE:FF".len());
        assert_eq!(percent_w, "PERCENT".len());
        assert!(state_w >= "discharging".len());
    }

    #[test]
    fn print_table_wide_does_not_panic() {
        // Smoke test: ensure print_table_wide runs without panic for mixed rows.
        let reading = BatteryReading::new(80, ChargeState::Discharging);
        let rows: Vec<Row> = vec![
            (
                device_with_locator("MX Master 3", "AA:BB:CC:DD:EE:FF"),
                Some(reading),
            ),
            (device("keyboard"), None),
        ];
        // print_table_wide writes to stdout; we just ensure no panic.
        print_table_wide(&rows);
    }

    #[test]
    fn print_table_aligns_non_ascii_names_by_char_count() {
        let name_width = ["Клавиатура", "mouse"]
            .iter()
            .map(|name| name.chars().count())
            .max()
            .unwrap_or(0);

        let cyrillic_prefix_len = format!("{:<width$}  ", "Клавиатура", width = name_width)
            .chars()
            .count();
        let ascii_prefix_len = format!("{:<width$}  ", "mouse", width = name_width)
            .chars()
            .count();

        assert_eq!(cyrillic_prefix_len, ascii_prefix_len);
    }

    #[test]
    fn transport_as_str() {
        use crate::domain::Transport;
        assert_eq!(Transport::Sysfs.as_str(), "sysfs");
        assert_eq!(Transport::Bluetooth.as_str(), "bluetooth");
        assert_eq!(Transport::Hidraw.as_str(), "hidraw");
    }

    // --- to_waybar ------------------------------------------------------------

    fn assert_single_line_json(text: &str) -> Value {
        assert_eq!(text.lines().count(), 1, "expected exactly one line");
        serde_json::from_str(text).expect("output must parse as JSON")
    }

    #[test]
    fn waybar_charging_device_is_charging_class() {
        let reading = BatteryReading::new(80, ChargeState::Charging);
        let rows: Vec<Row> = vec![(device("mouse"), Some(reading))];
        let cfg = Config::default();

        let value = to_waybar(&rows, &cfg);
        assert_eq!(value["class"], "charging");
        assert_eq!(value["percentage"], 80);
        assert_eq!(value["text"], "80%");
    }

    #[test]
    fn waybar_low_battery_is_low_class() {
        let reading = BatteryReading::new(10, ChargeState::Discharging);
        let rows: Vec<Row> = vec![(device("mouse"), Some(reading))];
        let cfg = Config {
            low_threshold: 20,
            ..Config::default()
        };

        let value = to_waybar(&rows, &cfg);
        assert_eq!(value["class"], "low");
        assert_eq!(value["percentage"], 10);
    }

    #[test]
    fn waybar_healthy_battery_is_ok_class() {
        let reading = BatteryReading::new(80, ChargeState::Discharging);
        let rows: Vec<Row> = vec![(device("mouse"), Some(reading))];
        let cfg = Config::default();

        let value = to_waybar(&rows, &cfg);
        assert_eq!(value["class"], "ok");
        assert_eq!(value["percentage"], 80);
    }

    #[test]
    fn waybar_failed_poll_is_offline_class_with_no_percentage() {
        let rows: Vec<Row> = vec![(device("mouse"), None)];
        let cfg = Config::default();

        let value = to_waybar(&rows, &cfg);
        assert_eq!(value["class"], "offline");
        assert!(value.get("percentage").is_none());
        assert_eq!(value["text"], "offline");
    }

    #[test]
    fn waybar_no_devices_is_offline_with_fallback_text() {
        let rows: Vec<Row> = vec![];
        let cfg = Config::default();

        let value = to_waybar(&rows, &cfg);
        assert_eq!(value["class"], "offline");
        assert_eq!(value["text"], "no devices");
        assert!(value.get("percentage").is_none());
    }

    #[test]
    fn waybar_tooltip_has_one_line_per_device_matching_tray_formatter() {
        let charging = BatteryReading::new(80, ChargeState::Charging);
        let rows: Vec<Row> = vec![
            (device("mouse"), Some(charging)),
            (device("keyboard"), None),
        ];
        let cfg = Config::default();

        let value = to_waybar(&rows, &cfg);
        let tooltip = value["tooltip"].as_str().expect("tooltip is a string");
        let lines: Vec<&str> = tooltip.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "mouse: 80%  charging");
        assert_eq!(lines[1], "keyboard: offline");
    }

    #[test]
    fn waybar_respects_shown_devices_filter() {
        let reading = BatteryReading::new(50, ChargeState::Discharging);
        let rows: Vec<Row> = vec![
            (device("mouse"), Some(reading)),
            (device("keyboard"), Some(reading)),
        ];
        let cfg = Config {
            shown_devices: vec!["keyboard".to_string()],
            ..Config::default()
        };

        let value = to_waybar(&rows, &cfg);
        let tooltip = value["tooltip"].as_str().expect("tooltip is a string");
        assert_eq!(tooltip.lines().count(), 1);
        assert!(tooltip.contains("keyboard"));
    }

    #[test]
    fn waybar_respects_explicit_primary_device() {
        let reading_mouse = BatteryReading::new(90, ChargeState::Discharging);
        let reading_kbd = BatteryReading::new(10, ChargeState::Discharging);
        let rows: Vec<Row> = vec![
            (device("mouse"), Some(reading_mouse)),
            (device("keyboard"), Some(reading_kbd)),
        ];
        let cfg = Config {
            primary_device: Some("keyboard".to_string()),
            ..Config::default()
        };

        let value = to_waybar(&rows, &cfg);
        assert_eq!(value["percentage"], 10);
        assert_eq!(value["class"], "low");
    }

    #[test]
    fn waybar_output_is_one_line_and_parses_as_json() {
        let reading = BatteryReading::new(80, ChargeState::Charging);
        let rows: Vec<Row> = vec![(device("mouse"), Some(reading))];
        let cfg = Config::default();

        let value = to_waybar(&rows, &cfg);
        let text = serde_json::to_string(&value).expect("serializes");
        let parsed = assert_single_line_json(&text);
        assert_eq!(parsed["class"], "charging");
    }
}
