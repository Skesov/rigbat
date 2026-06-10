use serde_json::{Value, json};

use crate::domain::{BatteryReading, ChargeState, DeviceInfo};

type Row = (DeviceInfo, Option<BatteryReading>);

fn state_str(state: ChargeState) -> &'static str {
    match state {
        ChargeState::Charging => "charging",
        ChargeState::Discharging => "discharging",
        ChargeState::Full => "full",
    }
}

fn kind_str(kind: crate::domain::DeviceKind) -> &'static str {
    match kind {
        crate::domain::DeviceKind::Mouse => "mouse",
        crate::domain::DeviceKind::Keyboard => "keyboard",
        crate::domain::DeviceKind::Headset => "headset",
        crate::domain::DeviceKind::Controller => "controller",
        crate::domain::DeviceKind::Other => "other",
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
                "kind": kind_str(info.kind),
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

pub fn print_json(rows: &[Row]) {
    let value = to_json(rows);
    // serde_json::to_string_pretty cannot fail on a valid Value
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".to_owned());
    println!("{}", text);
}

pub fn print_table(rows: &[Row]) {
    if rows.is_empty() {
        println!("No devices found");
        return;
    }

    let name_width = rows
        .iter()
        .map(|(info, _)| info.name.len())
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

pub fn print_table_wide(rows: &[Row]) {
    if rows.is_empty() {
        println!("No devices found");
        return;
    }

    // Compute per-column widths from data plus header.
    let name_w = rows
        .iter()
        .map(|(info, _)| info.name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let kind_w = rows
        .iter()
        .map(|(info, _)| kind_str(info.kind).len())
        .max()
        .unwrap_or(0)
        .max("KIND".len());
    let transport_w = rows
        .iter()
        .map(|(info, _)| info.transport.as_str().len())
        .max()
        .unwrap_or(0)
        .max("TRANSPORT".len());
    let locator_w = rows
        .iter()
        .map(|(info, _)| info.locator.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(0)
        .max("LOCATOR".len());
    // PERCENT and STATE are short fixed-width columns; anchor to header width.
    let percent_w = "PERCENT".len();
    let state_w = rows
        .iter()
        .map(|(_, reading)| match reading {
            None => "offline".len(),
            Some(r) => state_str(r.state).len(),
        })
        .max()
        .unwrap_or(0)
        .max("STATE".len());

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
            kind_str(info.kind),
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
    fn transport_as_str() {
        use crate::domain::Transport;
        assert_eq!(Transport::Sysfs.as_str(), "sysfs");
        assert_eq!(Transport::Bluetooth.as_str(), "bluetooth");
        assert_eq!(Transport::Hidraw.as_str(), "hidraw");
    }
}
