use anyhow::Context as _;

/// Shared infrastructure handles a backend may need. Built once at the
/// composition root and passed down; a backend that needs none ignores it.
/// The connection is opened on first use, not at construction, so the CLI
/// modes and hosts without a system bus pay nothing.
#[derive(Default)]
pub struct Context {
    /// `Mutex<Option<_>>` rather than a `OnceCell`, deliberately: the cached
    /// connection has to be *replaceable*. See `system_bus`.
    // A memoized handle, not shared data: the lock serialises one dial.
    #[expect(clippy::disallowed_types)]
    system_bus: tokio::sync::Mutex<Option<zbus::Connection>>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the process-wide system-bus connection, opening it on first call
    /// and re-dialling if the cached one has since died. `zbus::Connection` is
    /// cheap to clone (Arc-backed).
    ///
    /// The health check is not decoration. zbus does not reconnect on its own,
    /// so a `dbus-daemon` restart leaves the cached connection permanently
    /// closed; handing that corpse back to every caller would make BlueZ
    /// devices drop out of the roster for the rest of the process's life.
    /// Before this connection was shared, each discovery sweep opened its own
    /// and self-healed within one sweep — re-dialling here preserves that.
    ///
    /// A failed attempt is not cached either: the slot is left empty, so a bus
    /// that comes up later is picked up by the next call.
    ///
    /// The lock is held across the connect so concurrent callers dial once and
    /// share the result, the same serialisation a `OnceCell` would give.
    pub async fn system_bus(&self) -> anyhow::Result<zbus::Connection> {
        let mut slot = self.system_bus.lock().await;
        if let Some(conn) = slot.as_ref() {
            if !conn.is_closed() {
                return Ok(conn.clone());
            }
            tracing::info!("system D-Bus connection closed; reconnecting");
            *slot = None;
        }
        let conn = zbus::Connection::system()
            .await
            .context("connecting to system D-Bus")?;
        *slot = Some(conn.clone());
        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::Context;

    /// Requires a live system bus (`dbus-daemon` reachable at the system
    /// socket). Not guaranteed in every environment this crate is built or
    /// tested in (containers, CI runners without D-Bus), so this stays a
    /// manual/local check rather than a gate test.
    #[tokio::test]
    #[ignore = "requires a live system D-Bus, not guaranteed in every test environment"]
    async fn system_bus_is_memoized() {
        let ctx = Context::new();
        let a = ctx.system_bus().await.expect("system bus available");
        let b = ctx.system_bus().await.expect("system bus available");
        assert_eq!(a.unique_name(), b.unique_name());
    }

    /// A closed connection must not be handed out again: the next call has to
    /// dial a fresh one, which is what keeps the discovery sweep self-healing
    /// across a `dbus-daemon` restart.
    #[tokio::test]
    #[ignore = "requires a live system D-Bus, not guaranteed in every test environment"]
    async fn closed_connection_is_replaced() {
        let ctx = Context::new();
        let first = ctx.system_bus().await.expect("system bus available");
        let first_name = first.unique_name().cloned();
        // `close` consumes this clone and closes the shared underlying
        // connection, which is exactly the state a dbus-daemon restart leaves
        // behind: the clone still parked in `Context` is now closed too.
        first.close().await.expect("closing the connection");
        let second = ctx.system_bus().await.expect("system bus available");
        assert!(!second.is_closed());
        assert_ne!(second.unique_name().cloned(), first_name);
    }
}
