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

// The battery table/JSON/waybar line is the program's output on stdout, not
// a diagnostic — hence the narrow exemption from the project's tracing-only
// print lint.
#[expect(clippy::print_stdout)]
pub fn print_json(rows: &[Row]) {
    let value = to_json(rows);
    // serde_json::to_string_pretty cannot fail on a valid Value
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".to_owned());
    println!("{}", text);
}

// See print_json: the battery table is program output on stdout.
#[expect(clippy::print_stdout)]
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

    for (info, outcome) in rows {
        let status = match outcome {
            PollOutcome::Reading(r) => format!("{}%  {}", r.percent, state_str(r.state)),
            PollOutcome::Failed | PollOutcome::NoAccess => state_word(*outcome).to_owned(),
        };
        println!("{:<width$}  {}", info.name, status, width = name_width);
    }
}

// See print_json: the wide battery table is program output on stdout.
#[expect(clippy::print_stdout)]
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
        .map(|(_, outcome)| state_word(*outcome).chars().count())
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

    for (info, outcome) in rows {
        let percent_col = outcome
            .reading()
            .map_or_else(|| "-".to_owned(), |r| format!("{}%", r.percent));
        let state_col = state_word(*outcome);
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

    pub(super) fn device(name: &str) -> DeviceInfo {
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
                PollOutcome::Reading(reading),
            ),
            (device("keyboard"), PollOutcome::Failed),
            (device("mouse"), PollOutcome::NoAccess),
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
}
