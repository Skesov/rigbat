//! Single-instance guard: `rigbat tray` and `rigbat dashboard` each claim a
//! well-known session-bus name.
//!
//! `ksni` publishes each tray icon under `org.kde.StatusNotifierItem-<pid>-<n>`,
//! which is keyed by PID and therefore never collides — so two `rigbat tray`
//! processes (e.g. the systemd user service and the "Start with session"
//! autostart entry both enabled) each publish their own icon set silently,
//! doubling every device in the tray. This module claims a fixed well-known
//! name on the session bus instead, which does collide, so the second
//! process can detect the first and stand down.

use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::names::WellKnownName;

use super::TRAY_NAME as NAME;

/// Outcome of trying to become the one tray instance on this session bus.
pub enum SingleInstance {
    /// This process holds `NAME`. Keep the connection alive for the process
    /// lifetime — dropping it releases the name and reopens the collision.
    Acquired(zbus::Connection),
    /// Another instance already holds `NAME`; this process must not publish a
    /// tray.
    AlreadyRunning,
    /// The session bus itself is unavailable, so there is no tray host to
    /// collide with either. Not a reason to refuse to start.
    Unavailable,
}

/// Requests [`NAME`] on the session bus with `DoNotQueue` and without
/// `ReplaceExisting`/`AllowReplacement`, so the instance that already holds
/// the name keeps the tray and a second launch is the one that stands down.
pub async fn acquire() -> SingleInstance {
    acquire_named(NAME).await
}

/// The body of `acquire`, with the name as a parameter so a test can claim a
/// name of its own. Claiming `NAME` in a test would fail on any machine where
/// the maintainer's own tray is running — the test would be asserting that
/// nobody uses the program.
pub async fn acquire_named(name: &str) -> SingleInstance {
    let conn = match zbus::Connection::session().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!("session D-Bus unavailable: {e:#}; skipping single-instance guard");
            return SingleInstance::Unavailable;
        }
    };

    // Goes through the bus proxy rather than `Connection::request_name_with_flags`
    // for two reasons. That helper assumes the name is being taken in order to
    // serve interfaces, so it warns on every start that the object server is
    // not set up yet — noise in the journal for a name held purely as a lock.
    // It also reports "someone else owns it" as `Err(zbus::Error::NameTaken)`
    // rather than the protocol's own `RequestNameReply::Exists`, which puts the
    // one case this module cares about into the same channel as a broken bus.
    // Verified live: an earlier version treated every error as "no guard" and
    // the second tray started and published its icons anyway.
    let dbus = match DBusProxy::new(&conn).await {
        Ok(proxy) => proxy,
        Err(e) => {
            tracing::warn!("org.freedesktop.DBus unavailable: {e:#}; no single-instance guard");
            return SingleInstance::Unavailable;
        }
    };

    let well_known = match WellKnownName::try_from(name) {
        Ok(well_known) => well_known,
        Err(e) => {
            tracing::warn!("{name} is not a valid well-known name: {e}; no single-instance guard");
            return SingleInstance::Unavailable;
        }
    };

    let reply = match dbus
        .request_name(well_known, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(reply) => reply,
        Err(e) => {
            tracing::warn!(
                "requesting {name} on the session bus failed: {e:#}; skipping single-instance guard"
            );
            return SingleInstance::Unavailable;
        }
    };

    match reply {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {
            SingleInstance::Acquired(conn)
        }
        RequestNameReply::Exists | RequestNameReply::InQueue => SingleInstance::AlreadyRunning,
    }
}

#[cfg(test)]
mod tests {
    use super::{SingleInstance, acquire_named};

    /// Requires a live session bus, not guaranteed in every environment this
    /// crate is built or tested in (containers, CI runners without D-Bus), so
    /// this stays a manual/local check rather than a gate test. Run with
    /// `make test-live`.
    #[tokio::test]
    #[ignore = "requires a live session D-Bus, not guaranteed in every test environment"]
    async fn second_acquire_on_same_process_sees_the_first_as_running() {
        // Two acquisitions from the same process both go through the same
        // session bus daemon; the second one observes the first connection's
        // ownership exactly as a second `rigbat tray` process would.
        // A name of this test's own: requesting the production name would
        // fail whenever the maintainer's tray is running.
        let name = format!("org.rigbat.TestGuard{}", std::process::id());

        let first = acquire_named(&name).await;
        assert!(matches!(first, SingleInstance::Acquired(_)));

        let second = acquire_named(&name).await;
        assert!(matches!(second, SingleInstance::AlreadyRunning));
    }
}
