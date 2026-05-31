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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            kind: DeviceKind::Other,
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
}
