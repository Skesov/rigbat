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
            Err(_) => return,
        };

        if let Ok(cs) = settings.color_scheme().await {
            let _ = tx.send(map_scheme(cs));
        }

        let mut stream = match settings.receive_color_scheme_changed().await {
            Ok(s) => s,
            Err(_) => return,
        };

        while let Some(cs) = stream.next().await {
            if tx.send(map_scheme(cs)).is_err() {
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
