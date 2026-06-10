use std::collections::HashMap;
use std::sync::Arc;

use ksni::{MenuItem, ToolTip, Tray, TrayMethods};
use tokio::sync::{Notify, watch};

use crate::app::supervisor::TrayState;
use crate::appearance::ColorScheme;
use crate::config::Config;
use crate::domain::{PrimaryStatus, freedesktop_icon_name};
use crate::tray::format_device_entry;
use crate::tray::icon::{IconRenderer, Theme, TinySkiaRenderer};

// ---------------------------------------------------------------------------
// sanitize: map non-ASCII-alphanumeric chars to '-' for stable SNI ids
// ---------------------------------------------------------------------------

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// ---------------------------------------------------------------------------
// DeviceTray — one SNI item scoped to a single device (keyed by name)
// ---------------------------------------------------------------------------

pub struct DeviceTray {
    pub key: String,
    pub rx: watch::Receiver<TrayState>,
    pub theme_rx: watch::Receiver<ColorScheme>,
    pub config: watch::Receiver<Config>,
    pub renderer: Box<dyn IconRenderer>,
    pub refresh: Arc<Notify>,
}

impl Tray for DeviceTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        format!("rigbat-{}", sanitize(&self.key))
    }

    fn title(&self) -> String {
        self.key.clone()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let theme = match *self.theme_rx.borrow() {
            ColorScheme::Dark => Theme::dark(),
            ColorScheme::Light => Theme::light(),
        };
        let status = self
            .rx
            .borrow()
            .devices
            .iter()
            .find(|(info, _)| info.name == self.key)
            .map(|(_, reading)| {
                crate::domain::classify(*reading, crate::app::supervisor::LOW_THRESHOLD)
            })
            .unwrap_or(PrimaryStatus::Offline);
        let mode = self.config.borrow().display_mode;
        self.renderer.render(status, &theme, mode)
    }

    fn tool_tip(&self) -> ToolTip {
        let title = {
            let state = self.rx.borrow();
            state
                .devices
                .iter()
                .find(|(info, _)| info.name == self.key)
                .map(|(info, reading)| format_device_entry(info, *reading))
                .unwrap_or_else(|| format!("{}: offline", self.key))
        };
        ToolTip {
            title,
            ..ToolTip::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let label = {
            let state = self.rx.borrow();
            let cfg = self.config.borrow();
            state
                .devices
                .iter()
                .find(|(info, _)| info.name == self.key && cfg.is_shown(&info.name))
                .map(|(info, reading)| (format_device_entry(info, *reading), info.kind))
        };

        let mut items: Vec<MenuItem<Self>> = Vec::new();

        if let Some((entry_label, kind)) = label {
            items.push(MenuItem::Standard(ksni::menu::StandardItem {
                label: entry_label,
                icon_name: freedesktop_icon_name(kind).to_owned(),
                enabled: false,
                ..ksni::menu::StandardItem::default()
            }));
        } else {
            items.push(MenuItem::Standard(ksni::menu::StandardItem {
                label: format!("{}: offline", self.key),
                enabled: false,
                ..ksni::menu::StandardItem::default()
            }));
        }

        items.push(MenuItem::Separator);

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: "Refresh".into(),
            icon_name: "view-refresh".into(),
            activate: Box::new(|app: &mut Self| app.refresh.notify_waiters()),
            ..ksni::menu::StandardItem::default()
        }));

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: "Settings\u{2026}".into(),
            activate: Box::new(|_: &mut Self| match std::env::current_exe() {
                Ok(exe) => {
                    if let Err(e) = std::process::Command::new(exe).arg("settings").spawn() {
                        eprintln!("rigbat: failed to launch settings window: {e}");
                    }
                }
                Err(e) => eprintln!("rigbat: cannot find own executable: {e}"),
            }),
            ..ksni::menu::StandardItem::default()
        }));

        items.push(MenuItem::Separator);

        items.push(MenuItem::Standard(ksni::menu::StandardItem {
            label: "Quit".into(),
            activate: Box::new(|_| std::process::exit(0)),
            ..ksni::menu::StandardItem::default()
        }));

        items
    }
}

// ---------------------------------------------------------------------------
// PlaceholderTray — empty-state fallback when no device is shown
// ---------------------------------------------------------------------------

pub struct PlaceholderTray {
    pub theme_rx: watch::Receiver<ColorScheme>,
    pub config: watch::Receiver<Config>,
    pub renderer: Box<dyn IconRenderer>,
    pub refresh: Arc<Notify>,
}

impl Tray for PlaceholderTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "rigbat".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let theme = match *self.theme_rx.borrow() {
            ColorScheme::Dark => Theme::dark(),
            ColorScheme::Light => Theme::light(),
        };
        let mode = self.config.borrow().display_mode;
        self.renderer.render(PrimaryStatus::Offline, &theme, mode)
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "No devices".into(),
            ..ToolTip::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let items: Vec<MenuItem<Self>> = vec![
            MenuItem::Standard(ksni::menu::StandardItem {
                label: "No devices".into(),
                enabled: false,
                ..ksni::menu::StandardItem::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(ksni::menu::StandardItem {
                label: "Refresh".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|app: &mut Self| app.refresh.notify_waiters()),
                ..ksni::menu::StandardItem::default()
            }),
            MenuItem::Standard(ksni::menu::StandardItem {
                label: "Settings\u{2026}".into(),
                activate: Box::new(|_: &mut Self| match std::env::current_exe() {
                    Ok(exe) => {
                        if let Err(e) = std::process::Command::new(exe).arg("settings").spawn() {
                            eprintln!("rigbat: failed to launch settings window: {e}");
                        }
                    }
                    Err(e) => eprintln!("rigbat: cannot find own executable: {e}"),
                }),
                ..ksni::menu::StandardItem::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(ksni::menu::StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..ksni::menu::StandardItem::default()
            }),
        ];
        items
    }
}

// ---------------------------------------------------------------------------
// reconcile — sync live SNI items against the desired device set
// ---------------------------------------------------------------------------

async fn reconcile(
    items: &mut HashMap<String, ksni::Handle<DeviceTray>>,
    placeholder: &mut Option<ksni::Handle<PlaceholderTray>>,
    rx: &watch::Receiver<TrayState>,
    theme_rx: &watch::Receiver<ColorScheme>,
    config_rx: &watch::Receiver<Config>,
    refresh: &Arc<Notify>,
) {
    // Compute desired key list without holding any watch::Ref across an await.
    let desired: Vec<String> = {
        let state = rx.borrow();
        let cfg = config_rx.borrow();
        let mut v: Vec<String> = Vec::new();
        for (info, _) in &state.devices {
            if cfg.is_shown(&info.name) && !v.contains(&info.name) {
                v.push(info.name.clone());
            }
        }
        v
    }; // borrows dropped here

    let prev_len = items.len();

    // Retire icons for devices that are no longer desired.
    let to_remove: Vec<String> = items
        .keys()
        .filter(|k| !desired.contains(k))
        .cloned()
        .collect();
    for key in to_remove {
        if let Some(handle) = items.remove(&key) {
            handle.shutdown().await;
        }
    }

    // Spawn new icons for devices that just became desired.
    for key in &desired {
        if !items.contains_key(key) {
            let tray = DeviceTray {
                key: key.clone(),
                rx: rx.clone(),
                theme_rx: theme_rx.clone(),
                config: config_rx.clone(),
                renderer: Box::new(TinySkiaRenderer::default()),
                refresh: refresh.clone(),
            };
            match tray.spawn().await {
                Ok(handle) => {
                    items.insert(key.clone(), handle);
                }
                Err(e) => {
                    eprintln!("rigbat: failed to spawn tray icon for {key:?}: {e}");
                }
            }
        }
    }

    // Re-render surviving icons so fresh data is reflected.
    for handle in items.values() {
        let _ = handle.update(|_| {}).await;
    }

    // Manage placeholder: present when the device set is empty.
    if desired.is_empty() {
        if placeholder.is_none() {
            let p = PlaceholderTray {
                theme_rx: theme_rx.clone(),
                config: config_rx.clone(),
                renderer: Box::new(TinySkiaRenderer::default()),
                refresh: refresh.clone(),
            };
            match p.spawn().await {
                Ok(handle) => {
                    *placeholder = Some(handle);
                }
                Err(e) => {
                    eprintln!("rigbat: failed to spawn placeholder tray icon: {e}");
                }
            }
        } else if let Some(handle) = placeholder.as_ref() {
            let _ = handle.update(|_| {}).await;
        }
    } else if let Some(handle) = placeholder.take() {
        handle.shutdown().await;
    }

    // Log only when the visible set changes size.
    if items.len() != prev_len || (prev_len == 0 && desired.is_empty()) {
        eprintln!("rigbat: showing {} device icon(s)", items.len());
    }
}

// ---------------------------------------------------------------------------
// run — the manager loop, called from run_tray
// ---------------------------------------------------------------------------

pub async fn run(
    mut rx: watch::Receiver<TrayState>,
    mut theme_rx: watch::Receiver<ColorScheme>,
    mut config_rx: watch::Receiver<Config>,
    refresh: Arc<Notify>,
) {
    let mut items: HashMap<String, ksni::Handle<DeviceTray>> = HashMap::new();
    let mut placeholder: Option<ksni::Handle<PlaceholderTray>> = None;

    loop {
        reconcile(
            &mut items,
            &mut placeholder,
            &rx,
            &theme_rx,
            &config_rx,
            &refresh,
        )
        .await;

        tokio::select! {
            r = rx.changed() => if r.is_err() { break; },
            r = theme_rx.changed() => if r.is_err() { break; },
            r = config_rx.changed() => if r.is_err() { break; },
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitize_alphanumeric_unchanged() {
        assert_eq!(sanitize("mouse123"), "mouse123");
    }

    #[test]
    fn sanitize_spaces_become_dash() {
        assert_eq!(sanitize("MX Master 3"), "MX-Master-3");
    }

    #[test]
    fn sanitize_punctuation_becomes_dash() {
        assert_eq!(sanitize("kbd/bt#1"), "kbd-bt-1");
    }

    #[test]
    fn sanitize_empty_string() {
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn sanitize_non_ascii_becomes_dash() {
        assert_eq!(sanitize("héadset"), "h-adset");
    }
}
