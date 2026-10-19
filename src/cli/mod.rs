pub mod waybar;

use serde_json::{Value, json};

use crate::domain::{DeviceInfo, PollOutcome, state_str};

type Row = (DeviceInfo, PollOutcome);

/// STATE column word; never translated, like `state_str`.
fn state_word(outcome: PollOutcome) -> &'static str {
    match outcome {
        PollOutcome::Reading(r) => state_str(r.state),
        PollOutcome::Failed => "offline",
        PollOutcome::NoAccess => "no access",
    }
}

fn to_json(rows: &[Row]) -> Value {
    let items: Vec<Value> = rows
        .iter()
        .map(|(info, outcome)| {
            let reading = outcome.reading();
            let (percent, state) = match reading {
                Some(r) => (json!(r.percent), json!(state_str(r.state))),
                None => (Value::Null, Value::Null),
            };
            let mut item = json!({
                "name": info.name,
                "kind": info.kind.as_str(),
                "transport": info.transport.as_str(),
                "locator": info.locator,
                "online": reading.is_some(),
                "presence": outcome.presence(),
                "percent": percent,
                "state": state,
            });
            if reading.is_some_and(|r| r.coarse) {
                item["coarse"] = json!(true);
            }
            item
        })
        .collect();

    Value::Array(items)
}

// The battery table, JSON and waybar line are the program's output on stdout,
// not diagnostics: the one exemption from the tracing-only print lint.
#[expect(clippy::print_stdout)]
fn out(text: &str) {
    print!("{text}");
}

pub fn print_json(rows: &[Row]) {
    // serde_json::to_string_pretty cannot fail on a valid Value
    let text = serde_json::to_string_pretty(&to_json(rows)).unwrap_or_else(|_| "[]".to_owned());
    out(&format!("{text}\n"));
}

pub fn print_table(rows: &[Row]) {
    out(&table(rows));
}

pub fn print_table_wide(rows: &[Row]) {
    out(&table_wide(rows));
}

fn table(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "No devices found\n".to_owned();
    }
    let lines: Vec<Vec<String>> = rows
        .iter()
        .map(|(info, outcome)| {
            let status = match outcome {
                PollOutcome::Reading(r) => format!("{}%  {}", r.percent, state_str(r.state)),
                PollOutcome::Failed | PollOutcome::NoAccess => state_word(*outcome).to_owned(),
            };
            vec![info.name.clone(), status]
        })
        .collect();
    columns(&lines)
}

fn table_wide(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "No devices found\n".to_owned();
    }
    let header = ["NAME", "KIND", "TRANSPORT", "LOCATOR", "PERCENT", "STATE"];
    let mut lines = vec![header.map(str::to_owned).to_vec()];
    lines.extend(rows.iter().map(|(info, outcome)| {
        let percent = outcome
            .reading()
            .map_or_else(|| "-".to_owned(), |r| format!("{}%", r.percent));
        vec![
            info.name.clone(),
            info.kind.as_str().to_owned(),
            info.transport.as_str().to_owned(),
            info.locator.clone().unwrap_or_else(|| "-".to_owned()),
            percent,
            state_word(*outcome).to_owned(),
        ]
    }));
    columns(&lines)
}

/// Left-aligned columns two spaces apart. Widths count chars, not bytes: names
/// can be non-ASCII. Double-width (CJK) characters still misalign.
fn columns(lines: &[Vec<String>]) -> String {
    let count = lines.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..count)
        .map(|column| {
            lines
                .iter()
                .filter_map(|line| line.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    lines
        .iter()
        .map(|line| {
            let cells: Vec<String> = line
                .iter()
                .zip(&widths)
                .map(|(cell, &width)| format!("{cell:<width$}"))
                .collect();
            format!("{}\n", cells.join("  ").trim_end())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind, Transport};

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            kind: DeviceKind::Other,
            ..DeviceInfo::test(name)
        }
    }

    fn device_with_locator(name: &str, locator: &str) -> DeviceInfo {
        DeviceInfo {
            transport: Transport::Bluetooth,
            locator: Some(locator.to_owned()),
            ..device(name)
        }
    }

    #[test]
    fn to_json_online_device() {
        let reading = BatteryReading::new(75, ChargeState::Charging);
        let rows: Vec<Row> = vec![(device("mouse"), PollOutcome::Reading(reading))];

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
    fn to_json_flags_only_a_coarse_reading() {
        let coarse = BatteryReading::new_coarse(60, ChargeState::Discharging);
        let exact = BatteryReading::new(60, ChargeState::Discharging);
        let rows: Vec<Row> = vec![
            (device("pad"), PollOutcome::Reading(coarse)),
            (device("mouse"), PollOutcome::Reading(exact)),
        ];

        let value = to_json(&rows);
        let arr = value.as_array().expect("array");
        assert_eq!(arr[0]["coarse"], true);
        assert_eq!(arr[0]["percent"], 60);
        assert!(arr[1].get("coarse").is_none());
    }

    #[test]
    fn to_json_offline_device() {
        let rows: Vec<Row> = vec![(device("headset"), PollOutcome::Failed)];

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
        let rows: Vec<Row> = vec![
            (device("keyboard"), PollOutcome::Reading(reading)),
            (device("mouse"), PollOutcome::Failed),
        ];

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
    fn to_json_no_access_device_keeps_existing_keys_and_adds_presence() {
        let reading = BatteryReading::new(50, ChargeState::Discharging);
        let rows: Vec<Row> = vec![
            (device("keyboard"), PollOutcome::Reading(reading)),
            (device("headset"), PollOutcome::Failed),
            (device("mouse"), PollOutcome::NoAccess),
        ];

        let value = to_json(&rows);
        let arr = value.as_array().expect("array");

        assert_eq!(arr[0]["presence"], "online");
        assert_eq!(arr[1]["presence"], "unreachable");
        assert_eq!(arr[2]["presence"], "no_access");
        assert_eq!(arr[2]["online"], false);
        assert!(arr[2]["percent"].is_null());
        assert!(arr[2]["state"].is_null());
    }

    #[test]
    fn table_state_word_tells_no_access_from_offline() {
        assert_eq!(state_word(PollOutcome::Failed), "offline");
        assert_eq!(state_word(PollOutcome::NoAccess), "no access");
    }

    #[test]
    fn to_json_includes_transport_and_locator() {
        let reading = BatteryReading::new(80, ChargeState::Discharging);
        let rows: Vec<Row> = vec![(
            device_with_locator("mouse", "AA:BB:CC:DD:EE:FF"),
            PollOutcome::Reading(reading),
        )];

        let value = to_json(&rows);
        let obj = &value.as_array().expect("array")[0];
        assert_eq!(obj["transport"], "bluetooth");
        assert_eq!(obj["locator"], "AA:BB:CC:DD:EE:FF");
    }

    fn trimmed_lines(text: &str) -> Vec<&str> {
        text.lines().map(str::trim_end).collect()
    }

    #[test]
    fn wide_table_aligns_every_column_under_its_header() {
        let reading = BatteryReading::new(80, ChargeState::Discharging);
        let rows: Vec<Row> = vec![
            (
                device_with_locator("MX Master 3", "AA:BB:CC:DD:EE:FF"),
                PollOutcome::Reading(reading),
            ),
            (device("keyboard"), PollOutcome::Failed),
            (device("mouse"), PollOutcome::NoAccess),
        ];
        assert_eq!(
            trimmed_lines(&table_wide(&rows)),
            [
                "NAME         KIND   TRANSPORT  LOCATOR            PERCENT  STATE",
                "MX Master 3  other  bluetooth  AA:BB:CC:DD:EE:FF  80%      discharging",
                "keyboard     other  sysfs      -                  -        offline",
                "mouse        other  sysfs      -                  -        no access",
            ]
        );
    }

    #[test]
    fn tables_say_so_when_there_are_no_devices() {
        assert_eq!(table(&[]), "No devices found\n");
        assert_eq!(table_wide(&[]), "No devices found\n");
    }

    #[test]
    fn table_aligns_non_ascii_names_by_char_count() {
        let reading = BatteryReading::new(50, ChargeState::Discharging);
        let rows: Vec<Row> = vec![
            (device("Клавиатура"), PollOutcome::Reading(reading)),
            (device("mouse"), PollOutcome::Failed),
        ];
        assert_eq!(
            trimmed_lines(&table(&rows)),
            ["Клавиатура  50%  discharging", "mouse       offline"]
        );
    }

    #[test]
    fn transport_as_str() {
        assert_eq!(Transport::Sysfs.as_str(), "sysfs");
        assert_eq!(Transport::Bluetooth.as_str(), "bluetooth");
        assert_eq!(Transport::Hidraw.as_str(), "hidraw");
    }
}
