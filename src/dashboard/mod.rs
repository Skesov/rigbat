//! `rigbat dashboard`: the tray icon's left click (R61). A card per shown
//! device, read from the running tray over the session bus — it never polls a
//! device itself, so it opens instantly and classifies each device exactly as
//! its tray icon does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use eframe::egui;
use futures_util::{Stream, StreamExt as _};
use zbus::fdo::{DBusProxy, NameOwnerChangedStream};

use crate::config::{self, DisplayMode};
use crate::domain::{DeviceKind, Presence, PrimaryStatus, format_age, format_coarse, state_label};
use crate::gui;
use crate::i18n::{Lang, fl, loader};
use crate::icon::{IconRenderer, Theme, TinySkiaRenderer};
use crate::ipc::single_instance::{SingleInstance, acquire_named};
use crate::ipc::{DASHBOARD_NAME, DASHBOARD_PATH, DeviceCard, Snapshot, TRAY_NAME};
use crate::ipc::{Dashboard1Proxy, Tray1Proxy};

const CARD_WIDTH: f32 = 264.0;
const CARD_HEIGHT: f32 = 124.0;
const CARD_PADDING: f32 = 12.0;
/// COSMIC's `radius_m`.
const CARD_RADIUS: f32 = 8.0;
/// Opacity of the icon and bar of a card whose reading is not live.
const DIMMED: f32 = 0.6;
const GAP: f32 = 10.0;
const MARGIN: f32 = 14.0;
const FOOTER_HEIGHT: f32 = 44.0;
const MAX_WINDOW_HEIGHT: f32 = 700.0;
const ICON_PIXELS: u32 = 64;
const ICON_SIZE: f32 = 32.0;
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a restarted tray gets to register its state before we give up.
const RESTART_RETRIES: u32 = 10;
const RESTART_RETRY_DELAY: Duration = Duration::from_millis(500);
const REFRESH_SPINNER_LIMIT: Duration = Duration::from_secs(5);

/// Two cards per row; tall enough for every row up to `MAX_WINDOW_HEIGHT`.
fn window_size(devices: usize) -> [f32; 2] {
    let rows = devices.div_ceil(2).max(1) as f32;
    let width = 2.0 * MARGIN + 2.0 * CARD_WIDTH + GAP;
    let height = 2.0 * MARGIN + rows * CARD_HEIGHT + (rows - 1.0) * GAP + FOOTER_HEIGHT;
    [width, height.min(MAX_WINDOW_HEIGHT)]
}

pub fn run() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .context("building the tokio runtime for the session bus")?;

    let bus = match rt.block_on(acquire_named(DASHBOARD_NAME)) {
        // A second launch is the second click: close the open window instead.
        SingleInstance::AlreadyRunning => {
            rt.block_on(close_running());
            return Ok(());
        }
        SingleInstance::Acquired(conn) => Some(conn),
        SingleInstance::Unavailable => None,
    };

    let window: Arc<OnceLock<egui::Context>> = Arc::default();
    let close_pending = Arc::new(AtomicBool::new(false));
    let mut live = None;
    if let Some(conn) = &bus {
        let closer = Closer {
            window: window.clone(),
            pending: close_pending.clone(),
        };
        if let Err(e) = rt.block_on(conn.object_server().at(DASHBOARD_PATH, closer)) {
            tracing::warn!("a second click will not close this window: {e}");
        }
        // Subscribed before the first read, so a change in between is not lost.
        live = rt.block_on(subscribe(conn));
    }
    let (first, appearance) = rt.block_on(async {
        let first = async {
            match &live {
                Some((tray, _, _)) => fetch(tray).await,
                None => None,
            }
        };
        tokio::join!(first, crate::appearance::window_appearance())
    });
    let tray = live.as_ref().map(|(tray, _, _)| tray.clone());

    let lang = config::load().lang();
    let text_scale = appearance.borrow().text_scale;
    let size = window_size(first.as_ref().map_or(0, |s| s.devices.len()));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(gui::scaled(size, text_scale))
            .with_min_inner_size(gui::scaled([size[0], window_size(0)[1]], text_scale))
            .with_decorations(false)
            .with_title(fl!(loader(lang), "dashboard-title"))
            .with_app_id("rigbat"),
        ..Default::default()
    };
    let handle = rt.handle().clone();
    eframe::run_native(
        "rigbat-dashboard",
        options,
        Box::new(move |cc| {
            gui::apply(&cc.egui_ctx, &appearance.borrow());
            gui::follow(&handle, cc.egui_ctx.clone(), appearance);
            let _ = window.set(cc.egui_ctx.clone());
            if close_pending.load(Ordering::SeqCst) {
                cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            let (tx, updates) = mpsc::channel();
            if let Some((tray, changes, owners)) = live {
                handle.spawn(follow(tray, changes, owners, tx, cc.egui_ctx.clone()));
            }
            Ok(Box::new(Dashboard::new(
                first,
                lang,
                updates,
                tray.zip(Some(handle)),
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

async fn close_running() {
    let result = async {
        let conn = zbus::Connection::session().await?;
        Dashboard1Proxy::new(&conn).await?.close().await
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("could not close the open dashboard: {e}");
    }
}

async fn fetch(tray: &Tray1Proxy<'_>) -> Option<Snapshot> {
    let json = match tokio::time::timeout(FETCH_TIMEOUT, tray.state()).await {
        Ok(Ok(json)) => json,
        Ok(Err(e)) => {
            tracing::debug!("tray state unavailable: {e}");
            return None;
        }
        Err(_) => return None,
    };
    serde_json::from_str(&json)
        .map_err(|e| tracing::warn!("unreadable tray state: {e}"))
        .ok()
}

type Subscription = (
    Tray1Proxy<'static>,
    crate::ipc::StateChangedStream,
    Option<NameOwnerChangedStream>,
);

async fn subscribe(conn: &zbus::Connection) -> Option<Subscription> {
    let tray = Tray1Proxy::new(conn).await.ok()?;
    let changes = match tray.receive_state_changed().await {
        Ok(changes) => changes,
        Err(e) => {
            tracing::warn!("the dashboard will not update live: {e}");
            return None;
        }
    };
    let owners = match DBusProxy::new(conn).await {
        Ok(dbus) => dbus
            .receive_name_owner_changed_with_args(&[(0, TRAY_NAME)])
            .await
            .ok(),
        Err(_) => None,
    };
    Some((tray, changes, owners))
}

/// Re-reads the state on every `StateChanged`, and follows the tray going away
/// or coming back, for as long as the window is open.
async fn follow(
    tray: Tray1Proxy<'static>,
    mut changes: impl Stream + Unpin,
    mut owners: Option<NameOwnerChangedStream>,
    updates: mpsc::Sender<Option<Snapshot>>,
    window: egui::Context,
) {
    loop {
        let snapshot = tokio::select! {
            change = changes.next() => {
                if change.is_none() {
                    return;
                }
                fetch(&tray).await
            }
            Some(owner) = next_owner(&mut owners) => {
                let started = owner.args().is_ok_and(|args| args.new_owner().is_some());
                if started { fetch_restarted(&tray).await } else { None }
            }
        };
        if updates.send(snapshot).is_err() {
            return;
        }
        window.request_repaint();
    }
}

async fn next_owner(
    owners: &mut Option<NameOwnerChangedStream>,
) -> Option<zbus::fdo::NameOwnerChanged> {
    match owners {
        Some(owners) => owners.next().await,
        None => std::future::pending().await,
    }
}

/// A restarted tray owns its name a moment before it serves its state.
async fn fetch_restarted(tray: &Tray1Proxy<'_>) -> Option<Snapshot> {
    for _ in 0..RESTART_RETRIES {
        if let Some(snapshot) = fetch(tray).await {
            return Some(snapshot);
        }
        tokio::time::sleep(RESTART_RETRY_DELAY).await;
    }
    None
}

struct Closer {
    window: Arc<OnceLock<egui::Context>>,
    /// Set when `Close` arrives before the window exists.
    pending: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.rigbat.Dashboard1")]
impl Closer {
    fn close(&self) {
        match self.window.get() {
            Some(window) => {
                window.send_viewport_cmd(egui::ViewportCommand::Close);
                window.request_repaint();
            }
            None => self.pending.store(true, Ordering::SeqCst),
        }
    }
}

/// What an icon is drawn from; the textures are rebuilt only when it changes.
type IconKey = (Vec<(PrimaryStatus, DeviceKind, bool)>, DisplayMode, bool);

struct Dashboard {
    snapshot: Option<Snapshot>,
    received_at: Instant,
    lang: Lang,
    updates: mpsc::Receiver<Option<Snapshot>>,
    tray: Option<(Tray1Proxy<'static>, tokio::runtime::Handle)>,
    icons: Vec<Option<egui::TextureHandle>>,
    icons_for: Option<IconKey>,
    was_focused: bool,
    refreshing: Option<Refreshing>,
}

/// A Refresh click still waiting for its snapshot.
struct Refreshing {
    since: Instant,
    failed: tokio::sync::oneshot::Receiver<()>,
}

impl Dashboard {
    fn new(
        snapshot: Option<Snapshot>,
        lang: Lang,
        updates: mpsc::Receiver<Option<Snapshot>>,
        tray: Option<(Tray1Proxy<'static>, tokio::runtime::Handle)>,
    ) -> Self {
        let mut dashboard = Self {
            snapshot: None,
            received_at: Instant::now(),
            lang,
            updates,
            tray,
            icons: Vec::new(),
            icons_for: None,
            was_focused: false,
            refreshing: None,
        };
        dashboard.accept(snapshot);
        dashboard
    }

    fn accept(&mut self, snapshot: Option<Snapshot>) {
        self.snapshot = snapshot.map(|mut s| {
            sort_cards(&mut s.devices);
            s
        });
        self.received_at = Instant::now();
        self.refreshing = None;
    }

    fn drain_updates(&mut self) {
        while let Ok(snapshot) = self.updates.try_recv() {
            self.accept(snapshot);
        }
    }

    /// Esc, or losing focus once it has had it — without an activation token
    /// the window may open unfocused, and must not close before being seen.
    fn close_like_a_popup(&mut self, ctx: &egui::Context) {
        let (escape, focused) =
            ctx.input(|i| (i.key_pressed(egui::Key::Escape), i.viewport().focused));
        let blurred = match focused {
            Some(true) => {
                self.was_focused = true;
                false
            }
            Some(false) => self.was_focused,
            None => false,
        };
        if escape || blurred {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn render(&mut self, ui: &mut egui::Ui) {
        let l = loader(self.lang);
        let Some(snapshot) = &self.snapshot else {
            ui.centered_and_justified(|ui| ui.label(fl!(l, "dashboard-tray-not-running")));
            return;
        };
        if snapshot.devices.is_empty() {
            ui.centered_and_justified(|ui| ui.label(fl!(l, "tray-no-devices")));
            return;
        }
        self.refresh_icons(ui.ctx());

        let body_height = ui.available_height() - FOOTER_HEIGHT;
        egui::ScrollArea::vertical()
            .max_height(body_height)
            .auto_shrink([false, true])
            .show(ui, |ui| self.render_cards(ui));
        self.render_footer(ui);
    }

    fn render_cards(&self, ui: &mut egui::Ui) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let elapsed = self.received_at.elapsed().as_secs();
        ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
        for (row, pair) in snapshot.devices.chunks(2).enumerate() {
            ui.horizontal_top(|ui| {
                for (col, card) in pair.iter().enumerate() {
                    let icon = self.icons.get(row * 2 + col).and_then(Option::as_ref);
                    render_card(ui, card, icon, self.lang, elapsed);
                }
            });
        }
    }

    fn refresh_in_flight(&mut self) -> bool {
        if let Some(refreshing) = &mut self.refreshing
            && (refreshing.since.elapsed() >= REFRESH_SPINNER_LIMIT
                || refreshing.failed.try_recv().is_ok())
        {
            self.refreshing = None;
        }
        self.refreshing.is_some()
    }

    fn render_footer(&mut self, ui: &mut egui::Ui) {
        let l = loader(self.lang);
        let in_flight = self.refresh_in_flight();
        ui.separator();
        ui.horizontal(|ui| {
            if let Some((tray, rt)) = &self.tray {
                let button = egui::Button::new(fl!(l, "button-refresh"));
                if ui.add_enabled(!in_flight, button).clicked() {
                    let (failed_tx, failed) = tokio::sync::oneshot::channel();
                    self.refreshing = Some(Refreshing {
                        since: Instant::now(),
                        failed,
                    });
                    let tray = tray.clone();
                    rt.spawn(async move {
                        if let Err(e) = tray.refresh().await {
                            tracing::warn!("refresh request failed: {e}");
                            let _ = failed_tx.send(());
                        }
                    });
                }
                if in_flight {
                    ui.add(egui::Spinner::new());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(fl!(l, "tray-settings")).clicked() {
                    open_settings();
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }

    fn refresh_icons(&mut self, ctx: &egui::Context) {
        let dark = ctx.global_style().visuals.dark_mode;
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let key: IconKey = (
            snapshot
                .devices
                .iter()
                .map(|c| (c.status, c.kind, c.stale))
                .collect(),
            snapshot.display_mode,
            dark,
        );
        if self.icons_for.as_ref() == Some(&key) {
            return;
        }
        self.icons = snapshot
            .devices
            .iter()
            .enumerate()
            .map(|(i, card)| {
                let image = icon_image(card, snapshot.display_mode, dark)?;
                Some(ctx.load_texture(
                    format!("device-icon-{i}"),
                    image,
                    egui::TextureOptions::LINEAR,
                ))
            })
            .collect();
        self.icons_for = Some(key);
    }
}

impl eframe::App for Dashboard {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_updates();
        self.close_like_a_popup(ui.ctx());
        egui::Frame::central_panel(ui.style())
            .inner_margin(MARGIN)
            .show(ui, |ui| self.render(ui));
        // Undecorated, so the edge has to be drawn to stand off a light desktop.
        ui.painter().rect_stroke(
            ui.max_rect(),
            0.0,
            ui.visuals().window_stroke,
            egui::StrokeKind::Inside,
        );
        // Ages ("2 h ago") move without any state change.
        ui.ctx().request_repaint_after(Duration::from_secs(30));
    }
}

/// Online devices first, then the rest, each group by name.
fn sort_cards(cards: &mut [DeviceCard]) {
    cards.sort_by(|a, b| {
        (a.presence != Presence::Online)
            .cmp(&(b.presence != Presence::Online))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

fn render_card(
    ui: &mut egui::Ui,
    card: &DeviceCard,
    icon: Option<&egui::TextureHandle>,
    lang: Lang,
    elapsed: u64,
) {
    let l = loader(lang);
    let visuals = ui.visuals().clone();
    let theme = if visuals.dark_mode {
        Theme::dark()
    } else {
        Theme::light()
    };
    let low = matches!(card.status, PrimaryStatus::Low { .. });
    let online = card.presence == Presence::Online;
    // Like the tray icon: dimmed when not live, except a low reading. Text never dims.
    let dimmed = !online && !low;
    let secondary = secondary_text(&visuals);

    egui::Frame::new()
        .fill(visuals.faint_bg_color)
        .stroke(visuals.widgets.noninteractive.bg_stroke)
        .corner_radius(CARD_RADIUS)
        .inner_margin(CARD_PADDING)
        .show(ui, |ui| {
            ui.vertical(|ui| {
                let inner =
                    egui::vec2(CARD_WIDTH, CARD_HEIGHT) - egui::vec2(2.0, 2.0) * CARD_PADDING;
                ui.set_width(inner.x);
                ui.set_min_height(inner.y);
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 4.0);

                ui.horizontal(|ui| {
                    if let Some(icon) = icon {
                        let tint = egui::Color32::WHITE.gamma_multiply(opacity(dimmed));
                        ui.add(
                            egui::Image::new((icon.id(), egui::vec2(ICON_SIZE, ICON_SIZE)))
                                .tint(tint),
                        );
                    }
                    ui.vertical(|ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&card.name).strong()).truncate(),
                        );
                        let mut subtitle =
                            format!("{} · {}", card.kind.label(lang), card.transport.as_str());
                        if card.in_tray {
                            subtitle = format!("{subtitle} · {}", fl!(l, "dashboard-in-tray"));
                        }
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(subtitle).small().color(secondary),
                            )
                            .truncate(),
                        );
                    });
                });

                ui.horizontal(|ui| {
                    let percent = card
                        .percent
                        .map_or_else(|| "—".to_owned(), |p| format!("{p}%"));
                    let mut big = egui::RichText::new(percent).size(24.0).strong();
                    if low {
                        big = big.color(color(theme.low));
                    }
                    ui.label(big);
                    let status = match (online, card.charge) {
                        (true, Some(charge)) => state_label(charge, lang),
                        _ => presence_label(card.presence, lang),
                    };
                    ui.add(
                        egui::Label::new(egui::RichText::new(status).color(secondary)).truncate(),
                    );
                });

                let fill = match card.status {
                    PrimaryStatus::Low { .. } => color(theme.low),
                    PrimaryStatus::Charging { .. } => color(theme.charging),
                    PrimaryStatus::Ok { .. } => visuals.selection.bg_fill,
                    PrimaryStatus::Offline => color(theme.offline),
                };
                let fraction = f32::from(card.percent.unwrap_or(0)) / 100.0;
                ui.scope(|ui| {
                    ui.multiply_opacity(opacity(dimmed));
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .fill(fill)
                            .desired_height(6.0),
                    );
                });

                let footer = if online {
                    card.remaining_secs.map(|secs| {
                        let estimate = format_coarse(Duration::from_secs(secs), lang);
                        fl!(l, "dashboard-remaining", estimate = estimate.as_str())
                    })
                } else {
                    card.seen_secs_ago.map(|secs| {
                        let age = format_age(Duration::from_secs(secs + elapsed), lang);
                        fl!(l, "dashboard-last-reading", age = age.as_str())
                    })
                };
                ui.label(
                    egui::RichText::new(footer.unwrap_or_default())
                        .small()
                        .color(secondary),
                );
            })
        });
}

fn opacity(dimmed: bool) -> f32 {
    if dimmed { DIMMED } else { 1.0 }
}

/// `weak_text_color` misses WCAG 4.5:1 on a dark card.
fn secondary_text(visuals: &egui::Visuals) -> egui::Color32 {
    visuals.text_color()
}

fn presence_label(presence: Presence, lang: Lang) -> String {
    let l = loader(lang);
    match presence {
        Presence::Online => fl!(l, "presence-online"),
        Presence::Unreachable => fl!(l, "presence-unreachable"),
        Presence::Disconnected => fl!(l, "presence-disconnected"),
    }
}

fn color([r, g, b, a]: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// The same picture the device's tray icon shows.
fn icon_image(card: &DeviceCard, mode: DisplayMode, dark: bool) -> Option<egui::ColorImage> {
    let theme = if dark { Theme::dark() } else { Theme::light() };
    let renderer = TinySkiaRenderer {
        sizes: vec![ICON_PIXELS],
    };
    let icon = renderer
        .render(card.status, Some(card.kind), &theme, mode, card.stale)
        .into_iter()
        .next()?;
    // ksni pixels are ARGB in network byte order.
    let rgba: Vec<u8> = icon
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[a, r, g, b]| [r, g, b, a])
        .collect();
    let size = [
        usize::try_from(icon.width).ok()?,
        usize::try_from(icon.height).ok()?,
    ];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, &rgba))
}

fn open_settings() {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(exe).arg("settings").spawn() {
                tracing::error!("failed to launch rigbat settings: {e}");
            }
        }
        Err(e) => tracing::error!("cannot find own executable: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ChargeState, DeviceKind, Transport};
    use crate::egui_test::{assert_single_lines_without_overlap, fully_painted_text_at};

    fn card(name: &str, presence: Presence, percent: Option<u8>) -> DeviceCard {
        let status = match percent {
            Some(p) if p <= 20 => PrimaryStatus::Low { percent: p },
            Some(p) => PrimaryStatus::Ok { percent: p },
            None => PrimaryStatus::Offline,
        };
        DeviceCard {
            name: name.to_owned(),
            kind: DeviceKind::Mouse,
            transport: Transport::Hidraw,
            presence,
            percent,
            charge: percent.map(|_| ChargeState::Discharging),
            status,
            stale: presence != Presence::Online && percent.is_some(),
            seen_secs_ago: percent.map(|_| 2 * 3600),
            remaining_secs: Some(7 * 3600),
            in_tray: true,
        }
    }

    fn dashboard(devices: Vec<DeviceCard>, lang: Lang) -> Dashboard {
        let snapshot = Snapshot {
            display_mode: DisplayMode::IconOnly,
            devices,
        };
        Dashboard::new(Some(snapshot), lang, mpsc::channel().1, None)
    }

    fn roster() -> Vec<DeviceCard> {
        vec![
            card("NuPhy Air75 V2", Presence::Disconnected, Some(88)),
            card("SteelSeries Aerox 5 Wireless", Presence::Online, Some(15)),
            card("MX Anywhere 3", Presence::Online, Some(62)),
        ]
    }

    #[test]
    fn online_devices_come_first_then_by_name() {
        let d = dashboard(roster(), Lang::En);
        let names: Vec<_> = d
            .snapshot
            .iter()
            .flat_map(|s| &s.devices)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "MX Anywhere 3",
                "SteelSeries Aerox 5 Wireless",
                "NuPhy Air75 V2"
            ]
        );
    }

    #[test]
    fn cards_render_whole_in_every_language() {
        for lang in Lang::ALL {
            let mut d = dashboard(roster(), lang);
            let painted = fully_painted_text_at(window_size(3), |ui| d.render(ui));
            let l = loader(lang);
            let estimate = format_coarse(Duration::from_secs(7 * 3600), lang);
            let age = format_age(Duration::from_secs(2 * 3600), lang);
            let expected = [
                "MX Anywhere 3".to_owned(),
                "62%".to_owned(),
                "15%".to_owned(),
                state_label(ChargeState::Discharging, lang),
                fl!(l, "presence-disconnected"),
                fl!(l, "dashboard-remaining", estimate = estimate.as_str()),
                fl!(l, "dashboard-last-reading", age = age.as_str()),
                fl!(l, "tray-settings"),
            ];
            for text in expected {
                assert!(
                    painted.iter().any(|p| p.text == text),
                    "{lang:?}: {text:?} is cut off or missing; painted: {painted:?}"
                );
            }
            assert_single_lines_without_overlap(&painted);
        }
    }

    #[test]
    fn a_long_name_is_truncated_not_wrapped() {
        let mut d = dashboard(
            vec![card(
                "Logitech G Pro X Superlight 2 Lightspeed Wireless Gaming Mouse",
                Presence::Online,
                Some(50),
            )],
            Lang::En,
        );
        let painted = fully_painted_text_at(window_size(1), |ui| d.render(ui));
        assert_single_lines_without_overlap(&painted);
    }

    #[test]
    fn without_a_tray_it_says_so() {
        let mut d = Dashboard::new(None, Lang::Ru, mpsc::channel().1, None);
        let painted = fully_painted_text_at(window_size(0), |ui| d.render(ui));
        assert!(
            painted.iter().any(|p| p.text == "Трей rigbat не запущен."),
            "{painted:?}"
        );
    }

    /// The tray signals on every poll; textures follow what the icon is drawn
    /// from, not every new snapshot.
    #[test]
    fn icons_are_rebuilt_only_when_their_picture_changes() {
        let ctx = egui::Context::default();
        let mut d = dashboard(roster(), Lang::En);
        d.refresh_icons(&ctx);
        let ids = |d: &Dashboard| -> Vec<_> { d.icons.iter().flatten().map(|t| t.id()).collect() };
        let before = ids(&d);

        let mut aged = roster();
        for card in &mut aged {
            card.seen_secs_ago = Some(9_999);
        }
        d.accept(Some(Snapshot {
            display_mode: DisplayMode::IconOnly,
            devices: aged,
        }));
        d.refresh_icons(&ctx);
        assert_eq!(ids(&d), before);

        let mut charging = roster();
        charging[0].status = PrimaryStatus::Charging { percent: 88 };
        d.accept(Some(Snapshot {
            display_mode: DisplayMode::IconOnly,
            devices: charging,
        }));
        d.refresh_icons(&ctx);
        assert_ne!(ids(&d), before);
    }

    #[test]
    fn a_close_before_the_window_exists_is_kept_for_later() {
        let closer = Closer {
            window: Arc::default(),
            pending: Arc::default(),
        };
        closer.close();
        assert!(closer.pending.load(Ordering::SeqCst));
    }

    #[test]
    fn window_grows_by_rows_and_stops_at_the_cap() {
        assert_eq!(window_size(1), window_size(2));
        assert!(window_size(3)[1] > window_size(2)[1]);
        assert!(window_size(40)[1] <= MAX_WINDOW_HEIGHT);
    }

    /// `faint_bg_color` is premultiplied (additive), so composite it over the panel.
    fn card_background(visuals: &egui::Visuals) -> egui::Color32 {
        let (src, dst) = (visuals.faint_bg_color, visuals.panel_fill);
        let keep = 1.0 - f32::from(src.a()) / 255.0;
        let over = |s: u8, d: u8| (f32::from(s) + f32::from(d) * keep).round().min(255.0) as u8;
        egui::Color32::from_rgb(
            over(src.r(), dst.r()),
            over(src.g(), dst.g()),
            over(src.b(), dst.b()),
        )
    }

    #[test]
    fn secondary_text_is_readable_on_the_card_in_both_themes() {
        for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
            let text = secondary_text(&visuals);
            assert_eq!(text.a(), 255, "secondary text must be opaque");
            let ratio = gui::contrast_ratio(text, card_background(&visuals));
            assert!(
                ratio >= 4.5,
                "dark_mode={}: secondary text contrast {ratio:.2}:1 is below 4.5:1",
                visuals.dark_mode
            );
        }
    }

    fn start_refresh(d: &mut Dashboard, since: Instant) -> tokio::sync::oneshot::Sender<()> {
        let (failed_tx, failed) = tokio::sync::oneshot::channel();
        d.refreshing = Some(Refreshing { since, failed });
        failed_tx
    }

    #[test]
    fn the_refresh_spinner_ends_on_a_snapshot_a_failure_or_the_limit() {
        let mut d = dashboard(roster(), Lang::En);

        let _pending = start_refresh(&mut d, Instant::now());
        assert!(d.refresh_in_flight());
        d.accept(None);
        assert!(!d.refresh_in_flight(), "a snapshot ends it");

        drop(start_refresh(&mut d, Instant::now()));
        assert!(
            d.refresh_in_flight(),
            "a successful call waits for the snapshot"
        );

        start_refresh(&mut d, Instant::now()).send(()).unwrap();
        assert!(!d.refresh_in_flight(), "a failed call ends it");

        let _pending = start_refresh(&mut d, Instant::now() - REFRESH_SPINNER_LIMIT);
        assert!(!d.refresh_in_flight(), "the limit ends it");
    }

    #[test]
    fn icon_is_the_tray_picture_at_full_size() {
        let image = icon_image(
            &card("m", Presence::Online, Some(80)),
            DisplayMode::IconOnly,
            true,
        )
        .expect("renders");
        assert_eq!(image.size, [ICON_PIXELS as usize; 2]);
        assert!(image.pixels.iter().any(|p| p.a() > 0), "not blank");
    }
}
