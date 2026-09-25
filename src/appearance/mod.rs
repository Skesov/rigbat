use std::time::Duration;

use anyhow::Context as _;
use futures_util::StreamExt;
use tokio::sync::watch;

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
