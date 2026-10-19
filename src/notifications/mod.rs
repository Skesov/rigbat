use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt as _;
use tokio::sync::watch;

use crate::config::Config;
use crate::domain::{DeviceId, TrayState};
use crate::i18n::{fl, loader};

mod tracker;
use tracker::{Command, LowTracker, Urgency, compute_commands};

/// Upper bound on a single `notify` call. The D-Bus default reply timeout is
/// 25 s; a toast that hasn't been accepted well before that has already
/// missed its purpose, so this bounds the notifier task's stall, not the
/// D-Bus call itself.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// The shipped desktop file id (`packaging/rigbat.desktop`), so hosts can
/// attribute and mute rigbat's notifications per app.
const DESKTOP_ENTRY: &str = "rigbat";

/// The spec's action key for a click on the notification body.
const DEFAULT_ACTION: &str = "default";

/// zbus proxy for org.freedesktop.Notifications (session bus).
#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[expect(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    async fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// The notifications the server is showing for rigbat: the id `Notify`
/// returned per device, reused as `replaces_id` so a device never stacks.
struct Toasts<'a> {
    proxy: NotificationsProxy<'a>,
    ids: HashMap<DeviceId, u32>,
}

impl Toasts<'_> {
    async fn show(
        &mut self,
        device: &DeviceId,
        percent: u8,
        urgency: Urgency,
        lang: crate::i18n::Lang,
    ) {
        let l = loader(lang);
        let summary = fl!(l, "notify-low-title", name = device.name.as_str());
        let body = match urgency {
            Urgency::Normal => fl!(l, "notify-low-body", percent = percent),
            Urgency::Critical => fl!(l, "notify-critical-body", percent = percent),
        };
        let open = fl!(l, "notify-open-overview");
        let actions = [DEFAULT_ACTION, open.as_str()];
        let hints = HashMap::from([
            ("urgency", zbus::zvariant::Value::U8(urgency.wire())),
            ("category", zbus::zvariant::Value::from("device")),
            ("desktop-entry", zbus::zvariant::Value::from(DESKTOP_ENTRY)),
        ]);
        let replaces_id = self.ids.get(device).copied().unwrap_or(0);
        let send = self.proxy.notify(
            "rigbat",
            replaces_id,
            "battery-caution",
            &summary,
            &body,
            &actions,
            hints,
            -1,
        );
        // A lost toast stays notified: re-arming would retry a wedged daemon every poll.
        if let Some(id) = bounded(device, "delivered", send).await {
            self.ids.insert(device.clone(), id);
            tracing::info!(
                device = %device.name,
                percent,
                ?urgency,
                "low battery alert delivered"
            );
        }
    }

    async fn close(&mut self, device: &DeviceId) {
        let Some(id) = self.ids.remove(device) else {
            return;
        };
        let close = self.proxy.close_notification(id);
        if bounded(device, "withdrawn", close).await.is_some() {
            tracing::debug!(device = %device.name, "low battery alert withdrawn");
        }
    }

    fn is_ours(&self, id: u32) -> bool {
        self.ids.values().any(|&shown| shown == id)
    }

    /// The server closed `id` (expired, dismissed or clicked): it can no
    /// longer be replaced or withdrawn.
    fn forget(&mut self, id: u32) {
        self.ids.retain(|_, shown| *shown != id);
    }
}

/// `call` bounded by `NOTIFY_TIMEOUT`; a failure logs "low battery alert not {what}".
async fn bounded<T>(
    device: &DeviceId,
    what: &str,
    call: impl Future<Output = zbus::Result<T>>,
) -> Option<T> {
    match tokio::time::timeout(NOTIFY_TIMEOUT, call).await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            tracing::warn!(device = %device.name, "low battery alert not {what}: {e}");
            None
        }
        Err(_) => {
            tracing::warn!(
                device = %device.name,
                "low battery alert not {what}: timed out after {NOTIFY_TIMEOUT:?}"
            );
            None
        }
    }
}

/// Spawns a background task that fires a desktop notification whenever a device
/// crosses into the low-battery state while discharging, and withdraws it when
/// the crossing is over. Clicking a notification calls `open_dashboard`.
///
/// The task exits quietly if the session bus or the Notifications service is
/// unavailable — battery monitoring continues unaffected.
pub fn spawn(
    rx: watch::Receiver<TrayState>,
    config_rx: watch::Receiver<Config>,
    open_dashboard: impl Fn() + Send + 'static,
) {
    tokio::spawn(async move {
        let conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "session D-Bus unavailable: {e}; low battery notifications disabled"
                );
                return;
            }
        };
        if let Err(e) = run(&conn, rx, config_rx, open_dashboard).await {
            tracing::warn!(
                "org.freedesktop.Notifications unavailable: {e}; low battery notifications disabled"
            );
        }
    });
}

/// Launching the dashboard toggles it, so a click must not launch it while it is open.
async fn dashboard_open(dbus: &zbus::fdo::DBusProxy<'_>) -> bool {
    let name = zbus::names::BusName::try_from(crate::ipc::DASHBOARD_NAME);
    let owned = async { anyhow::Ok(dbus.name_has_owner(name?).await?) };
    owned.await.unwrap_or_else(|e| {
        tracing::warn!("cannot tell whether the dashboard is open: {e:#}");
        false
    })
}

/// The notifier loop on `conn`, until the state channel closes.
///
/// `config_rx` is watched for the `notifications_enabled` flag. While disabled,
/// `LowTracker::observe` still runs (crossings are consumed silently), so toggling
/// notifications back on does not retroactively spam for devices that are already
/// low — they re-notify only on the next fresh low crossing.
async fn run(
    conn: &zbus::Connection,
    mut rx: watch::Receiver<TrayState>,
    mut config_rx: watch::Receiver<Config>,
    open_dashboard: impl Fn() + Send,
) -> zbus::Result<()> {
    let proxy = NotificationsProxy::new(conn).await?;
    // Subscribed before the first `Notify`, so no click or close is missed.
    let mut invoked = proxy.receive_action_invoked().await?;
    let mut closed = proxy.receive_notification_closed().await?;
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let mut toasts = Toasts {
        proxy,
        ids: HashMap::new(),
    };
    let mut tracker = LowTracker::default();
    // A click or a close needs no new commands; a state or config change does.
    let mut changed = true;

    loop {
        if changed {
            // No watch::Ref may cross an await.
            let (commands, lang) = {
                let state = rx.borrow_and_update();
                let cfg = config_rx.borrow_and_update();
                let commands: Vec<Command> = compute_commands(&state, &cfg, &mut tracker)
                    .into_iter()
                    .filter(|c| cfg.notifications_enabled || matches!(c, Command::Close(_)))
                    .collect();
                (commands, cfg.lang())
            };
            for command in commands {
                match command {
                    Command::Show {
                        device,
                        percent,
                        urgency,
                    } => toasts.show(&device, percent, urgency, lang).await,
                    Command::Close(device) => toasts.close(&device).await,
                }
            }
        }

        changed = tokio::select! {
            r = rx.changed() => {
                if r.is_err() {
                    return Ok(());
                }
                true
            }
            r = config_rx.changed() => {
                if r.is_err() {
                    return Ok(());
                }
                true
            }
            Some(signal) = invoked.next() => {
                if let Ok(args) = signal.args()
                    && args.action_key == DEFAULT_ACTION
                    && toasts.is_ours(args.id)
                    && !dashboard_open(&dbus).await
                {
                    open_dashboard();
                }
                false
            }
            Some(signal) = closed.next() => {
                if let Ok(args) = signal.args() {
                    toasts.forget(args.id);
                }
                false
            }
        };
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn notify_timeout_is_well_under_the_dbus_reply_timeout() {
        assert!(super::NOTIFY_TIMEOUT > std::time::Duration::ZERO);
        assert!(super::NOTIFY_TIMEOUT < std::time::Duration::from_secs(25));
    }

    mod bus {
        use std::collections::HashMap;
        use std::time::Duration;

        use tokio::sync::{mpsc, watch};
        use zbus::object_server::SignalEmitter;
        use zbus::zvariant::OwnedValue;

        use super::super::run;
        use super::super::tracker::tests::{device_state_at, threshold_20};
        use crate::bus_test::isolated;
        use crate::config::Config;
        use crate::domain::{BatteryReading, BootTime, ChargeState, Presence, TrayState};

        const TIMEOUT: Duration = Duration::from_secs(5);
        const PATH: &str = "/org/freedesktop/Notifications";

        #[derive(Debug, PartialEq, Eq)]
        enum Call {
            Notify(Toast),
            Close(u32),
        }

        #[derive(Debug, PartialEq, Eq)]
        struct Toast {
            id: u32,
            replaces_id: u32,
            actions: Vec<String>,
            urgency: Option<u8>,
            desktop_entry: Option<String>,
            category: Option<String>,
            expire_timeout: i32,
        }

        struct FakeServer {
            calls: mpsc::UnboundedSender<Call>,
            last_id: u32,
        }

        #[zbus::interface(name = "org.freedesktop.Notifications")]
        impl FakeServer {
            #[expect(clippy::too_many_arguments)]
            fn notify(
                &mut self,
                _app_name: &str,
                replaces_id: u32,
                _app_icon: &str,
                _summary: &str,
                _body: &str,
                actions: Vec<String>,
                hints: HashMap<String, OwnedValue>,
                expire_timeout: i32,
            ) -> u32 {
                let id = if replaces_id == 0 {
                    self.last_id += 1;
                    self.last_id
                } else {
                    replaces_id
                };
                let text = |key: &str| {
                    hints
                        .get(key)
                        .and_then(|v| v.downcast_ref::<&str>().ok())
                        .map(str::to_owned)
                };
                let toast = Toast {
                    id,
                    replaces_id,
                    actions,
                    urgency: hints
                        .get("urgency")
                        .and_then(|v| v.downcast_ref::<u8>().ok()),
                    desktop_entry: text("desktop-entry"),
                    category: text("category"),
                    expire_timeout,
                };
                self.calls
                    .send(Call::Notify(toast))
                    .expect("the test holds the receiver");
                id
            }

            fn close_notification(&self, id: u32) {
                self.calls
                    .send(Call::Close(id))
                    .expect("the test holds the receiver");
            }

            #[zbus(signal)]
            async fn action_invoked(
                emitter: &SignalEmitter<'_>,
                id: u32,
                action_key: &str,
            ) -> zbus::Result<()>;

            #[zbus(signal)]
            async fn notification_closed(
                emitter: &SignalEmitter<'_>,
                id: u32,
                reason: u32,
            ) -> zbus::Result<()>;
        }

        struct Harness {
            server: zbus::Connection,
            calls: mpsc::UnboundedReceiver<Call>,
            opened: mpsc::UnboundedReceiver<()>,
            state: watch::Sender<TrayState>,
            config: watch::Sender<Config>,
            t0: BootTime,
            tick: u64,
        }

        impl Harness {
            async fn start() -> Self {
                let (calls_tx, calls) = mpsc::unbounded_channel();
                let server = zbus::connection::Builder::session()
                    .expect("private bus")
                    .name("org.freedesktop.Notifications")
                    .expect("name")
                    .serve_at(
                        PATH,
                        FakeServer {
                            calls: calls_tx,
                            last_id: 0,
                        },
                    )
                    .expect("serving the fake server")
                    .build()
                    .await
                    .expect("claiming the notifications name");
                let (state, state_rx) = watch::channel(TrayState {
                    devices: Vec::new(),
                });
                let (config, config_rx) = watch::channel(threshold_20());
                let (opened_tx, opened) = mpsc::unbounded_channel();
                let conn = zbus::Connection::session().await.expect("private bus");
                tokio::spawn(async move {
                    run(&conn, state_rx, config_rx, move || {
                        let _ = opened_tx.send(());
                    })
                    .await
                });
                Self {
                    server,
                    calls,
                    opened,
                    state,
                    config,
                    t0: crate::clock::now(),
                    tick: 0,
                }
            }

            /// Publishes a fresh reading and lets the notifier consume it, so
            /// the next one cannot overwrite it unseen.
            async fn reading(&mut self, percent: u8, charge: ChargeState) {
                self.tick += 1;
                let mut device = device_state_at(
                    "mouse",
                    Presence::Online,
                    Some(percent),
                    Some(self.t0 + Duration::from_secs(60 * self.tick)),
                );
                device.last_reading = Some(BatteryReading::new(percent, charge));
                self.state.send_replace(TrayState {
                    devices: vec![device],
                });
                settle().await;
            }

            async fn next_call(&mut self) -> Call {
                tokio::time::timeout(TIMEOUT, self.calls.recv())
                    .await
                    .expect("a call within the timeout")
                    .expect("the server is alive")
            }

            async fn next_toast(&mut self) -> Toast {
                match self.next_call().await {
                    Call::Notify(toast) => Some(toast),
                    Call::Close(_) => None,
                }
                .expect("a Notify, not a CloseNotification")
            }

            fn emitter(&self) -> SignalEmitter<'_> {
                SignalEmitter::new(&self.server, PATH).expect("a valid path")
            }
        }

        async fn settle() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        #[tokio::test]
        async fn one_toast_per_device_escalates_in_place_and_closes_on_charge() {
            if !isolated(
                module_path!(),
                "one_toast_per_device_escalates_in_place_and_closes_on_charge",
            ) {
                return;
            }
            let mut h = Harness::start().await;
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let first = h.next_toast().await;
            assert_eq!(first.replaces_id, 0);
            assert_eq!(first.urgency, Some(1), "normal at the threshold");
            assert_eq!(first.expire_timeout, -1);
            assert_eq!(first.desktop_entry.as_deref(), Some("rigbat"));
            assert_eq!(first.category.as_deref(), Some("device"));
            assert_eq!(first.actions.first().map(String::as_str), Some("default"));

            h.reading(5, ChargeState::Discharging).await;
            h.reading(4, ChargeState::Discharging).await;
            let critical = h.next_toast().await;
            assert_eq!(critical.replaces_id, first.id, "replaces, never stacks");
            assert_eq!(critical.urgency, Some(2), "critical at 5 % and below");

            h.reading(4, ChargeState::Charging).await;
            assert_eq!(h.next_call().await, Call::Close(first.id));
        }

        #[tokio::test]
        async fn a_click_opens_the_dashboard_and_a_closed_toast_is_not_reused() {
            if !isolated(
                module_path!(),
                "a_click_opens_the_dashboard_and_a_closed_toast_is_not_reused",
            ) {
                return;
            }
            let mut h = Harness::start().await;
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let toast = h.next_toast().await;

            FakeServer::action_invoked(&h.emitter(), toast.id + 100, "default")
                .await
                .expect("emit");
            FakeServer::action_invoked(&h.emitter(), toast.id, "default")
                .await
                .expect("emit");
            tokio::time::timeout(TIMEOUT, h.opened.recv())
                .await
                .expect("the dashboard opens")
                .expect("the notifier is alive");
            settle().await;
            assert!(h.opened.try_recv().is_err(), "a foreign id opened it too");

            FakeServer::notification_closed(&h.emitter(), toast.id, 2)
                .await
                .expect("emit");
            settle().await;
            h.reading(19, ChargeState::Charging).await;
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let next = h.next_toast().await;
            assert_eq!(next.replaces_id, 0, "the dismissed id is gone");
        }

        #[tokio::test]
        async fn hiding_the_device_withdraws_its_toast() {
            if !isolated(module_path!(), "hiding_the_device_withdraws_its_toast") {
                return;
            }
            let mut h = Harness::start().await;
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let toast = h.next_toast().await;
            h.config.send_replace(Config {
                hidden_devices: vec!["mouse".to_owned()],
                ..threshold_20()
            });
            assert_eq!(h.next_call().await, Call::Close(toast.id));
        }

        #[tokio::test]
        async fn a_click_leaves_an_open_overview_alone() {
            if !isolated(module_path!(), "a_click_leaves_an_open_overview_alone") {
                return;
            }
            let mut h = Harness::start().await;
            let _dashboard = zbus::connection::Builder::session()
                .expect("private bus")
                .name(crate::ipc::DASHBOARD_NAME)
                .expect("name")
                .build()
                .await
                .expect("claiming the dashboard name");
            h.reading(19, ChargeState::Discharging).await;
            h.reading(19, ChargeState::Discharging).await;
            let toast = h.next_toast().await;

            FakeServer::action_invoked(&h.emitter(), toast.id, "default")
                .await
                .expect("emit");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                h.opened.try_recv().is_err(),
                "the open overview was toggled"
            );
        }
    }
}
