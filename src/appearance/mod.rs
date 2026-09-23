use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Dark,
    Light,
}

/// Spawns a background task: reads color-scheme from xdg-portal and
/// updates the watch on theme change. If the portal is unavailable, defaults to Dark.
pub fn spawn() -> watch::Receiver<ColorScheme> {
    let (tx, rx) = watch::channel(ColorScheme::Dark);

    tokio::spawn(async move {
        let settings = match ashpd::desktop::settings::Settings::new().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("xdg-portal settings unavailable: {e}; theme stays at its default");
                return;
            }
        };

        if let Ok(cs) = settings.color_scheme().await {
            let scheme = map_scheme(cs);
            tx.send_if_modified(|cur| {
                if *cur != scheme {
                    *cur = scheme;
                    true
                } else {
                    false
                }
            });
        }

        let mut stream = match settings.receive_color_scheme_changed().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "xdg-portal color-scheme signal unavailable: {e}; theme stays at its default"
                );
                return;
            }
        };

        while let Some(cs) = stream.next().await {
            let scheme = map_scheme(cs);
            // Notify receivers only when the scheme actually changes; repeated
            // portal signals (known COSMIC portal bug) must not wake the tray loop.
            let changed = tx.send_if_modified(|cur| {
                if *cur != scheme {
                    *cur = scheme;
                    true
                } else {
                    false
                }
            });
            if changed {
                tracing::debug!("color scheme -> {scheme:?}");
            }
            // All receivers gone (tray exited) — stop the task.
            if tx.is_closed() {
                break;
            }
        }
    });

    rx
}

/// What a window draws with: the session's scheme, accent and text scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Appearance {
    pub scheme: ColorScheme,
    pub accent: Option<[u8; 3]>,
    pub text_scale: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            scheme: ColorScheme::Dark,
            accent: None,
            text_scale: 1.0,
        }
    }
}

const INITIAL_READ_TIMEOUT: Duration = Duration::from_millis(300);
const INTERFACE_NAMESPACE: &str = "org.gnome.desktop.interface";
const TEXT_SCALE_KEY: &str = "text-scaling-factor";

enum Change {
    Scheme(ColorScheme),
    Accent(Option<[u8; 3]>),
    TextScale(f32),
}

/// Waits at most `INITIAL_READ_TIMEOUT` for the first portal read, then follows its signals.
pub async fn window_appearance() -> watch::Receiver<Appearance> {
    let (tx, mut rx) = watch::channel(Appearance::default());
    tokio::spawn(follow_portal(tx));
    // An error means the portal task gave up; the defaults stand.
    let _ = tokio::time::timeout(INITIAL_READ_TIMEOUT, rx.changed()).await;
    rx
}

async fn follow_portal(tx: watch::Sender<Appearance>) {
    let settings = match ashpd::desktop::settings::Settings::new().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("xdg-portal settings unavailable: {e}; windows use the default look");
            return;
        }
    };

    let mut initial = Appearance::default();
    if let Ok(cs) = settings.color_scheme().await {
        initial.scheme = map_scheme(cs);
    }
    if let Ok(c) = settings.accent_color().await {
        initial.accent = map_accent(c.red(), c.green(), c.blue());
    }
    if let Ok(scale) = settings
        .read::<f64>(INTERFACE_NAMESPACE, TEXT_SCALE_KEY)
        .await
    {
        initial.text_scale = clamp_text_scale(scale);
    }
    tx.send_replace(initial);

    let mut changes = Vec::new();
    match settings.receive_color_scheme_changed().await {
        Ok(s) => changes.push(s.map(|cs| Change::Scheme(map_scheme(cs))).boxed()),
        Err(e) => tracing::warn!("xdg-portal color-scheme signal unavailable: {e}"),
    }
    match settings.receive_accent_color_changed().await {
        Ok(s) => changes.push(
            s.map(|c| Change::Accent(map_accent(c.red(), c.green(), c.blue())))
                .boxed(),
        ),
        Err(e) => tracing::warn!("xdg-portal accent-color signal unavailable: {e}"),
    }
    match settings
        .receive_setting_changed_with_args::<f64>(INTERFACE_NAMESPACE, TEXT_SCALE_KEY)
        .await
    {
        Ok(s) => changes.push(
            s.filter_map(|r| std::future::ready(r.ok()))
                .map(|scale| Change::TextScale(clamp_text_scale(scale)))
                .boxed(),
        ),
        Err(e) => tracing::warn!("xdg-portal text-scaling signal unavailable: {e}"),
    }
    if changes.is_empty() {
        return;
    }

    let mut changes = futures_util::stream::select_all(changes);
    loop {
        tokio::select! {
            change = changes.next() => {
                let Some(change) = change else { return };
                tx.send_if_modified(|cur| apply_change(cur, change));
            }
            () = tx.closed() => return,
        }
    }
}

/// Returns whether `change` altered `appearance`.
fn apply_change(appearance: &mut Appearance, change: Change) -> bool {
    let before = *appearance;
    match change {
        Change::Scheme(scheme) => appearance.scheme = scheme,
        Change::Accent(accent) => appearance.accent = accent,
        Change::TextScale(scale) => appearance.text_scale = scale,
    }
    *appearance != before
}

/// The portal marks "no accent" with channels outside 0..=1.
fn map_accent(red: f64, green: f64, blue: f64) -> Option<[u8; 3]> {
    let channel = |c: f64| (0.0..=1.0).contains(&c).then(|| (c * 255.0).round() as u8);
    Some([channel(red)?, channel(green)?, channel(blue)?])
}

fn clamp_text_scale(scale: f64) -> f32 {
    if scale.is_finite() {
        scale.clamp(0.75, 2.0) as f32
    } else {
        1.0
    }
}

fn map_scheme(cs: ashpd::desktop::settings::ColorScheme) -> ColorScheme {
    match cs {
        ashpd::desktop::settings::ColorScheme::PreferLight => ColorScheme::Light,
        _ => ColorScheme::Dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ashpd::desktop::settings::ColorScheme as AshpdColorScheme;

    #[test]
    fn map_prefer_light_to_light() {
        assert_eq!(
            map_scheme(AshpdColorScheme::PreferLight),
            ColorScheme::Light
        );
    }

    #[test]
    fn map_prefer_dark_to_dark() {
        assert_eq!(map_scheme(AshpdColorScheme::PreferDark), ColorScheme::Dark);
    }

    #[test]
    fn map_no_preference_to_dark() {
        assert_eq!(
            map_scheme(AshpdColorScheme::NoPreference),
            ColorScheme::Dark
        );
    }

    #[test]
    fn accent_channels_map_to_bytes() {
        assert_eq!(map_accent(0.25, 0.627, 0.169), Some([0x40, 0xA0, 0x2B]));
        assert_eq!(map_accent(0.0, 1.0, 0.5), Some([0, 255, 128]));
    }

    #[test]
    fn an_out_of_range_accent_means_none() {
        assert_eq!(map_accent(-1.0, -1.0, -1.0), None);
        assert_eq!(map_accent(0.5, 1.5, 0.5), None);
        assert_eq!(map_accent(f64::NAN, 0.5, 0.5), None);
    }

    #[test]
    fn text_scale_is_clamped() {
        assert_eq!(clamp_text_scale(1.25), 1.25);
        assert_eq!(clamp_text_scale(0.5), 0.75);
        assert_eq!(clamp_text_scale(3.0), 2.0);
        assert_eq!(clamp_text_scale(f64::NAN), 1.0);
    }

    #[test]
    fn a_repeated_signal_is_not_a_change() {
        let mut appearance = Appearance::default();
        assert!(apply_change(
            &mut appearance,
            Change::Scheme(ColorScheme::Light)
        ));
        assert!(!apply_change(
            &mut appearance,
            Change::Scheme(ColorScheme::Light)
        ));
        assert!(apply_change(&mut appearance, Change::TextScale(1.5)));
        assert_eq!(appearance.text_scale, 1.5);
    }
}
