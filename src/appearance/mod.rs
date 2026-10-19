use std::time::Duration;

use anyhow::Context as _;
use futures_util::StreamExt;
use tokio::sync::watch;

use ashpd::desktop::settings::{Contrast, ReducedMotion};

use crate::domain::WindowTheme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Dark,
    Light,
}

/// Reads the portal's color scheme into `tx`, then follows its changes until
/// the signal stream ends. Takes its own connection: ashpd's shared one is
/// never replaced once closed. Run under a retry loop, so a portal or bus
/// restart does not freeze the theme.
pub async fn follow_color_scheme(
    conn: zbus::Connection,
    tx: watch::Sender<ColorScheme>,
) -> anyhow::Result<()> {
    let settings = ashpd::desktop::settings::Settings::with_connection(conn)
        .await
        .context("xdg-portal settings")?;
    if let Ok(cs) = settings.color_scheme().await {
        set_scheme(&tx, map_scheme(cs));
    }
    let mut stream = settings
        .receive_color_scheme_changed()
        .await
        .context("xdg-portal color-scheme signal")?;
    while let Some(cs) = stream.next().await {
        // Repeated portal signals (known COSMIC portal bug) must not wake the tray loop.
        if set_scheme(&tx, map_scheme(cs)) {
            tracing::debug!("color scheme -> {:?}", *tx.borrow());
        }
    }
    Ok(())
}

fn set_scheme(tx: &watch::Sender<ColorScheme>, scheme: ColorScheme) -> bool {
    tx.send_if_modified(|cur| {
        let changed = *cur != scheme;
        *cur = scheme;
        changed
    })
}

/// What a window draws with: the session's scheme, accent, text scale, and
/// its contrast and motion preferences.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Appearance {
    pub scheme: ColorScheme,
    pub accent: Option<[u8; 3]>,
    pub text_scale: f32,
    /// The portal's `contrast` is 1, "higher contrast".
    pub high_contrast: bool,
    /// The portal's `reduced-motion` is 1.
    pub reduced_motion: bool,
}

impl Appearance {
    /// The portal's look with `theme`'s scheme forced over it; accent and text scale stay.
    pub fn with_theme(self, theme: WindowTheme) -> Self {
        let scheme = match theme {
            WindowTheme::System => self.scheme,
            WindowTheme::Light => ColorScheme::Light,
            WindowTheme::Dark => ColorScheme::Dark,
        };
        Self { scheme, ..self }
    }
}

/// Light, as libadwaita and the GNOME HIG default to when nothing says otherwise.
impl Default for Appearance {
    fn default() -> Self {
        Self {
            scheme: ColorScheme::Light,
            accent: None,
            text_scale: 1.0,
            high_contrast: false,
            reduced_motion: false,
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
    HighContrast(bool),
    ReducedMotion(bool),
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
    if let Ok(contrast) = settings.contrast().await {
        initial.high_contrast = contrast == Contrast::High;
    }
    if let Ok(motion) = settings.reduced_motion().await {
        initial.reduced_motion = motion == ReducedMotion::ReducedMotion;
    }
    tx.send_replace(initial);

    let mut changes = Vec::new();
    let scheme = settings.receive_color_scheme_changed().await;
    subscribe(&mut changes, "color-scheme", scheme, |cs| {
        Change::Scheme(map_scheme(cs))
    });
    let accent = settings.receive_accent_color_changed().await;
    subscribe(&mut changes, "accent-color", accent, |c| {
        Change::Accent(map_accent(c.red(), c.green(), c.blue()))
    });
    let scale = settings
        .receive_setting_changed_with_args::<f64>(INTERFACE_NAMESPACE, TEXT_SCALE_KEY)
        .await
        .map(|s| s.filter_map(|r| std::future::ready(r.ok())));
    subscribe(&mut changes, "text-scaling", scale, |scale| {
        Change::TextScale(clamp_text_scale(scale))
    });
    let contrast = settings.receive_contrast_changed().await;
    subscribe(&mut changes, "contrast", contrast, |c| {
        Change::HighContrast(c == Contrast::High)
    });
    let motion = settings.receive_reduced_motion_changed().await;
    subscribe(&mut changes, "reduced-motion", motion, |m| {
        Change::ReducedMotion(m == ReducedMotion::ReducedMotion)
    });
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

fn subscribe<'a, T>(
    changes: &mut Vec<futures_util::stream::BoxStream<'a, Change>>,
    what: &str,
    stream: ashpd::Result<impl futures_util::Stream<Item = T> + Send + 'a>,
    change: impl Fn(T) -> Change + Send + 'a,
) {
    match stream {
        Ok(s) => changes.push(s.map(change).boxed()),
        Err(e) => tracing::warn!("xdg-portal {what} signal unavailable: {e}"),
    }
}

/// Returns whether `change` altered `appearance`.
fn apply_change(appearance: &mut Appearance, change: Change) -> bool {
    let before = *appearance;
    match change {
        Change::Scheme(scheme) => appearance.scheme = scheme,
        Change::Accent(accent) => appearance.accent = accent,
        Change::TextScale(scale) => appearance.text_scale = scale,
        Change::HighContrast(high) => appearance.high_contrast = high,
        Change::ReducedMotion(reduced) => appearance.reduced_motion = reduced,
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

/// "No preference" is light: the portal spec leaves it to the app, and libadwaita prefers light.
fn map_scheme(cs: ashpd::desktop::settings::ColorScheme) -> ColorScheme {
    match cs {
        ashpd::desktop::settings::ColorScheme::PreferDark => ColorScheme::Dark,
        _ => ColorScheme::Light,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ashpd::desktop::settings::ColorScheme as AshpdColorScheme;

    #[test]
    fn only_prefer_dark_maps_to_dark() {
        for (portal, scheme) in [
            (AshpdColorScheme::PreferLight, ColorScheme::Light),
            (AshpdColorScheme::PreferDark, ColorScheme::Dark),
            (AshpdColorScheme::NoPreference, ColorScheme::Light),
        ] {
            assert_eq!(map_scheme(portal), scheme, "{portal:?}");
        }
    }

    #[test]
    fn without_a_portal_windows_are_light() {
        assert_eq!(Appearance::default().scheme, ColorScheme::Light);
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
    fn a_window_theme_overrides_only_the_scheme() {
        let portal = Appearance {
            scheme: ColorScheme::Dark,
            accent: Some([1, 2, 3]),
            text_scale: 1.25,
            high_contrast: true,
            reduced_motion: true,
        };
        assert_eq!(portal.with_theme(WindowTheme::System), portal);
        assert_eq!(
            portal.with_theme(WindowTheme::Light),
            Appearance {
                scheme: ColorScheme::Light,
                ..portal
            }
        );
        let light = Appearance {
            scheme: ColorScheme::Light,
            ..portal
        };
        assert_eq!(light.with_theme(WindowTheme::Dark), portal);
    }

    #[test]
    fn a_repeated_signal_is_not_a_change() {
        let mut appearance = Appearance::default();
        assert!(apply_change(
            &mut appearance,
            Change::Scheme(ColorScheme::Dark)
        ));
        assert!(!apply_change(
            &mut appearance,
            Change::Scheme(ColorScheme::Dark)
        ));
        assert!(apply_change(&mut appearance, Change::TextScale(1.5)));
        assert_eq!(appearance.text_scale, 1.5);
        assert!(apply_change(&mut appearance, Change::HighContrast(true)));
        assert!(apply_change(&mut appearance, Change::ReducedMotion(true)));
        assert!(appearance.high_contrast && appearance.reduced_motion);
    }
}

#[cfg(test)]
mod bus_tests {
    use std::time::Duration;

    use tokio::sync::{mpsc, watch};
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::{OwnedValue, Value};

    use super::{ColorScheme, follow_color_scheme};
    use crate::bus_test::{eventually, isolated};
    use crate::sources::supervise::supervise;

    const TIMEOUT: Duration = Duration::from_secs(5);
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    const PREFER_DARK: u32 = 1;
    const PREFER_LIGHT: u32 = 2;

    struct FakePortal {
        scheme: u32,
    }

    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl FakePortal {
        fn read(&self, _namespace: &str, _key: &str) -> OwnedValue {
            OwnedValue::from(self.scheme)
        }

        #[zbus(property)]
        fn version(&self) -> u32 {
            2
        }

        #[zbus(signal)]
        async fn setting_changed(
            emitter: &SignalEmitter<'_>,
            namespace: &str,
            key: &str,
            value: Value<'_>,
        ) -> zbus::Result<()>;
    }

    async fn until_scheme(rx: &watch::Receiver<ColorScheme>, want: ColorScheme) {
        eventually(TIMEOUT, || async { (*rx.borrow() == want).then_some(()) }).await;
    }

    /// The watcher's connection closing ends its stream; the retry loop must
    /// read the scheme again instead of leaving the theme frozen.
    #[tokio::test]
    async fn the_theme_follows_the_portal_across_a_lost_connection() {
        if !isolated(
            module_path!(),
            "the_theme_follows_the_portal_across_a_lost_connection",
        ) {
            return;
        }
        let portal = zbus::connection::Builder::session()
            .expect("private bus")
            .name("org.freedesktop.portal.Desktop")
            .expect("name")
            .serve_at(
                PORTAL_PATH,
                FakePortal {
                    scheme: PREFER_LIGHT,
                },
            )
            .expect("path")
            .build()
            .await
            .expect("fake portal");
        let (tx, rx) = watch::channel(ColorScheme::Dark);
        let (conns_tx, mut conns) = mpsc::unbounded_channel();
        supervise("test color-scheme watcher", move || {
            let (tx, conns_tx) = (tx.clone(), conns_tx.clone());
            async move {
                let conn = zbus::Connection::session().await?;
                let _ = conns_tx.send(conn.clone());
                follow_color_scheme(conn, tx).await
            }
        });
        let watcher = conns.recv().await.expect("first attempt");
        until_scheme(&rx, ColorScheme::Light).await;

        let settings = portal
            .object_server()
            .interface::<_, FakePortal>(PORTAL_PATH)
            .await
            .expect("portal interface");
        eventually(TIMEOUT, || async {
            FakePortal::setting_changed(
                settings.signal_emitter(),
                "org.freedesktop.appearance",
                "color-scheme",
                Value::from(PREFER_DARK),
            )
            .await
            .expect("SettingChanged");
            (*rx.borrow() == ColorScheme::Dark).then_some(())
        })
        .await;

        settings.get_mut().await.scheme = PREFER_LIGHT;
        watcher
            .close()
            .await
            .expect("closing the watcher's connection");
        until_scheme(&rx, ColorScheme::Light).await;
    }
}
