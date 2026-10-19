use serde_json::{Value, json};

use crate::config::Config;
use crate::domain::{BootTime, DeviceState, PrimaryStatus, Roster, device_line, device_status};
use crate::i18n::Lang;

/// The device the waybar module features and its status: the aggregate tray
/// icon's pick and classification, from the same `domain` policy.
pub fn waybar_featured<'a>(
    states: &'a [DeviceState],
    cfg: &Config,
    now: BootTime,
) -> Option<(&'a DeviceState, PrimaryStatus)> {
    let roster = Roster::visible(
        states,
        |name| cfg.is_shown(name),
        now,
        cfg.hide_offline_after(),
    );
    let device = roster.featured(cfg.primary_device.as_deref())?;
    let (status, _) = device_status(device, cfg.effective_low_threshold(&device.info.name));
    Some((device, status))
}

/// Builds the waybar `custom` module payload (`return-type: json`): a single
/// object describing the featured device (`waybar_featured`), with a tooltip
/// line per device the tray lists, in the tray's order.
///
/// Renders from the live `DeviceState` snapshots `Supervisor` publishes, so a
/// sleeping device keeps its retained reading, as it does in the tray. `now`
/// is a parameter so tests are deterministic.
///
/// `class` vocabulary (documented in the README, styled by the user's CSS):
/// `charging`, `low`, `ok`, `offline`. `percentage` is omitted, not `0`, when
/// there is no reading to report.
pub fn to_waybar(states: &[DeviceState], cfg: &Config, now: BootTime) -> Value {
    let roster = Roster::visible(
        states,
        |name| cfg.is_shown(name),
        now,
        cfg.hide_offline_after(),
    );

    let tooltip = if roster.devices().is_empty() {
        "No devices".to_owned()
    } else {
        roster
            .devices()
            .iter()
            // CLI surface: always English.
            .map(|d| {
                let (status, _) = device_status(d, cfg.effective_low_threshold(&d.info.name));
                device_line(d, status, now, Lang::En)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let (text, class, percentage): (String, &str, Option<u8>) =
        match waybar_featured(states, cfg, now) {
            None => ("no devices".to_owned(), "offline", None),
            Some((_, status)) => match status {
                PrimaryStatus::Offline => ("offline".to_owned(), "offline", None),
                PrimaryStatus::Charging { percent } => {
                    (format!("{percent}%"), "charging", Some(percent))
                }
                PrimaryStatus::Low { percent } => (format!("{percent}%"), "low", Some(percent)),
                PrimaryStatus::Ok { percent } => (format!("{percent}%"), "ok", Some(percent)),
            },
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

/// Serializes the `to_waybar` payload as one line of JSON — waybar's `custom`
/// module reads exactly one JSON object per line. Falls back to a valid (never
/// empty) line on the practically-impossible serialization failure, since a
/// custom module that receives malformed output logs an error on every update.
///
/// Returns the line rather than printing it so the streaming loop can compare
/// consecutive lines and skip a repeat: the supervisor republishes its state on
/// every poll, most of which leave the rendered line byte-identical.
pub(super) fn render_waybar_line(states: &[DeviceState], cfg: &Config, now: BootTime) -> String {
    let value = to_waybar(states, cfg, now);
    serde_json::to_string(&value).unwrap_or_else(|_| {
        r#"{"text":"error","tooltip":"rigbat: failed to render status","class":"offline"}"#
            .to_owned()
    })
}
