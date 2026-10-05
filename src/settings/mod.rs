mod appearance_tab;
mod devices;
mod devices_tab;
mod general_tab;
mod open_uri;
mod scan;
mod widgets;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, OnceLock};

use eframe::egui;

use crate::autostart;
use crate::config::{self, Config};
use crate::domain::{DeviceId, WindowTheme};
use crate::gui;
use crate::i18n::{Lang, fl, loader};
use crate::ipc::single_instance::{SingleInstance, acquire_named};
use crate::ipc::{SETTINGS_NAME, SETTINGS_PATH, Settings1Proxy};
use crate::state;
use appearance_tab::{PaletteSwatches, StylePreviews};
use devices::{DeleteState, DeviceRow};

/// The minimum is where every tab's column reaches `CONTENT_MAX_WIDTH`; the
/// tabs' width tests run at it.
const WINDOW_DEFAULT_SIZE: [f32; 2] = [720.0, 640.0];
const WINDOW_MIN_SIZE: [f32; 2] = [
    widgets::CONTENT_MAX_WIDTH + 2.0 * widgets::PANEL_MARGIN,
    360.0,
];

/// Global low-battery threshold range, percent. Below 5% the warning fires too
/// late to matter; above 50% it stops meaning "low".
const LOW_THRESHOLD_RANGE: std::ops::RangeInclusive<u8> = 5..=50;

/// The window's top-level sections. Hand-rolled tab bar, not `egui_dock`
/// (a docking system for editor layouts, not a fixed few-section switcher)
/// and not a sidebar (GNOME HIG reserves the sidebar pattern for apps with
/// many destinations or their own iconography; three sections is squarely
/// view-switcher territory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    General,
    Appearance,
    Devices,
}

impl Tab {
    /// In tab-bar order.
    const ALL: [Tab; 3] = [Tab::General, Tab::Appearance, Tab::Devices];

    /// The tab `rigbat settings <name>` opens on.
    pub fn from_arg(arg: &str) -> Option<Self> {
        match arg {
            "general" => Some(Self::General),
            "appearance" => Some(Self::Appearance),
            "devices" => Some(Self::Devices),
            _ => None,
        }
    }

    fn label(self, lang: Lang) -> String {
        let l = loader(lang);
        match self {
            Tab::General => fl!(l, "tab-general"),
            Tab::Appearance => fl!(l, "tab-appearance"),
            Tab::Devices => fl!(l, "tab-devices"),
        }
    }
}

/// One scan's raw result: the running tray's roster (or, with no tray, a
/// discovery-and-poll pass; an error when the tray did not answer), plus a
/// fresh read of the persisted device inventory.
/// `SettingsApp::apply_scan_result` merges the two into the Devices tab's
/// rows.
struct ScanResult {
    discovered: anyhow::Result<scan::Discovered>,
    records: Vec<state::DeviceRecord>,
}

/// Why changes are not being saved; a banner until the next save succeeds.
#[derive(Debug, Clone, PartialEq)]
enum SaveProblem {
    NoConfigDir,
    /// `config.json` does not read or parse, so no writer replaces it.
    Unreadable {
        path: PathBuf,
        detail: String,
    },
    Unwritable {
        path: PathBuf,
        detail: String,
    },
}

impl SaveProblem {
    /// What is wrong with the config file at `path` as the window opens.
    fn at_open(path: Option<&std::path::Path>) -> Option<Self> {
        let Some(path) = path else {
            return Some(Self::NoConfigDir);
        };
        config::read(path).err().map(|e| Self::Unreadable {
            path: path.to_owned(),
            detail: format!("{e:#}"),
        })
    }

    /// Why saving to `path` failed with `err`.
    fn of_save(path: &std::path::Path, err: &anyhow::Error) -> Self {
        if err.is::<config::Unreadable>() {
            let detail = config::read(path)
                .err()
                .map_or_else(|| format!("{err:#}"), |e| format!("{e:#}"));
            Self::Unreadable {
                path: path.to_owned(),
                detail,
            }
        } else {
            Self::Unwritable {
                path: path.to_owned(),
                detail: format!("{err:#}"),
            }
        }
    }

    /// What happened and what to do, then the technical detail if any.
    fn text(&self, lang: Lang) -> (String, Option<&str>) {
        let l = loader(lang);
        match self {
            Self::NoConfigDir => (fl!(l, "config-no-dir"), None),
            Self::Unreadable { path, detail } => (
                fl!(l, "config-unreadable", path = path.display().to_string()),
                Some(detail),
            ),
            Self::Unwritable { path, detail } => {
                let dir = path.parent().unwrap_or(path).display().to_string();
                (fl!(l, "config-unwritable", dir = dir), Some(detail))
            }
        }
    }
}

/// An action the user asked for that did not happen; a banner until the same
/// action succeeds.
#[derive(Debug, Clone, PartialEq)]
enum ActionProblem {
    Autostart { on: bool, detail: String },
    Remove { name: String, detail: String },
}

impl ActionProblem {
    /// What did not happen, what to do, and the technical detail.
    fn text(&self, lang: Lang) -> (String, String, &str) {
        let l = loader(lang);
        match self {
            Self::Autostart { on, detail } => {
                let title = if *on {
                    fl!(l, "problem-autostart-on")
                } else {
                    fl!(l, "problem-autostart-off")
                };
                let dir = autostart::desktop_path()
                    .and_then(|p| p.parent().map(|d| d.display().to_string()))
                    .unwrap_or_else(|| "~/.config/autostart".to_owned());
                (title, fl!(l, "problem-autostart-fix", dir = dir), detail)
            }
            Self::Remove { name, detail } => (
                fl!(l, "problem-remove", name = name.as_str()),
                fl!(l, "problem-remove-fix"),
                detail,
            ),
        }
    }
}

struct SettingsApp {
    config: Config,
    /// Shown above every tab while set.
    action_problem: Option<ActionProblem>,
    /// Writes or removes the autostart entry; a parameter so tests touch no real file.
    set_autostart: fn(bool) -> anyhow::Result<()>,
    /// `None` when there is no home directory; every save then fails.
    config_path: Option<PathBuf>,
    /// Shown above every tab while set.
    save_problem: Option<SaveProblem>,
    /// The last scan that answered; kept when a later one fails.
    discovered: scan::Discovered,
    /// The last scan found a tray running that did not answer.
    tray_unanswered: bool,
    /// Reflects `~/.config/autostart/rigbat.desktop` existence — not stored in Config.
    autostart_enabled: bool,
    /// Whether `rigbat.service` is enabled in the systemd user manager,
    /// checked once when the window opens (T29): a second launch path to the
    /// same `rigbat tray` that the autostart checkbox must not silently
    /// duplicate.
    systemd_service_enabled: bool,
    /// Kept alive for the life of the window and used to spawn scans; never
    /// entered blockingly from `ui()`, which runs on the main thread.
    rt: Arc<tokio::runtime::Runtime>,
    /// Shared across scans so the system-bus connection they open is memoized.
    discovery_ctx: Arc<crate::sources::Context>,
    /// `Some` while a scan's result is outstanding; taken (and cleared) once
    /// `try_recv` yields something.
    scan_rx: Option<mpsc::Receiver<ScanResult>>,
    /// When the scan in flight was spawned; `None` once its result is applied.
    scanning: Option<std::time::Instant>,
    tab: Tab,
    /// `None` if the state store failed to open (see `state::open`) — the
    /// Devices tab then shows only what the current scan finds, same as
    /// this window behaved before the inventory existed.
    store: Option<state::Store>,
    /// The Devices tab's backing list: every inventory record merged with
    /// the last scan. Search is applied to a copy of this on render, never in
    /// place — `device_rows` itself always holds the full, unfiltered set.
    device_rows: Vec<DeviceRow>,
    device_search: String,
    /// The one Devices row shown expanded.
    expanded_device: Option<DeviceId>,
    delete_state: DeleteState,
    style_previews: Option<StylePreviews>,
    palette_swatches: Option<PaletteSwatches>,
    open_uri: open_uri::Opener,
    /// The language the window title was last set in.
    title_lang: Lang,
    /// What the window's look follows; `gui::follow` re-applies on every change.
    theme: tokio::sync::watch::Sender<WindowTheme>,
}

impl SettingsApp {
    /// Saves one user edit. On failure every control keeps showing the value
    /// that is on disk, and the window says why until a save succeeds.
    ///
    /// `edit` names exactly the field the call site just changed — see
    /// `save_edit` for why. On success, `self.config` adopts the freshly
    /// saved config, so any field changed on disk by another process since
    /// this window opened is picked up too, not just the one this call
    /// touched.
    fn persist(&mut self, edit: impl FnOnce(&mut Config)) {
        let Some(path) = self.config_path.clone() else {
            tracing::error!("failed to save config: cannot determine config directory");
            self.save_problem = Some(SaveProblem::NoConfigDir);
            return;
        };
        let load = || config::load_from(&path);
        let save = |cfg: &Config| config::save_to(&path, cfg);
        match save_edit(&load, &save, edit) {
            Ok(on_disk) => {
                self.config = on_disk;
                self.save_problem = None;
            }
            Err(e) => {
                tracing::error!("failed to save config: {e:#}");
                self.save_problem = Some(SaveProblem::of_save(&path, &e));
            }
        }
    }

    /// Spawns one scan on `rt` if none is already in flight, plus a fresh
    /// read of the device inventory, wiring the result to a fresh channel and
    /// waking `egui_ctx` when it lands so the window updates without waiting
    /// for the next input event.
    ///
    /// With a tray running, the scan reads its roster (see `scan`), after
    /// having it re-poll when `kind` is `Refresh`. Without one it polls every
    /// discovered device, because the Devices tab needs each one's charge;
    /// this only runs on an explicit Refresh click or window open, not on a
    /// timer, so the extra device wake-up this costs is the same one-off the
    /// user just asked for, not the continuous drain of a short poll interval.
    fn spawn_scan(&mut self, egui_ctx: egui::Context, kind: scan::Scan) {
        if self.scanning.is_some() {
            return;
        }
        self.scanning = Some(std::time::Instant::now());
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        let ctx = Arc::clone(&self.discovery_ctx);
        let store = self.store.clone();
        self.rt.spawn(async move {
            let discovered = scan::scan_devices(kind, || async {
                let sweeps = crate::discovery::discover_all(&ctx).await;
                crate::app::poll_once(crate::discovery::flatten(sweeps)).await
            })
            .await;
            let records = match &store {
                Some(store) => store.list_devices().await.unwrap_or_else(|e| {
                    tracing::warn!("failed to read device inventory: {e:#}");
                    Vec::new()
                }),
                None => Vec::new(),
            };
            // The receiver is dropped if the window closed mid-scan; ignore that.
            let _ = tx.send(ScanResult {
                discovered,
                records,
            });
            egui_ctx.request_repaint();
        });
    }

    /// Drains a completed scan's result, if any, without blocking. Safe to
    /// call every frame.
    fn poll_scan(&mut self) {
        let Some(rx) = self.scan_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => self.apply_scan_result(result),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.scanning = None;
                self.scan_rx = None;
            }
        }
    }

    /// Applies a freshly completed scan's result: `self.device_rows` (the
    /// Devices tab's rows) is the fresh inventory merged with the scan, or
    /// with the last scan that answered when this one failed.
    /// `hidden_devices` and `device_overrides` are keyed by device name and
    /// are left exactly as the user set them, whether or not the device set
    /// changed since the previous scan.
    fn apply_scan_result(&mut self, result: ScanResult) {
        match result.discovered {
            Ok(discovered) => {
                self.discovered = discovered;
                self.tray_unanswered = false;
            }
            Err(_) => self.tray_unanswered = true,
        }
        self.device_rows = devices::merge_devices(result.records, self.discovered.clone());
        self.scanning = None;
        self.scan_rx = None;
    }
}

impl eframe::App for SettingsApp {
    /// Called each frame; `ui` is the root central panel provided by eframe 0.34.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_scan();
        self.handle_escape(ui);
        self.follow_language(ui.ctx());
        self.follow_theme();

        let frame = egui::Frame::central_panel(ui.style()).inner_margin(widgets::PANEL_MARGIN);
        frame.show(ui, |ui| {
            self.render_tab_bar(ui);
            ui.add_space(widgets::TAB_BAR_GAP);
            self.render_problems(ui);
            match self.tab {
                Tab::General => self.render_general_tab(ui),
                Tab::Appearance => self.render_appearance_tab(ui),
                Tab::Devices => self.render_devices_tab(ui),
            }
        });
    }
}

impl SettingsApp {
    fn follow_language(&mut self, ctx: &egui::Context) {
        let lang = self.config.lang();
        if lang != self.title_lang {
            self.title_lang = lang;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title(lang)));
        }
    }

    fn follow_theme(&self) {
        let theme = self.config.theme;
        self.theme
            .send_if_modified(|current| std::mem::replace(current, theme) != theme);
    }

    /// The save problem and the failed action, if any, as banners.
    fn render_problems(&self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let warn =
            gui::status_colors(ui.visuals(), self.config.palette, gui::targets(ui.ctx())).low;
        if let Some(problem) = &self.save_problem {
            let (text, detail) = problem.text(lang);
            let title = fl!(loader(lang), "config-not-saved");
            widgets::banner(ui, &title, warn, |ui| {
                ui.label(&text);
                if let Some(detail) = detail {
                    ui.label(widgets::secondary(ui, detail));
                }
            });
        }
        if let Some(problem) = &self.action_problem {
            let (title, text, detail) = problem.text(lang);
            widgets::banner(ui, &title, warn, |ui| {
                ui.label(&text);
                ui.label(widgets::secondary(ui, detail));
            });
        }
    }

    fn render_tab_bar(&mut self, ui: &mut egui::Ui) {
        let lang = self.config.lang();
        let labels = Tab::ALL.map(|tab| tab.label(lang));
        let selected = Tab::ALL
            .iter()
            .position(|&tab| tab == self.tab)
            .unwrap_or(0);
        if let Some(tab) = widgets::tab_bar(ui, &labels, selected).and_then(|i| Tab::ALL.get(i)) {
            self.tab = *tab;
        }
    }
}

fn title(lang: Lang) -> String {
    fl!(loader(lang), "settings-title")
}

/// Re-reads the on-disk config via `load_config`, applies `edit` — the one
/// change a call site just made — to that fresh copy, and writes the result
/// back via `save_config`. Returns the saved config on success.
///
/// Replaces the earlier `apply_to`, which copied every window-owned field
/// from the window's in-memory snapshot regardless of whether the user had
/// touched it in this window session. That blanket overwrite is what let the
/// tray's one-time `shown_devices` → `hidden_devices` migration be silently
/// undone: a window opened before the migration ran held `hidden_devices`
/// empty, and its next save — for any field, even an unrelated one — wrote
/// that stale empty list back over the migrated value, permanently, since
/// the migration cannot re-run once `shown_devices` is already empty. `edit`
/// makes that impossible by construction: every field other than the one it
/// names always comes from `load_config`, never from a window snapshot.
///
/// `load_config`/`save_config` are parameters, not `config::load`/`save`
/// called directly, so tests can point this at a temporary file instead of
/// the real `config::config_path()` — mirrors
/// `app::migration::migrate_shown_devices_once`.
fn save_edit<L, S>(
    load_config: &L,
    save_config: &S,
    edit: impl FnOnce(&mut Config),
) -> anyhow::Result<Config>
where
    L: Fn() -> Config,
    S: Fn(&Config) -> anyhow::Result<()>,
{
    let mut on_disk = load_config();
    edit(&mut on_disk);
    save_config(&on_disk)?;
    Ok(on_disk)
}

/// Serves `org.rigbat.Settings1`: a second launch asks the open window to come forward.
#[derive(Clone, Default)]
struct Raiser {
    window: Arc<OnceLock<egui::Context>>,
    /// Set when `Raise` arrives before the window exists.
    pending: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.rigbat.Settings1")]
impl Raiser {
    fn raise(&self) {
        match self.window.get() {
            Some(window) => raise_window(window),
            None => self.pending.store(true, Ordering::SeqCst),
        }
    }
}

impl Raiser {
    /// Called from the eframe creator: applies a `Raise` that came before the window.
    fn attach(&self, window: &egui::Context) {
        let _ = self.window.set(window.clone());
        if self.pending.swap(false, Ordering::SeqCst) {
            raise_window(window);
        }
    }
}

/// On Wayland winit 0.30 ignores `Focus` and un-minimizing; only the
/// attention request reaches the compositor.
fn raise_window(window: &egui::Context) {
    window.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    window.send_viewport_cmd(egui::ViewportCommand::Focus);
    window.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
        egui::UserAttentionType::Informational,
    ));
    window.request_repaint();
}

async fn raise_running() {
    let result = async {
        let conn = zbus::Connection::session().await?;
        Settings1Proxy::new(&conn).await?.raise().await
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("could not raise the open settings window: {e}");
    }
}

/// Keeps a tokio runtime alive for the life of the window (unlike the old
/// discover-once-and-drop approach) so devices that connect after the window
/// opens still show up: a scan is spawned on it at startup and again on
/// every "Refresh" click, never entered blockingly from `ui()`.
fn runtime() -> anyhow::Result<Arc<tokio::runtime::Runtime>> {
    use anyhow::Context as _;

    Ok(Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building the tokio runtime for device discovery")?,
    ))
}

enum Startup {
    /// Another settings window holds the name: this launch only raises it.
    AlreadyRunning,
    Started {
        /// Holds the name and serves `Raiser` for as long as it lives.
        bus: Option<zbus::Connection>,
        appearance: tokio::sync::watch::Receiver<crate::appearance::Appearance>,
    },
}

/// The bus setup `run` does from its own thread, which is not a runtime thread.
fn start(rt: &tokio::runtime::Runtime, raiser: Raiser) -> Startup {
    let bus = match rt.block_on(acquire_named(SETTINGS_NAME)) {
        SingleInstance::AlreadyRunning => return Startup::AlreadyRunning,
        SingleInstance::Acquired(conn) => {
            // `object_server()` spawns zbus's dispatch task, so it must run inside the runtime.
            if let Err(e) =
                rt.block_on(async { conn.object_server().at(SETTINGS_PATH, raiser).await })
            {
                tracing::warn!("a second launch will not raise this window: {e}");
            }
            Some(conn)
        }
        SingleInstance::Unavailable => None,
    };
    let appearance = rt.block_on(crate::appearance::window_appearance());
    Startup::Started { bus, appearance }
}

/// Opens the settings window, or raises the one already open. Blocks until the user closes it.
pub fn run(tab: Tab) -> anyhow::Result<()> {
    let rt = runtime()?;
    // Dropping zbus objects can spawn a task, so `_bus` drops with the runtime entered.
    let _entered = rt.enter();
    let raiser = Raiser::default();
    let (_bus, appearance) = match start(&rt, raiser.clone()) {
        Startup::AlreadyRunning => {
            rt.block_on(raise_running());
            return Ok(());
        }
        Startup::Started { bus, appearance } => (bus, appearance),
    };
    let config = config::load();
    let title_lang = config.lang();
    // A second connection to the same database the tray writes through —
    // safe since the schema migration takes BEGIN IMMEDIATE plus
    // CREATE TABLE IF NOT EXISTS. `None` (no state directory, a corrupt
    // file) degrades to today's scan-only device list, same as the tray
    // treats a missing store as an optimisation, never a dependency.
    let store = state::open();
    let discovery_ctx = Arc::new(crate::sources::Context::new());
    let text_scale = appearance.borrow().text_scale;
    let (theme, theme_rx) = tokio::sync::watch::channel(config.theme);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(gui::scaled(WINDOW_DEFAULT_SIZE, text_scale))
            .with_min_inner_size(gui::scaled(WINDOW_MIN_SIZE, text_scale))
            .with_title(title(title_lang))
            .with_app_id("rigbat"),
        ..Default::default()
    };
    eframe::run_native(
        "rigbat",
        options,
        Box::new(move |cc| {
            raiser.attach(&cc.egui_ctx);
            gui::follow(rt.handle(), cc.egui_ctx.clone(), appearance, theme_rx);
            let config_path = config::config_path();
            let mut app = SettingsApp {
                config,
                action_problem: None,
                set_autostart: autostart::set_enabled,
                save_problem: SaveProblem::at_open(config_path.as_deref()),
                config_path,
                discovered: Vec::new(),
                tray_unanswered: false,
                autostart_enabled: autostart::is_enabled(),
                systemd_service_enabled: autostart::systemd_service_enabled(),
                rt,
                discovery_ctx,
                scan_rx: None,
                scanning: None,
                tab,
                store,
                device_rows: Vec::new(),
                device_search: String::new(),
                expanded_device: None,
                delete_state: DeleteState::default(),
                style_previews: None,
                palette_swatches: None,
                open_uri: open_uri::portal(),
                title_lang,
                theme,
            };
            app.spawn_scan(cc.egui_ctx.clone(), scan::Scan::Read);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeviceSettings;
    use crate::domain::{DeviceInfo, PollOutcome};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique scratch directory under the OS temp dir for one test. Removed
    /// at the end of the test regardless of outcome.
    fn scratch_dir(test_name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rigbat-settings-test-{test_name}-{}-{n}",
            std::process::id()
        ))
    }

    pub(super) fn settings_app_with(mut config: Config) -> SettingsApp {
        // Tests assert English text unless they pick a language; LANG must not decide.
        config.language.get_or_insert_with(|| "en".to_owned());
        let title_lang = config.lang();
        let theme = tokio::sync::watch::Sender::new(config.theme);
        SettingsApp {
            config,
            action_problem: None,
            set_autostart: |_| Ok(()),
            config_path: None,
            save_problem: None,
            discovered: Vec::new(),
            tray_unanswered: false,
            autostart_enabled: false,
            systemd_service_enabled: false,
            rt: Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("building test tokio runtime"),
            ),
            discovery_ctx: Arc::new(crate::sources::Context::new()),
            scan_rx: None,
            scanning: None,
            tab: Tab::General,
            store: None,
            device_rows: Vec::new(),
            device_search: String::new(),
            expanded_device: None,
            delete_state: DeleteState::default(),
            style_previews: None,
            palette_swatches: None,
            open_uri: Arc::new(|_| Box::pin(std::future::ready(Ok(())))),
            title_lang,
            theme,
        }
    }

    /// The narrowest the window gets at `text_scale`, in the window's own
    /// points, and `height` tall.
    pub(super) fn tab_size(text_scale: f32, height: f32) -> [f32; 2] {
        [gui::zoomed_width(WINDOW_MIN_SIZE, text_scale), height]
    }

    #[test]
    fn the_minimum_width_fits_the_smallest_screen_at_every_text_scale() {
        for scale in crate::egui_test::TEXT_SCALES {
            let width = gui::scaled(WINDOW_MIN_SIZE, scale)[0];
            assert!(width <= gui::SMALLEST_SCREEN_WIDTH, "{scale}: {width}");
        }
    }

    pub(super) fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            kind: crate::domain::DeviceKind::Mouse,
            transport: crate::domain::Transport::Hidraw,
            locator: None,
        }
    }

    /// Wraps a plain device list into a `ScanResult` with no reading and no
    /// inventory records — what `apply_scan_result`'s pre-T35 tests exercised
    /// before it started merging in the store's half.
    fn scan_result(devices: Vec<DeviceInfo>) -> ScanResult {
        ScanResult {
            discovered: Ok(devices
                .into_iter()
                .map(|info| scan::ScannedDevice {
                    info,
                    outcome: PollOutcome::Failed,
                    remaining: None,
                    read_at: None,
                })
                .collect()),
            records: Vec::new(),
        }
    }

    /// Unique scratch config file path under the OS temp dir for one test.
    /// Never the real `config::config_path()` — `save_edit` tests must not
    /// touch `~/.config/rigbat/config.json`.
    pub(super) fn scratch_config_path(test_name: &str) -> PathBuf {
        scratch_dir(test_name).join("config.json")
    }

    /// A fresh settings window over a scratch config file, so a click saves
    /// somewhere other than `~/.config/rigbat/config.json`.
    pub(super) fn app_saving_to(test_name: &str, config: Config) -> (SettingsApp, PathBuf) {
        let path = scratch_config_path(test_name);
        config::save_to(&path, &config).unwrap();
        let mut app = settings_app_with(config::load_from(&path));
        app.config_path = Some(path.clone());
        (app, path)
    }

    #[test]
    fn save_edit_writes_only_the_edited_field() {
        let path = scratch_config_path("writes-only-edited-field");
        config::save_to(&path, &Config::default()).unwrap();

        let load = || config::load_from(&path);
        let save = |cfg: &Config| config::save_to(&path, cfg);
        let result = save_edit(&load, &save, |target| target.low_threshold = 15).unwrap();

        assert_eq!(result.low_threshold, 15);
        assert_eq!(
            result.poll_interval_secs,
            Config::default().poll_interval_secs
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A field `edit` does not name — here `primary_device`, which no UI
    /// control ever sets — must survive untouched. Replaces the old
    /// `apply_to_preserves_primary_device`, which tested the same guarantee
    /// against the blanket-overwrite `apply_to` this function replaces.
    #[test]
    fn save_edit_preserves_fields_it_does_not_touch() {
        let path = scratch_config_path("preserves-untouched-fields");
        config::save_to(
            &path,
            &Config {
                primary_device: Some("mouse".to_string()),
                ..Config::default()
            },
        )
        .unwrap();

        let load = || config::load_from(&path);
        let save = |cfg: &Config| config::save_to(&path, cfg);
        let result = save_edit(&load, &save, |target| target.low_threshold = 10).unwrap();

        assert_eq!(result.primary_device, Some("mouse".to_string()));
        assert_eq!(result.low_threshold, 10);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Regression test for C1: a settings window's own `self.config` snapshot
    /// is never fed to `save_edit` — only `load_config`'s fresh read is. This
    /// reproduces the failing sequence directly: the window "opens" (saved
    /// once with `hidden_devices` empty), something else — the tray's
    /// one-time `shown_devices` migration, in this test's role — writes
    /// `hidden_devices` afterward, and the window then saves an unrelated
    /// field. The migrated value must survive.
    #[test]
    fn save_edit_stale_window_snapshot_does_not_clobber_concurrent_disk_write() {
        let path = scratch_config_path("stale-snapshot");
        config::save_to(&path, &Config::default()).unwrap();

        // The window would have opened here, holding hidden_devices == [].
        // It is never consulted below — only load_config is.

        config::save_to(
            &path,
            &Config {
                hidden_devices: vec!["keyboard".to_string()],
                ..Config::default()
            },
        )
        .unwrap();

        let load = || config::load_from(&path);
        let save = |cfg: &Config| config::save_to(&path, cfg);
        let result =
            save_edit(&load, &save, |target| target.notifications_enabled = false).unwrap();

        assert_eq!(result.hidden_devices, vec!["keyboard".to_string()]);
        assert!(!result.notifications_enabled);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn save_edit_refuses_to_write_over_a_config_that_does_not_parse() {
        let path = scratch_config_path("broken-on-disk");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let broken = "{ \"hidden_devices\": [\"mouse\"], oops }";
        std::fs::write(&path, broken).unwrap();

        let load = || config::load_from(&path);
        let save = |cfg: &Config| config::save_to(&path, cfg);
        let err = save_edit(&load, &save, |target| target.low_threshold = 15).unwrap_err();

        assert!(err.is::<config::Unreadable>(), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn apply_scan_result_preserves_hidden_devices_and_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mouse".to_string(),
            DeviceSettings {
                poll_interval_secs: Some(30),
                low_threshold: Some(10),
            },
        );
        let mut app = settings_app_with(Config {
            hidden_devices: vec!["mouse".to_string()],
            device_overrides: overrides.clone(),
            ..Config::default()
        });
        app.apply_scan_result(scan_result(vec![device("mouse"), device("keyboard")]));
        assert_eq!(app.config.hidden_devices, vec!["mouse".to_string()]);
        assert_eq!(app.config.device_overrides, overrides);
        assert!(app.scanning.is_none());
        assert!(app.scan_rx.is_none());
    }

    #[test]
    fn apply_scan_result_empty_hidden_devices_survives_device_set_change() {
        // hidden_devices == [] is "show all"; it must not flip to a concrete
        // list just because the discovered set changed.
        let mut app = settings_app_with(Config::default());
        app.apply_scan_result(scan_result(vec![device("mouse")]));
        assert!(app.config.hidden_devices.is_empty());
        app.apply_scan_result(scan_result(vec![device("mouse"), device("keyboard")]));
        assert!(app.config.hidden_devices.is_empty());
    }

    #[test]
    fn apply_scan_result_keeps_override_for_device_that_disappeared() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "headset".to_string(),
            DeviceSettings {
                poll_interval_secs: None,
                low_threshold: Some(5),
            },
        );
        let mut app = settings_app_with(Config {
            device_overrides: overrides.clone(),
            ..Config::default()
        });
        // "headset" is not in this scan's result.
        app.apply_scan_result(scan_result(vec![device("mouse")]));
        assert_eq!(app.config.device_overrides, overrides);
        assert!(!app.discovered.iter().any(|s| s.info.name == "headset"));
    }

    #[test]
    fn a_scan_the_tray_did_not_answer_keeps_the_live_rows() {
        let mut app = settings_app_with(Config::default());
        app.apply_scan_result(scan_result(vec![device("mouse")]));

        app.apply_scan_result(ScanResult {
            discovered: Err(anyhow::anyhow!("no answer")),
            records: Vec::new(),
        });

        let names: Vec<&str> = app
            .device_rows
            .iter()
            .map(|r| r.device.name.as_str())
            .collect();
        assert_eq!(names, ["mouse"]);
        assert!(app.tray_unanswered);
        app.apply_scan_result(scan_result(vec![device("mouse")]));
        assert!(!app.tray_unanswered);
    }

    fn commands(output: &egui::FullOutput) -> Vec<egui::ViewportCommand> {
        output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default()
    }

    fn raises(commands: &[egui::ViewportCommand]) -> bool {
        commands.contains(&egui::ViewportCommand::Focus)
            && commands.contains(&egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ))
    }

    #[test]
    fn a_raise_before_the_window_exists_is_applied_when_it_opens() {
        let raiser = Raiser::default();
        raiser.raise();
        assert!(raiser.pending.load(Ordering::SeqCst));

        let window = egui::Context::default();
        raiser.attach(&window);
        let output = crate::egui_test::run_frame(&window, WINDOW_DEFAULT_SIZE, Vec::new(), |_| {});
        assert!(raises(&commands(&output)), "{:?}", commands(&output));
        assert!(!raiser.pending.load(Ordering::SeqCst));
    }

    #[test]
    fn a_window_opened_without_a_raise_is_left_alone() {
        let raiser = Raiser::default();
        let window = egui::Context::default();
        raiser.attach(&window);
        let output = crate::egui_test::run_frame(&window, WINDOW_DEFAULT_SIZE, Vec::new(), |_| {});
        assert!(!raises(&commands(&output)));
    }

    #[test]
    fn a_raise_after_the_window_opened_raises_it_at_once() {
        let raiser = Raiser::default();
        let window = egui::Context::default();
        raiser.attach(&window);
        raiser.raise();
        let output = crate::egui_test::run_frame(&window, WINDOW_DEFAULT_SIZE, Vec::new(), |_| {});
        assert!(raises(&commands(&output)));
        assert!(!raiser.pending.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod bus_tests {
    use std::time::{Duration, Instant};

    use eframe::egui;
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::{OwnedValue, Value};

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{Raiser, Startup, raise_running, runtime, start};
    use crate::appearance::ColorScheme;
    use crate::bus_test::isolated;
    use crate::gui;

    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    const APPEARANCE: &str = "org.freedesktop.appearance";
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Answers only `color-scheme`, with "prefer light".
    struct FakePortal;

    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl FakePortal {
        fn read(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
            match (namespace, key) {
                (APPEARANCE, "color-scheme") => Ok(OwnedValue::from(2u32)),
                _ => Err(zbus::fdo::Error::Failed(format!("no {namespace} {key}"))),
            }
        }

        #[zbus(property, name = "version")]
        fn version(&self) -> u32 {
            1
        }

        #[zbus(signal)]
        async fn setting_changed(
            emitter: &SignalEmitter<'_>,
            namespace: &str,
            key: &str,
            value: Value<'_>,
        ) -> zbus::Result<()>;
    }

    /// Called from the test thread, which no runtime has entered — as `run` calls it.
    #[test]
    fn start_follows_the_portal_from_a_plain_thread() {
        if !isolated(
            module_path!(),
            "start_follows_the_portal_from_a_plain_thread",
        ) {
            return;
        }
        let portal_rt = tokio::runtime::Runtime::new().expect("portal runtime");
        let portal = portal_rt
            .block_on(async {
                zbus::connection::Builder::session()?
                    .name("org.freedesktop.portal.Desktop")?
                    .serve_at(PORTAL_PATH, FakePortal)?
                    .build()
                    .await
            })
            .expect("fake portal");

        let rt = runtime().expect("runtime");
        let (_bus, appearance) = match start(&rt, Raiser::default()) {
            Startup::Started { bus, appearance } => (bus, Some(appearance)),
            Startup::AlreadyRunning => (None, None),
        };
        let appearance =
            appearance.expect("no other settings window holds the name on the private bus");
        assert_eq!(appearance.borrow().scheme, ColorScheme::Light);

        let window = egui::Context::default();
        let (_, theme) = tokio::sync::watch::channel(crate::domain::WindowTheme::System);
        gui::follow(rt.handle(), window.clone(), appearance, theme);
        let dark = || window.options(|o| o.theme_preference) == egui::ThemePreference::Dark;
        let deadline = Instant::now() + TIMEOUT;
        // The follower subscribes after `start` returns, so an early signal can go unheard.
        while !dark() {
            assert!(Instant::now() < deadline, "the window never turned dark");
            portal_rt
                .block_on(async {
                    let emitter = SignalEmitter::new(&portal, PORTAL_PATH)?;
                    FakePortal::setting_changed(&emitter, APPEARANCE, "color-scheme", Value::U32(1))
                        .await
                })
                .expect("SettingChanged");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Called from the test thread, which no runtime has entered — as `run` calls it.
    #[test]
    fn a_second_start_raises_the_first_from_a_plain_thread() {
        if !isolated(
            module_path!(),
            "a_second_start_raises_the_first_from_a_plain_thread",
        ) {
            return;
        }
        let rt = runtime().expect("runtime");
        let pending = Arc::new(AtomicBool::new(false));
        let first = start(
            &rt,
            Raiser {
                window: Arc::default(),
                pending: pending.clone(),
            },
        );
        assert!(
            matches!(first, Startup::Started { bus: Some(_), .. }),
            "the first launch owns the name"
        );

        let second = start(&rt, Raiser::default());
        assert!(matches!(second, Startup::AlreadyRunning));
        rt.block_on(raise_running());
        assert!(
            pending.load(Ordering::SeqCst),
            "Raise reached the first window"
        );

        let _entered = rt.enter();
        drop(first);
    }
}
