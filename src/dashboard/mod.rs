//! `rigbat dashboard`: the tray icon's left click (R61). A row per shown
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

use crate::config;
use crate::domain::{DeviceKind, Presence, PrimaryStatus, charge_value, roster_order, status_note};
use crate::gui;
use crate::i18n::{Lang, fl, loader};
use crate::icon::Theme;
use crate::ipc::single_instance::{SingleInstance, acquire_named};
use crate::ipc::{DASHBOARD_NAME, DASHBOARD_PATH, DeviceCard, Snapshot, TRAY_NAME};
use crate::ipc::{Dashboard1Proxy, Tray1Proxy};

const WINDOW_WIDTH: f32 = 380.0;
const MARGIN: f32 = 12.0;
const ROW_HEIGHT: f32 = 48.0;
const ROW_PADDING: f32 = 5.0;
const GLYPH_COLUMN: f32 = 30.0;
const GLYPH_SIZE: f32 = 20.0;
const GAP: f32 = 8.0;
const NOTE_SIZE: f32 = 12.0;
const BAR_HEIGHT: f32 = 4.0;
/// How far the bar's track leans from `extreme_bg_color` toward the fill.
const TRACK_TINT: f32 = 0.25;
/// Opacity of the bar of a row whose reading is not live.
const DIMMED: f32 = 0.6;
const FOOTER_HEIGHT: f32 = 32.0;
const MAX_VISIBLE_ROWS: usize = 10;
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a restarted tray gets to register its state before we give up.
const RESTART_RETRIES: u32 = 10;
const RESTART_RETRY_DELAY: Duration = Duration::from_millis(500);
const REFRESH_SPINNER_LIMIT: Duration = Duration::from_secs(5);

/// Exactly the rows, footer and margins; past `MAX_VISIBLE_ROWS` the list scrolls.
fn window_size(devices: usize) -> [f32; 2] {
    let rows = devices.clamp(1, MAX_VISIBLE_ROWS) as f32;
    [
        WINDOW_WIDTH,
        2.0 * MARGIN + rows * ROW_HEIGHT + FOOTER_HEIGHT,
    ]
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .context("building the tokio runtime for the session bus")
}

// Built once per launch and taken apart at once: boxing would buy nothing.
#[expect(clippy::large_enum_variant)]
enum Startup {
    /// Another dashboard holds the name: this launch is the second click.
    SecondClick,
    Started {
        /// Holds the name and serves `Closer` for as long as it lives.
        bus: Option<zbus::Connection>,
        live: Option<Subscription>,
    },
}

/// The bus setup `run` does from its own thread, which is not a runtime thread.
fn start(rt: &tokio::runtime::Runtime, closer: Closer) -> Startup {
    let conn = match rt.block_on(acquire_named(DASHBOARD_NAME)) {
        SingleInstance::AlreadyRunning => return Startup::SecondClick,
        SingleInstance::Acquired(conn) => conn,
        SingleInstance::Unavailable => {
            return Startup::Started {
                bus: None,
                live: None,
            };
        }
    };
    // `object_server()` spawns zbus's dispatch task, so it must run inside the runtime.
    if let Err(e) = rt.block_on(async { conn.object_server().at(DASHBOARD_PATH, closer).await }) {
        tracing::warn!("a second click will not close this window: {e}");
    }
    // Subscribed before the first read, so a change in between is not lost.
    let live = rt.block_on(subscribe(&conn));
    Startup::Started {
        bus: Some(conn),
        live,
    }
}

pub fn run() -> anyhow::Result<()> {
    let rt = runtime()?;
    let window: Arc<OnceLock<egui::Context>> = Arc::default();
    let close_pending = Arc::new(AtomicBool::new(false));
    let closer = Closer {
        window: window.clone(),
        pending: close_pending.clone(),
    };
    let (_bus, live) = match start(&rt, closer) {
        Startup::SecondClick => {
            rt.block_on(close_running());
            return Ok(());
        }
        Startup::Started { bus, live } => (bus, live),
    };
    // Dropping a zbus proxy or stream spawns a task, and some drop on this thread.
    let _entered = rt.enter();
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

const REFRESH: &str = "\u{21BB}";

struct Dashboard {
    snapshot: Option<Snapshot>,
    received_at: Instant,
    lang: Lang,
    updates: mpsc::Receiver<Option<Snapshot>>,
    tray: Option<(Tray1Proxy<'static>, tokio::runtime::Handle)>,
    /// The window size last asked for, in points.
    size: [f32; 2],
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
            size: window_size(0),
            was_focused: false,
            refreshing: None,
        };
        dashboard.accept(snapshot);
        dashboard.size = dashboard.wanted_size();
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

    fn wanted_size(&self) -> [f32; 2] {
        window_size(self.snapshot.as_ref().map_or(0, |s| s.devices.len()))
    }

    /// A device coming or going changes the row count; the window follows it.
    fn fit_window(&mut self, ctx: &egui::Context) {
        let size = self.wanted_size();
        if size != self.size {
            self.size = size;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size.into()));
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

    /// The whole window: the panel fills it, so no band of bare window shows.
    fn show(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(MARGIN))
            .show_inside(ui, |ui| self.render(ui));
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
        let (lang, elapsed) = (self.lang, self.received_at.elapsed().as_secs());
        let (list, footer) = ui
            .max_rect()
            .split_top_bottom_at_y(ui.max_rect().bottom() - FOOTER_HEIGHT);
        let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list));
        list_ui.spacing_mut().item_spacing.y = 0.0;
        let rows = |ui: &mut egui::Ui| {
            for card in &snapshot.devices {
                render_row(ui, card, lang, elapsed);
            }
        };
        if snapshot.devices.len() > MAX_VISIBLE_ROWS {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(&mut list_ui, rows);
        } else {
            rows(&mut list_ui);
        }
        self.render_footer(ui, footer);
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

    fn render_footer(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let l = loader(self.lang);
        let in_flight = self.refresh_in_flight();
        let layout = egui::Layout::left_to_right(egui::Align::Center);
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
        if let Some((tray, rt)) = &self.tray {
            let button = egui::Button::new(REFRESH);
            let clicked = ui
                .add_enabled(!in_flight, button)
                .on_hover_text(fl!(l, "button-refresh"))
                .clicked();
            if clicked {
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
    }
}

impl eframe::App for Dashboard {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_updates();
        self.close_like_a_popup(ui.ctx());
        self.fit_window(ui.ctx());
        self.show(ui);
        // Ages ("2 h ago") move without any state change.
        ui.ctx().request_repaint_after(Duration::from_secs(30));
    }
}

fn sort_cards(cards: &mut [DeviceCard]) {
    cards.sort_by_cached_key(|c| roster_order(&c.name, c.presence));
}

fn render_row(ui: &mut egui::Ui, card: &DeviceCard, lang: Lang, elapsed: u64) {
    let size = egui::vec2(ui.available_width(), ROW_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    response.on_hover_text(details(card, lang));
    let visuals = ui.visuals().clone();
    let theme = if visuals.dark_mode {
        Theme::dark()
    } else {
        Theme::light()
    };
    let low = matches!(card.status, PrimaryStatus::Low { .. });
    let online = card.presence == Presence::Online;

    ui.painter().text(
        egui::pos2(rect.left() + GLYPH_COLUMN / 2.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        kind_glyph(card.kind),
        egui::FontId::proportional(GLYPH_SIZE),
        visuals.text_color(),
    );

    let body = egui::Rect::from_min_max(
        egui::pos2(rect.left() + GLYPH_COLUMN + GAP, rect.top() + ROW_PADDING),
        egui::pos2(rect.right(), rect.bottom() - ROW_PADDING),
    );
    let (top, bottom) = body.split_top_bottom_at_fraction(0.5);

    let value = egui::RichText::new(value_text(card, lang));
    let value = match (low, online) {
        (true, _) => value.strong().color(color(theme.low)),
        (false, true) => value.strong(),
        (false, false) => value.color(secondary_text(&visuals)),
    };
    let value_rect = place(ui, top, egui::Align::Max, egui::Label::new(value));
    let name = egui::Label::new(egui::RichText::new(&card.name).strong()).truncate();
    place(
        ui,
        top.with_max_x(value_rect.left() - GAP),
        egui::Align::Min,
        name,
    );

    let note = row_note(card, lang, elapsed).map(|note| {
        egui::Label::new(
            egui::RichText::new(note)
                .size(NOTE_SIZE)
                .color(secondary_text(&visuals)),
        )
    });
    let Some(percent) = card.percent else {
        if let Some(note) = note {
            place(ui, bottom, egui::Align::Min, note);
        }
        return;
    };
    let bar_right = match note {
        Some(note) => place(ui, bottom, egui::Align::Max, note).left() - GAP,
        None => bottom.right(),
    };
    let fill = match card.status {
        PrimaryStatus::Low { .. } => color(theme.low),
        PrimaryStatus::Charging { .. } => color(theme.charging),
        PrimaryStatus::Ok { .. } => visuals.selection.bg_fill,
        PrimaryStatus::Offline => color(theme.offline),
    };
    // Like the tray icon: dimmed when not live, except a low reading.
    let opacity = if online || low { 1.0 } else { DIMMED };
    let track = egui::Rect::from_center_size(
        egui::pos2((bottom.left() + bar_right) / 2.0, bottom.center().y),
        egui::vec2(bar_right - bottom.left(), BAR_HEIGHT),
    );
    let filled = track.with_max_x(track.left() + track.width() * f32::from(percent) / 100.0);
    let painter = ui.painter();
    painter.rect_filled(
        track,
        BAR_HEIGHT / 2.0,
        track_color(&visuals, fill).gamma_multiply(opacity),
    );
    painter.rect_filled(filled, BAR_HEIGHT / 2.0, fill.gamma_multiply(opacity));
}

/// Leaves the parent's cursor alone: the row already allocated its rect.
fn place(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    from: egui::Align,
    widget: egui::Label,
) -> egui::Rect {
    let layout = match from {
        egui::Align::Max => egui::Layout::right_to_left(egui::Align::Center),
        _ => egui::Layout::left_to_right(egui::Align::Center),
    };
    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout))
        .add(widget.selectable(false))
        .rect
}

/// A track that belongs to its fill, not a black groove.
fn track_color(visuals: &egui::Visuals, fill: egui::Color32) -> egui::Color32 {
    visuals.extreme_bg_color.lerp_to_gamma(fill, TRACK_TINT)
}

fn kind_glyph(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Mouse => "\u{1F5B1}",
        DeviceKind::Keyboard => "\u{2328}",
        DeviceKind::Headset => "\u{1F3A7}",
        DeviceKind::Controller => "\u{1F3AE}",
        DeviceKind::Other => "\u{1F50B}",
    }
}

fn value_text(card: &DeviceCard, lang: Lang) -> String {
    charge_value(card.presence, card.percent, card.charge, card.status, lang)
}

fn row_note(card: &DeviceCard, lang: Lang, elapsed: u64) -> Option<String> {
    status_note(
        card.presence,
        card.remaining_secs.map(Duration::from_secs),
        card.seen_secs_ago
            .map(|secs| Duration::from_secs(secs + elapsed)),
        lang,
    )
}

fn details(card: &DeviceCard, lang: Lang) -> String {
    let mut parts = vec![card.kind.label(lang), card.transport.as_str().to_owned()];
    if card.in_tray {
        parts.push(fl!(loader(lang), "dashboard-in-tray"));
    }
    parts.join(" · ")
}

/// `weak_text_color` misses WCAG 4.5:1 on a dark panel.
fn secondary_text(visuals: &egui::Visuals) -> egui::Color32 {
    visuals.text_color()
}

fn color([r, g, b, a]: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
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
    use crate::config::DisplayMode;
    use crate::domain::{
        CHARGING_SIGN, ChargeState, DeviceKind, LOW_SIGN, Transport, format_age, format_coarse,
        state_label,
    };
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
            locator: None,
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

    fn charging(name: &str, percent: u8) -> DeviceCard {
        DeviceCard {
            charge: Some(ChargeState::Charging),
            status: PrimaryStatus::Charging { percent },
            remaining_secs: None,
            kind: DeviceKind::Headset,
            ..card(name, Presence::Online, Some(percent))
        }
    }

    fn full(name: &str) -> DeviceCard {
        DeviceCard {
            charge: Some(ChargeState::Full),
            remaining_secs: None,
            kind: DeviceKind::Controller,
            ..card(name, Presence::Online, Some(100))
        }
    }

    fn dashboard(devices: Vec<DeviceCard>, lang: Lang) -> Dashboard {
        let snapshot = Snapshot {
            display_mode: DisplayMode::IconOnly,
            devices,
            hidden: Vec::new(),
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

    /// One row per state the dashboard distinguishes.
    fn every_state() -> Vec<DeviceCard> {
        let mut devices = roster();
        devices.push(charging("Nothing Ear (2)", 40));
        devices.push(full("8BitDo Ultimate 2C Wireless"));
        devices.push(card("Aerox 5 Wireless", Presence::NoAccess, None));
        devices
    }

    fn painted(d: &mut Dashboard) -> Vec<crate::egui_test::Painted> {
        let size = d.wanted_size();
        fully_painted_text_at(size, |ui| d.show(ui))
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
    fn every_row_state_renders_whole_on_one_line_in_every_language() {
        for lang in Lang::ALL {
            let mut d = dashboard(every_state(), lang);
            let painted = painted(&mut d);
            let l = loader(lang);
            let age = format_age(Duration::from_secs(2 * 3600), lang);
            let estimate = format_coarse(Duration::from_secs(7 * 3600), lang);
            let expected = [
                "MX Anywhere 3".to_owned(),
                "Nothing Ear (2)".to_owned(),
                "\u{1F5B1}".to_owned(),
                "\u{1F3A7}".to_owned(),
                "62%".to_owned(),
                format!("{LOW_SIGN} 15%"),
                format!("{CHARGING_SIGN} 40%"),
                format!("100% · {}", state_label(ChargeState::Full, lang)),
                fl!(l, "presence-disconnected"),
                fl!(l, "presence-no-access"),
                fl!(l, "note-remaining", estimate = estimate.as_str()),
                fl!(l, "note-last-reading", age = age.as_str()),
                fl!(l, "note-no-access"),
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

    /// Kind, transport and tray membership live in the tooltip, not the row.
    #[test]
    fn a_row_carries_no_kind_transport_or_state_noise() {
        for lang in Lang::ALL {
            let mut d = dashboard(roster(), lang);
            let painted = painted(&mut d);
            let l = loader(lang);
            let noise = [
                DeviceKind::Mouse.label(lang),
                Transport::Hidraw.as_str().to_owned(),
                fl!(l, "dashboard-in-tray"),
                state_label(ChargeState::Discharging, lang),
            ];
            for p in &painted {
                for word in &noise {
                    assert!(!p.text.contains(word.as_str()), "{lang:?}: {p:?}");
                }
            }
            assert_eq!(
                details(&roster()[0], lang),
                format!(
                    "{} · hidraw · {}",
                    DeviceKind::Mouse.label(lang),
                    fl!(l, "dashboard-in-tray")
                )
            );
        }
    }

    #[test]
    fn a_row_without_access_sorts_after_online_ones() {
        let mut devices = roster();
        devices.insert(0, card("Aerox 5 Wireless", Presence::NoAccess, None));
        let d = dashboard(devices, Lang::En);
        let presences: Vec<Presence> = d
            .snapshot
            .iter()
            .flat_map(|s| &s.devices)
            .map(|c| c.presence)
            .collect();
        assert_eq!(&presences[..2], [Presence::Online, Presence::Online]);
    }

    #[test]
    fn a_retained_reading_that_is_not_low_names_the_presence_not_a_percent() {
        let unreachable = card("m", Presence::Unreachable, Some(88));
        assert_eq!(value_text(&unreachable, Lang::En), "Unreachable");
        let low = card("m", Presence::Unreachable, Some(12));
        assert_eq!(
            value_text(&low, Lang::En),
            format!("{LOW_SIGN} Unreachable")
        );
    }

    #[test]
    fn a_full_reading_at_100_while_discharging_shows_no_state_word() {
        let c = card("m", Presence::Online, Some(100));
        assert_eq!(value_text(&c, Lang::En), "100%");
        assert_eq!(value_text(&full("m"), Lang::En), "100% · full");
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
        let painted = painted(&mut d);
        assert!(painted.iter().any(|p| p.text == "50%"), "{painted:?}");
        assert_single_lines_without_overlap(&painted);
    }

    #[test]
    fn without_a_tray_or_devices_it_says_so_in_every_language() {
        for lang in Lang::ALL {
            let l = loader(lang);
            let mut none = Dashboard::new(None, lang, mpsc::channel().1, None);
            let mut empty = dashboard(Vec::new(), lang);
            for (d, text) in [
                (&mut none, fl!(l, "dashboard-tray-not-running")),
                (&mut empty, fl!(l, "tray-no-devices")),
            ] {
                let painted = painted(d);
                assert!(
                    painted.iter().any(|p| p.text == text),
                    "{lang:?}: {text:?} missing; painted: {painted:?}"
                );
                assert_single_lines_without_overlap(&painted);
            }
        }
    }

    #[test]
    fn every_glyph_is_in_the_bundled_fonts() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        let kinds = [
            DeviceKind::Mouse,
            DeviceKind::Keyboard,
            DeviceKind::Headset,
            DeviceKind::Controller,
            DeviceKind::Other,
        ];
        let font = egui::FontId::proportional(GLYPH_SIZE);
        ctx.fonts_mut(|fonts| {
            for glyph in kinds.map(kind_glyph).into_iter().chain([REFRESH]) {
                assert!(fonts.has_glyphs(&font, glyph), "{glyph:?}");
            }
            for sign in [CHARGING_SIGN, LOW_SIGN] {
                assert!(fonts.has_glyph(&font, sign), "{sign:?}");
            }
        });
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
    fn window_fits_the_rows_exactly_and_scrolls_past_the_cap() {
        let footer_and_margins = 2.0 * MARGIN + FOOTER_HEIGHT;
        assert_eq!(window_size(0), window_size(1));
        assert_eq!(window_size(3)[1], 3.0 * ROW_HEIGHT + footer_and_margins);
        assert_eq!(window_size(40), window_size(MAX_VISIBLE_ROWS));
    }

    #[test]
    fn the_window_follows_a_device_appearing() {
        let ctx = egui::Context::default();
        let mut d = dashboard(roster(), Lang::En);
        d.fit_window(&ctx);
        assert_eq!(d.size, window_size(3));
        d.accept(Some(Snapshot {
            display_mode: DisplayMode::IconOnly,
            devices: every_state(),
            hidden: Vec::new(),
        }));
        d.fit_window(&ctx);
        assert_eq!(d.size, window_size(6));
    }

    #[test]
    fn a_track_is_tinted_toward_its_fill_not_black() {
        let visuals = egui::Visuals::dark();
        let fill = visuals.selection.bg_fill;
        let track = track_color(&visuals, fill);
        assert_ne!(track, visuals.extreme_bg_color);
        assert!(
            gui::contrast_ratio(track, fill) > 1.5,
            "the fill must stand off its track"
        );
    }

    #[test]
    fn secondary_text_is_readable_on_the_panel_in_both_themes() {
        for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
            let text = secondary_text(&visuals);
            assert_eq!(text.a(), 255, "secondary text must be opaque");
            let ratio = gui::contrast_ratio(text, visuals.panel_fill);
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
}

#[cfg(test)]
mod bus_tests {
    use std::time::Duration;

    use tokio::sync::watch;

    use super::*;
    use crate::app::refresh::RefreshSignal;
    use crate::app::supervisor::TrayState;
    use crate::bus_test::isolated;
    use crate::config::Config;
    use crate::domain::{
        BatteryReading, ChargeState, DeviceInfo, DeviceState, Estimate, Transport,
    };

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn closer(pending: &Arc<AtomicBool>) -> Closer {
        Closer {
            window: Arc::default(),
            pending: pending.clone(),
        }
    }

    /// Called from the test thread, which no runtime has entered — as `run` calls it.
    #[test]
    fn start_serves_close_from_a_plain_thread() {
        if !isolated(module_path!(), "start_serves_close_from_a_plain_thread") {
            return;
        }
        let rt = runtime().expect("runtime");
        let pending = Arc::new(AtomicBool::new(false));
        let started = start(&rt, closer(&pending));
        assert!(
            matches!(
                started,
                Startup::Started {
                    bus: Some(_),
                    live: Some(_)
                }
            ),
            "the first launch owns the name and subscribes"
        );

        let second = start(&rt, closer(&Arc::default()));
        assert!(matches!(second, Startup::SecondClick));
        rt.block_on(close_running());
        assert!(
            pending.load(Ordering::SeqCst),
            "Close reached the first window"
        );

        let _entered = rt.enter();
        drop(started);
    }

    fn tray_state(percent: u8) -> TrayState {
        TrayState {
            devices: vec![DeviceState {
                info: DeviceInfo {
                    name: "mouse".to_owned(),
                    kind: DeviceKind::Mouse,
                    transport: Transport::Sysfs,
                    locator: None,
                },
                last_reading: Some(BatteryReading::new(percent, ChargeState::Discharging)),
                last_seen: Some(Instant::now()),
                presence: Presence::Online,
                estimate: Estimate::Unknown,
            }],
        }
    }

    fn percent(update: Option<Snapshot>) -> Option<u8> {
        update.expect("a snapshot").devices[0].percent
    }

    #[test]
    fn follow_tracks_the_tray_leaving_and_coming_back() {
        if !isolated(
            module_path!(),
            "follow_tracks_the_tray_leaving_and_coming_back",
        ) {
            return;
        }
        let rt = runtime().expect("runtime");
        let (state_tx, state_rx) = watch::channel(tray_state(80));
        let (_config_tx, config_rx) = watch::channel(Config::default());
        let tray_bus = rt
            .block_on(
                zbus::connection::Builder::session()
                    .expect("bus")
                    .name(TRAY_NAME)
                    .expect("name")
                    .build(),
            )
            .expect("claiming the tray name");
        rt.spawn(crate::tray::state_service::serve(
            tray_bus.clone(),
            state_rx,
            config_rx,
            RefreshSignal::new(),
        ));

        let dashboard_bus = rt.block_on(zbus::Connection::session()).expect("bus");
        let (tray, changes, owners) = rt.block_on(subscribe(&dashboard_bus)).expect("subscribe");
        let (tx, updates) = mpsc::channel();
        rt.spawn(follow(tray, changes, owners, tx, egui::Context::default()));

        state_tx.send_replace(tray_state(79));
        assert_eq!(
            percent(updates.recv_timeout(TIMEOUT).expect("update")),
            Some(79)
        );

        let dbus = rt.block_on(DBusProxy::new(&tray_bus)).expect("DBus proxy");
        rt.block_on(dbus.release_name(TRAY_NAME.try_into().expect("name")))
            .expect("release");
        assert_eq!(updates.recv_timeout(TIMEOUT).expect("update"), None);

        rt.block_on(dbus.request_name(TRAY_NAME.try_into().expect("name"), Default::default()))
            .expect("request");
        assert_eq!(
            percent(updates.recv_timeout(TIMEOUT).expect("update")),
            Some(79)
        );
    }
}
