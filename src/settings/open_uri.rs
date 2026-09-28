use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context as _;
use eframe::egui;

/// Opens a URL outside this process; injected so tests never start a browser.
pub(super) type Opener =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

pub(super) fn portal() -> Opener {
    Arc::new(|url| Box::pin(open_with_portal(url)))
}

/// A browser started as our child runs inside `rigbat.service` and inherits
/// its sandbox, where it crashes; the portal starts it outside.
async fn open_with_portal(url: String) -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await.context("session bus")?;
    let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    conn.call_method(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        Some("org.freedesktop.portal.OpenURI"),
        "OpenURI",
        &("", url.as_str(), options),
    )
    .await
    .context("xdg-portal OpenURI")?;
    Ok(())
}

/// Opens `url` through `opener`; if that fails, hands it to egui, whose
/// winit backend starts the browser itself.
pub(super) fn open(rt: &tokio::runtime::Runtime, opener: &Opener, ctx: &egui::Context, url: &str) {
    let opened = opener(url.to_owned());
    let ctx = ctx.clone();
    let url = url.to_owned();
    rt.spawn(async move {
        if let Err(e) = opened.await {
            tracing::warn!("cannot open {url} through the portal: {e:#}; opening it directly");
            ctx.open_url(egui::OpenUrl::same_tab(url));
            ctx.request_repaint();
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::sync::mpsc;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue};

    use super::open_with_portal;
    use crate::bus_test::isolated;

    struct FakePortal {
        opened: mpsc::UnboundedSender<(String, String)>,
    }

    #[zbus::interface(name = "org.freedesktop.portal.OpenURI")]
    impl FakePortal {
        #[zbus(name = "OpenURI")]
        fn open_uri(
            &self,
            parent_window: &str,
            uri: &str,
            _options: HashMap<String, OwnedValue>,
        ) -> OwnedObjectPath {
            self.opened
                .send((parent_window.to_owned(), uri.to_owned()))
                .expect("the test holds the receiver");
            OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/t")
                .expect("a valid path")
        }
    }

    #[tokio::test]
    async fn the_url_goes_to_the_portal_and_fails_without_one() {
        if !isolated(
            module_path!(),
            "the_url_goes_to_the_portal_and_fails_without_one",
        ) {
            return;
        }
        let url = "https://example.org/rigbat";
        assert!(open_with_portal(url.to_owned()).await.is_err());

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _portal = zbus::connection::Builder::session()
            .expect("private bus")
            .name("org.freedesktop.portal.Desktop")
            .expect("name")
            .serve_at("/org/freedesktop/portal/desktop", FakePortal { opened: tx })
            .expect("serving the fake portal")
            .build()
            .await
            .expect("claiming the portal name");

        open_with_portal(url.to_owned())
            .await
            .expect("the portal answered");
        assert_eq!(rx.recv().await, Some((String::new(), url.to_owned())));
    }
}
