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
}
