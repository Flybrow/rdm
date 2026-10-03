//! Presentation layer. `theme` (tokens, fonts) → `widgets` (atoms) → `chrome` (sidebar, header,
//! dashboard), `card` (a download), `dialogs`, `toast` → `App`.

mod browsers;
mod card;
mod chrome;
mod dialogs;
mod edit;
mod theme;
mod toast;
mod widgets;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use domain::{Category, DownloadId, Status};
use eframe::{
    UserEvent,
    egui::{
        self, CentralPanel, ColorImage, Context, Frame, Key, LayerId, Margin, Pos2, TextureHandle, TextureOptions,
        ThemePreference, Vec2, ViewportBuilder, ViewportCommand, vec2,
    },
};
use egui_phosphor::regular as icon;
use url::Url;
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::run_on_demand::EventLoopExtRunOnDemand,
    window::WindowId,
};

use crate::{
    manager::{AddRequest, Entry, Manager, Scan, ScanRefused, Stats},
    settings::{Queue, Settings, Theme},
    tr, trf,
    shell::{Shell, Wake},
    tray::{self, Tray},
    window::{self, Window},
};
use theme::Palette;
use toast::Toasts;
use widgets::speed;

const ICON_RGBA: &[u8] = include_bytes!("../../assets/icon64.rgba");
const ICON_SIZE: u32 = 64;
/// Windows draws notification-area icons at 16–32 px: a 32 px render of the bolder small icon.
#[cfg(windows)]
const TRAY_ICON: (&[u8], u32) = (include_bytes!("../../assets/tray32.rgba"), 32);
#[cfg(not(windows))]
const TRAY_ICON: (&[u8], u32) = (ICON_RGBA, ICON_SIZE);
const LOGO_RGBA: &[u8] = include_bytes!("../../assets/logo128.rgba");
/// Frame rate while something moves (bars, spinners, toasts); nothing is redrawn when idle.
const ANIMATION_FRAME: Duration = Duration::from_millis(33);
/// Frame rate while only progress changes: 15 per second with the window in front, 2 behind it
/// (numbers still move; the processor and the battery are spared).
const PROGRESS_FRAME: Duration = Duration::from_millis(66);
const BACKGROUND_FRAME: Duration = Duration::from_millis(500);
/// Settings are written once the user stops fiddling (sliders fire every frame).
const SAVE_DEBOUNCE: Duration = Duration::from_millis(600);
/// Windowless (in the tray only): how often the tooltip's transfer summary is refreshed.
const TRAY_REFRESH: Duration = Duration::from_secs(1);
/// "Quitter" in the tray first asks the window to close. A window that cannot run that frame (a
/// renderer stuck in the graphics driver) must not keep RDM alive: past this delay, RDM saves
/// everything and leaves anyway.
const QUIT_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_SIZE: Vec2 = vec2(1180.0, 740.0);
const MIN_SIZE: Vec2 = vec2(900.0, 560.0);
/// Frame pacing when rendering without vsync (see `self_paced`): one frame per 60 Hz refresh.
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// Wayland — the backend winit picks when either variable is set and non-empty. There, Mesa's vsync
/// waits for the compositor's frame callback, which never comes while the window is minimized or on
/// another workspace: the UI thread would hang until the window is shown again, deaf to the tray,
/// the browser and "Quitter". So no vsync there; `App::pace` spaces frames out instead.
fn self_paced() -> bool {
    cfg!(unix) && ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"].iter().any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

/// Runs the UI until the user quits, then shuts the manager down (downloads saved, resumable).
///
/// Closing the window to the tray *destroys* it, on every system: Wayland cannot hide a window (a
/// minimized one stays in the taskbar), and on Windows a hidden window never gets painted, which
/// left eframe's event loop polling at 100 % CPU for as long as RDM sat in the tray. The event loop
/// itself runs on without a window (see `Idle`) and "Ouvrir" builds a fresh window.
pub fn run(manager: Arc<Manager>, minimized: bool) -> eframe::Result {
    // The process's one event loop (winit cannot build a second): windows come and go on it. Built
    // first: on Windows this makes the process DPI-aware, which the tray icon's menu needs to be sharp.
    let mut event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let shell = Arc::new(Shell::default());
    shell.set_waker(event_loop.create_proxy());
    manager.on_change({
        let shell = shell.clone();
        move || shell.repaint()
    });
    manager.on_show({
        let shell = shell.clone();
        move || shell.show()
    });
    manager.on_quit({
        let (shell, manager) = (shell.clone(), Arc::downgrade(&manager));
        move || {
            shell.quit();
            if let Some(manager) = manager.upgrade() {
                quit_watchdog(manager);
            }
        }
    });
    let mut tray = create_tray(&shell, &manager);

    // Survives the window: what the user typed and chose, and where the window was.
    let mut memo = Memo::default();
    // Started at login: with a notification area, the icon only; without one, a minimized window
    // (the taskbar is then the only way back).
    let mut windowless = minimized && tray.is_some();
    let mut start_minimized = minimized && tray.is_none();
    let result = loop {
        if windowless {
            let mut idle = Idle { shell: &shell, manager: &manager, tray: tray.as_mut(), wake: None };
            let ran = event_loop.run_app_on_demand(&mut idle);
            match (ran, idle.wake) {
                (Err(e), _) => break Err(e.into()),
                (Ok(()), Some(Wake::Show)) => {}
                (Ok(()), _) => break Ok(()),
            }
        }
        let result = open_window(&mut event_loop, &manager, &shell, tray.as_mut(), &mut memo, start_minimized);
        start_minimized = false;
        let keep_running = tray.is_some() && !shell.quitting() && manager.with_settings(|s| s.close_to_tray);
        if result.is_err() || !keep_running {
            break result;
        }
        windowless = true;
    };

    manager.shutdown();
    if let Some(tray) = tray {
        tray.close();
    }
    result
}

/// The event loop while RDM sits in the tray without a window. It keeps running — blocked, not
/// polling — so the platform still delivers the tray icon's messages (Windows) and the display
/// connection is served (Linux); its first turn also completes the destruction of the window that
/// just closed (on Wayland winit only *marks* a dropped window closed until the loop's next turn:
/// it stayed on screen and in the taskbar meanwhile). Woken up by the shell ("Ouvrir", "Quitter",
/// browser, second launch), and once per `TRAY_REFRESH` for the tooltip.
struct Idle<'a> {
    shell: &'a Shell,
    manager: &'a Manager,
    tray: Option<&'a mut Tray>,
    wake: Option<Wake>,
}

impl ApplicationHandler<UserEvent> for Idle<'_> {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, _: StartCause) {
        if let Some(wake) = self.shell.take_pending() {
            self.wake = Some(wake);
            event_loop.exit();
            return;
        }
        if let Some(tray) = self.tray.as_deref_mut() {
            tray.set_tooltip(tray_summary(self.manager.stats()));
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TRAY_REFRESH));
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn create_tray(shell: &Arc<Shell>, manager: &Arc<Manager>) -> Option<Tray> {
    let (shell, manager) = (shell.clone(), manager.clone());
    let (rgba, size) = TRAY_ICON;
    tray::create(rgba.to_vec(), size, move |command| match command {
        tray::Command::Show => shell.show(),
        tray::Command::PauseAll => manager.pause_all(),
        tray::Command::ResumeAll => manager.resume_all(),
        tray::Command::Quit => manager.request_quit(),
    })
}

/// Last resort behind the tray's "Quitter" (see `QUIT_GRACE`). Stands down as soon as the normal
/// exit path has begun shutting the manager down.
fn quit_watchdog(manager: Arc<Manager>) {
    std::thread::spawn(move || {
        std::thread::sleep(QUIT_GRACE);
        if !manager.is_closing() {
            manager.shutdown();
            exit_now();
        }
    });
}

/// Ends the process on the spot, everything being saved already. Not `std::process::exit`: that
/// runs the graphics driver's exit hooks, and the stuck UI thread may be blocked inside the driver
/// holding the very locks those hooks wait for — the process would then never end.
fn exit_now() -> ! {
    #[cfg(unix)]
    // SAFETY: `_exit` only terminates the process; no Rust state is observed afterwards.
    unsafe {
        libc::_exit(0)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        // SAFETY: the pseudo-handle of our own process; terminating it does not return.
        unsafe { TerminateProcess(GetCurrentProcess(), 0) };
        std::process::exit(0)
    }
}

/// Runs one window until it closes.
fn open_window(
    event_loop: &mut EventLoop<UserEvent>,
    manager: &Arc<Manager>,
    shell: &Arc<Shell>,
    tray: Option<&mut Tray>,
    memo: &mut Memo,
    minimized: bool,
) -> eframe::Result {
    let icon = egui::IconData { rgba: ICON_RGBA.to_vec(), width: ICON_SIZE, height: ICON_SIZE };
    let paced = self_paced();
    let mut viewport = ViewportBuilder::default()
        .with_title("RDM")
        .with_app_id("rdm")
        .with_icon(icon)
        .with_inner_size(memo.size.unwrap_or(DEFAULT_SIZE))
        .with_min_inner_size(MIN_SIZE)
        .with_maximized(memo.maximized);
    // Never recorded on Wayland (no window positions there).
    if let Some((position, _)) = memo.position.filter(|&(p, ppp)| window::on_screen(p.x * ppp, p.y * ppp)) {
        viewport = viewport.with_position(position);
    }
    let options = eframe::NativeOptions { viewport, vsync: !paced, ..Default::default() };
    let mut created = false;
    let opened = &mut created;
    let mut app = eframe::create_native(
        "RDM",
        options,
        Box::new(move |cc| {
            *opened = true;
            let ctx = cc.egui_ctx.clone();
            let window = Window::of(cc);
            theme::install(&ctx);
            ctx.set_theme(preference(manager.with_settings(|s| s.theme)));
            shell.attach(&ctx, window);
            if minimized {
                ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
            }
            let logo = ctx.load_texture("logo", ColorImage::from_rgba_unmultiplied([128, 128], LOGO_RGBA), TextureOptions::LINEAR);
            Ok(Box::new(App::new(manager.clone(), shell.clone(), tray, memo, logo, paced, window)))
        }),
        event_loop,
    );
    event_loop.run_app_on_demand(&mut app)?;
    drop(app);
    // The window (or its OpenGL context) could not be created: nothing to reopen later either.
    if created { Ok(()) } else { Err(eframe::Error::AppCreation(tr!("la fenêtre n'a pas pu être créée", "the window could not be created").into())) }
}

/// Tooltip text while something downloads; `None` when idle.
fn tray_summary(stats: Stats) -> Option<String> {
    (stats.running > 0).then(|| {
        let pct = stats.done.saturating_mul(100).checked_div(stats.total).map(|p| format!(" · {p} %")).unwrap_or_default();
        let (n, rate) = (stats.running, speed(stats.speed));
        let n = crate::i18n::count(n as u64, ("actif", "actifs"), ("active", "active"));
        format!("RDM — {n} · {rate}{pct}")
    })
}

/// A size in the interface's units ("1,5 Mo" / "1.5 MB"), for messages built outside the UI.
pub(crate) fn size_text(n: u64) -> String {
    widgets::bytes(n)
}

fn preference(theme: Theme) -> ThemePreference {
    match theme {
        Theme::System => ThemePreference::System,
        Theme::Dark => ThemePreference::Dark,
        Theme::Light => ThemePreference::Light,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Active,
    Queued,
    Done,
    Failed,
    Kind(Category),
    /// A named queue (0 = the main one).
    Queue(u32),
}

impl Filter {
    fn accepts(self, e: &Entry) -> bool {
        let status = e.download.status();
        match self {
            Self::All => true,
            Self::Active => *status == Status::Running,
            Self::Queued => matches!(status, Status::Queued | Status::Paused),
            Self::Done => *status == Status::Completed,
            Self::Failed => matches!(status, Status::Failed(_)),
            Self::Kind(c) => e.category == c,
            Self::Queue(q) => e.download.queue == q && !e.download.is_recording(),
        }
    }

    fn title(self, queues: &[Queue]) -> String {
        match self {
            Self::All => tr!("Tous les téléchargements", "All downloads").into(),
            Self::Active => tr!("En cours", "Active").into(),
            Self::Queued => tr!("En attente", "Waiting").into(),
            Self::Done => tr!("Terminés", "Completed").into(),
            Self::Failed => tr!("Échecs", "Failed").into(),
            Self::Kind(c) => crate::i18n::category(c).into(),
            Self::Queue(q) => queue_name(queues, q),
        }
    }
}

/// A queue's name as shown (the main queue has no name of its own).
fn queue_name(queues: &[Queue], queue: u32) -> String {
    match queues.iter().find(|q| q.id == queue) {
        Some(q) if !q.name.is_empty() => q.name.clone(),
        Some(q) => {
            let id = q.id;
            trf!("File {id}", "Queue {id}", id = id)
        }
        None => tr!("File principale", "Main queue").into(),
    }
}

/// Sidebar: the status filters, then one entry per category.
const STATUS_FILTERS: usize = 5;
const NAV: [(Filter, &str); STATUS_FILTERS + Category::ALL.len()] = [
    (Filter::All, icon::STACK),
    (Filter::Active, icon::ARROW_CIRCLE_DOWN),
    (Filter::Queued, icon::HOURGLASS),
    (Filter::Done, icon::CHECK_CIRCLE),
    (Filter::Failed, icon::WARNING_CIRCLE),
    (Filter::Kind(Category::ALL[0]), category_icon(Category::ALL[0])),
    (Filter::Kind(Category::ALL[1]), category_icon(Category::ALL[1])),
    (Filter::Kind(Category::ALL[2]), category_icon(Category::ALL[2])),
    (Filter::Kind(Category::ALL[3]), category_icon(Category::ALL[3])),
    (Filter::Kind(Category::ALL[4]), category_icon(Category::ALL[4])),
    (Filter::Kind(Category::ALL[5]), category_icon(Category::ALL[5])),
];

const fn category_icon(c: Category) -> &'static str {
    match c {
        Category::Video => icon::FILM_STRIP,
        Category::Music => icon::MUSIC_NOTES,
        Category::Archive => icon::FILE_ZIP,
        Category::Program => icon::APP_WINDOW,
        Category::Document => icon::FILE_TEXT,
        Category::Other => icon::FILE,
    }
}

enum Action {
    Resume(DownloadId),
    Pause(DownloadId),
    Remove(DownloadId, bool),
    Open(PathBuf),
    Reveal(PathBuf),
    /// Text to copy, and what it is (for the confirmation).
    Copy(String, &'static str),
    /// Opens a small editor on one download (new link, speed limit, checksum).
    Edit(DownloadId, edit::Field),
    SetInsecure(DownloadId, bool),
    MoveToQueue(DownloadId, u32),
    Hash(DownloadId),
    Scan(DownloadId),
    ShowReport(DownloadId),
    PauseAll,
    ResumeAll,
    ClearCompleted,
    OpenSettings,
    CheckUpdates,
    InstallUpdate,
    OpenBrowsers,
}

/// UI state that outlives a window (the tray reopens a fresh one): what the user typed and chose,
/// and the window's geometry.
struct Memo {
    url: String,
    search: String,
    filter: Filter,
    size: Option<Vec2>,
    /// Outer top-left corner, in points, with the pixels-per-point it was measured at.
    position: Option<(Pos2, f32)>,
    maximized: bool,
}

impl Default for Memo {
    fn default() -> Self {
        Self { url: String::new(), search: String::new(), filter: Filter::All, size: None, position: None, maximized: false }
    }
}

impl Memo {
    /// Remembers the window's size, position and maximized state. A minimized window reports a
    /// meaningless geometry (0 × 0 at −32000 on Windows): ignored, like the maximized size and position.
    /// Not `inner_rect` for the size: egui leaves it empty on Wayland (no window positions there).
    fn keep_geometry(&mut self, ctx: &Context) {
        let (minimized, maximized, outer) =
            ctx.input(|i| (i.viewport().minimized == Some(true), i.viewport().maximized == Some(true), i.viewport().outer_rect));
        if minimized {
            return;
        }
        self.maximized = maximized;
        let size = ctx.screen_rect().size();
        if !maximized && size.x >= MIN_SIZE.x && size.y >= MIN_SIZE.y {
            self.size = Some(size);
            self.position = outer.map(|r| (r.min, ctx.pixels_per_point()));
        }
    }
}

struct App<'a> {
    window: Window,
    /// The theme the title bar was last painted for.
    title_bar_dark: Option<bool>,
    manager: Arc<Manager>,
    shell: Arc<Shell>,
    tray: Option<&'a mut Tray>,
    memo: &'a mut Memo,
    logo: TextureHandle,
    /// Settings dialog draft, while open.
    settings: Option<Settings>,
    /// Passwords edited in the settings (proxy, site logins), while open.
    secrets: Option<crate::secrets::Secrets>,
    unsaved_since: Option<Instant>,
    /// Reveal the VirusTotal key in the settings.
    show_key: bool,
    /// The settings tab shown (the last one used, while RDM runs).
    settings_tab: dialogs::SettingsTab,
    /// The settings were opened to ask for the VirusTotal key.
    asking_key: bool,
    /// The browser-extension window, while open.
    browsers: Option<browsers::Browsers>,
    /// Download whose VirusTotal report is open.
    report: Option<DownloadId>,
    /// The small editor open on one download, if any.
    edit: Option<edit::Editor>,
    /// "Delete the file" asked: waiting for the user's confirmation.
    confirm_delete: Option<DownloadId>,
    /// "Don't ask again" ticked in the browser download's confirmation.
    confirm_always: bool,
    /// An update was just installed: its release notes are shown.
    changelog: bool,
    toasts: Toasts,
    /// Something on screen moves this frame (progress, spinner): keep redrawing.
    animating: bool,
    /// Frames still to draw right away after a dialog or menu closed.
    settle_frames: u8,
    /// The language the fonts were installed for (see `theme::install_fonts`).
    fonts_language: crate::i18n::Language,
    /// Rendering without vsync: frames are spaced out by `pace`.
    paced: bool,
    last_frame: Option<Instant>,
}

const SEARCH_ID: &str = "search";
const ADD_ID: &str = "add-url";

impl<'a> App<'a> {
    fn new(
        manager: Arc<Manager>,
        shell: Arc<Shell>,
        tray: Option<&'a mut Tray>,
        memo: &'a mut Memo,
        logo: TextureHandle,
        paced: bool,
        window: Window,
    ) -> Self {
        let mut app = Self {
            window,
            title_bar_dark: None,
            manager,
            shell,
            tray,
            memo,
            logo,
            settings: None,
            secrets: None,
            unsaved_since: None,
            show_key: false,
            settings_tab: dialogs::SettingsTab::default(),
            asking_key: false,
            report: None,
            browsers: None,
            edit: None,
            confirm_delete: None,
            confirm_always: false,
            changelog: false,
            toasts: Toasts::default(),
            animating: false,
            settle_frames: 0,
            fonts_language: crate::i18n::active(),
            paced,
            last_frame: None,
        };
        app.offer_changelog();
        app.offer_extension();
        app
    }

    /// First start of a new version: its release notes, unless RDM was just installed (first launch).
    /// RDM 0.3.11 and older did not note their version: having offered the extension says they ran.
    fn offer_changelog(&mut self) {
        let current = env!("CARGO_PKG_VERSION");
        let (last, ran_before) = self.manager.with_settings(|s| (s.last_version.clone(), s.extension_offered));
        if last == current {
            return;
        }
        self.changelog = if last.is_empty() { ran_before } else { true };
        let mut settings = self.manager.settings();
        settings.last_version = current.to_owned();
        self.manager.apply_settings(settings);
        self.manager.save_settings();
    }

    /// First launch without the extension anywhere: the extension window opens by itself, once.
    fn offer_extension(&mut self) {
        if self.manager.with_settings(|s| s.extension_offered) {
            return;
        }
        let mut settings = self.manager.settings();
        settings.extension_offered = true;
        self.manager.apply_settings(settings);
        self.manager.save_settings();
        if self.manager.browsers_seen().is_empty() {
            self.open_browsers();
        }
    }

    /// What vsync does, minus its hang (see `self_paced`): without it, moving the mouse would redraw
    /// at the mouse's polling rate, up to 1000 frames per second.
    fn pace(&mut self) {
        if !self.paced {
            return;
        }
        if let Some(wait) = self.last_frame.and_then(|t| FRAME_INTERVAL.checked_sub(t.elapsed())) {
            std::thread::sleep(wait);
        }
        self.last_frame = Some(Instant::now());
    }

    /// Adds every http(s) link found in `text`; tells how many.
    fn submit(&mut self, text: &str) -> usize {
        let urls: Vec<Url> = text
            .split_whitespace()
            .filter_map(|t| t.parse::<Url>().ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .collect();
        let n = urls.len();
        urls.into_iter().for_each(|u| self.manager.add(AddRequest::from_url(u)));
        match n {
            0 => self.toasts.warn(icon::LINK_BREAK, tr!("Aucun lien http(s) à télécharger", "No http(s) link to download")),
            1 => self.toasts.info(icon::DOWNLOAD_SIMPLE, tr!("Téléchargement ajouté", "Download added")),
            n => self.toasts.info(icon::DOWNLOAD_SIMPLE, &trf!("{n} téléchargements ajoutés", "{n} downloads added", n = n)),
        }
        n
    }

    fn open_settings(&mut self) {
        self.settings = Some(self.manager.settings());
        self.secrets = Some(self.manager.secrets());
    }

    /// Ctrl+V anywhere (no text field focused) adds the copied link(s), like IDM; Ctrl+F searches.
    fn shortcuts(&mut self, ctx: &Context) {
        if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::F)) {
            ctx.memory_mut(|m| m.request_focus(egui::Id::new(SEARCH_ID)));
        }
        if ctx.memory(|m| m.focused().is_some()) || self.settings.is_some() {
            return;
        }
        let pasted: Vec<String> = ctx.input(|i| {
            i.events.iter().filter_map(|e| if let egui::Event::Paste(t) = e { Some(t.clone()) } else { None }).collect()
        });
        for text in pasted {
            self.submit(&text);
        }
    }

    fn apply(&mut self, ctx: &Context, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Resume(id) => self.manager.resume(id),
                Action::Pause(id) => self.manager.pause(id),
                // Deleting a file cannot be undone: asked first (see `delete_dialog`).
                Action::Remove(id, true) => self.confirm_delete = Some(id),
                Action::Remove(id, false) => self.manager.remove(id, false),
                Action::Open(path) => open_file(path),
                Action::Reveal(path) => reveal_file(path),
                Action::Copy(text, what) => {
                    self.manager.copied_by_rdm(&text);
                    ctx.copy_text(text);
                    self.toasts.info(icon::COPY, &trf!("{what} copié", "{what} copied", what = what));
                }
                Action::Edit(id, field) => self.edit = edit::Editor::open(&self.manager, id, field),
                Action::SetInsecure(id, insecure) => {
                    self.manager.set_insecure(id, insecure);
                    if insecure {
                        self.toasts.warn(icon::WARNING, tr!("Certificat accepté pour ce téléchargement seulement", "Certificate accepted for this download only"));
                    }
                }
                Action::MoveToQueue(id, queue) => self.manager.move_to_queue(id, queue),
                Action::Hash(id) => self.manager.compute_sha256(id),
                Action::Scan(id) => match self.manager.scan_virustotal(id) {
                    Ok(()) => self.toasts.info(icon::SHIELD, tr!("Analyse VirusTotal lancée", "VirusTotal analysis started")),
                    Err(ScanRefused::NoKey) => {
                        self.open_settings();
                        self.asking_key = true;
                        self.toasts.warn(icon::KEY, tr!("Ajoutez d'abord votre clé API VirusTotal (gratuite)", "Add your (free) VirusTotal API key first"));
                    }
                    Err(ScanRefused::NotEligible) => {}
                },
                Action::ShowReport(id) => self.report = Some(id),
                Action::PauseAll => self.manager.pause_all(),
                Action::ResumeAll => self.manager.resume_all(),
                Action::ClearCompleted => {
                    let n = self.manager.view(|es| es.iter().filter(|e| *e.download.status() == Status::Completed).count());
                    self.manager.clear_completed();
                    if n > 0 {
                        let text = crate::i18n::count(
                            n as u64,
                            ("téléchargement terminé retiré de la liste", "téléchargements terminés retirés de la liste"),
                            ("completed download removed from the list", "completed downloads removed from the list"),
                        );
                        self.toasts.info(icon::BROOM, &text);
                    }
                }
                Action::OpenSettings => self.open_settings(),
                Action::OpenBrowsers => self.open_browsers(),
                Action::CheckUpdates => self.manager.check_updates(true),
                Action::InstallUpdate => self.install_update(),
            }
        }
    }

    /// Installs the update on offer, or opens its page when RDM cannot install it itself.
    fn install_update(&mut self) {
        if !self.manager.install_update()
            && let Some(release) = self.manager.update_state().release()
        {
            open_link(release.page.clone());
        }
    }

    /// Saves once edits have settled; schedules a wake-up so it happens even if nothing moves.
    fn flush_settings(&mut self, ctx: &Context) {
        match self.unsaved_since {
            Some(t) if t.elapsed() >= SAVE_DEBOUNCE => {
                self.manager.save_settings();
                self.unsaved_since = None;
            }
            Some(t) => ctx.request_repaint_after(SAVE_DEBOUNCE.saturating_sub(t.elapsed())),
            None => {}
        }
    }
}

impl eframe::App for App<'_> {
    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        // The title bar follows the theme (chosen, or the system's when it changes).
        let dark = ctx.style().visuals.dark_mode;
        if self.title_bar_dark != Some(dark) {
            self.title_bar_dark = Some(dark);
            let p = Palette::from_ctx(ctx);
            self.window.paint_title_bar(dark, p.bg, p.text, p.border);
        }
        if self.fonts_language != crate::i18n::active() {
            self.fonts_language = crate::i18n::active();
            theme::install_fonts(ctx);
        }
        self.shortcuts(ctx);
        let stats = self.manager.stats();
        let mut actions = Vec::new();
        // A dialog or menu closing: two more frames at once, without it. egui matches clicks
        // against the previous frame, and keeps a dialog's modal layer one frame longer: otherwise
        // the next click would still hit the closed dialog and be lost.
        let overlays = (self.settings.is_some(), self.report.is_some(), self.browsers.is_some(), self.edit.is_some(), self.confirm_delete.is_some(), self.changelog);
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.settle_frames = 2;
        }
        self.animating = false;

        for notice in self.manager.take_notices() {
            if notice.warning {
                self.toasts.warn(icon::WARNING, &notice.text);
            } else {
                self.toasts.info(icon::INFO, &notice.text);
            }
        }
        backdrop(ctx);
        self.sidebar(ctx, &mut actions);
        self.header(ctx);
        CentralPanel::default()
            .frame(Frame::new().inner_margin(Margin { left: 26, right: 26, top: 6, bottom: 0 }))
            .show(ctx, |ui| {
                self.dashboard(ui, stats);
                self.clipboard_banner(ui);
                self.list(ui, &mut actions);
            });
        self.settings_dialog(ctx);
        self.report_dialog(ctx, &mut actions);
        self.browsers_dialog(ctx);
        self.edit_dialog(ctx);
        self.delete_dialog(ctx);
        self.firefox_prompt(ctx);
        self.confirm_prompt(ctx);
        self.changelog_dialog(ctx);
        self.apply(ctx, actions);
        if overlays != (self.settings.is_some(), self.report.is_some(), self.browsers.is_some(), self.edit.is_some(), self.confirm_delete.is_some(), self.changelog) {
            self.settle_frames = 2;
        }
        if self.settle_frames > 0 {
            self.settle_frames -= 1;
            ctx.request_repaint();
        }
        let toasts = self.toasts.show(ctx);

        if let Some(tray) = self.tray.as_deref_mut() {
            tray.relabel();
            tray.set_tooltip(tray_summary(stats));
        }
        self.flush_settings(ctx);
        // Animations at full rate; progress alone at a lower one, lower still behind other windows.
        if self.animating || toasts {
            ctx.request_repaint_after(ANIMATION_FRAME);
        } else if stats.running > 0 {
            let focused = ctx.input(|i| i.focused);
            ctx.request_repaint_after(if focused { PROGRESS_FRAME } else { BACKGROUND_FRAME });
        }
        self.memo.keep_geometry(ctx);
        self.pace();
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    /// The window is going away (quit, or closed to the tray): the manager lives on.
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        self.shell.detach();
        self.manager.save_settings();
    }
}

/// Window background: the base colour and two soft lights of the brand gradient ("aurora").
fn backdrop(ctx: &Context) {
    let p = Palette::from_ctx(ctx);
    let screen = ctx.screen_rect();
    let painter = ctx.layer_painter(LayerId::background());
    painter.rect_filled(screen, 0, p.bg);
    let strength = if p.dark { 0.13 } else { 0.10 };
    widgets::glow(&painter, screen.right_top() + vec2(-260.0, -40.0), vec2(620.0, 360.0), p.accent2.gamma_multiply(strength));
    widgets::glow(&painter, screen.left_top() + vec2(520.0, -80.0), vec2(560.0, 320.0), p.accent.gamma_multiply(strength));
}

/// Opens a finished file with its default application.
///
/// Linux: off the UI thread, and `xdg-open` is waited for there, so it is reaped instead of
/// lingering as a zombie process until RDM exits (the `opener` crate never waits for it).
fn open_file(path: PathBuf) {
    #[cfg(windows)]
    let _ = opener::open(path);
    #[cfg(not(windows))]
    std::thread::spawn(move || {
        use std::process::{Command, Stdio};
        match Command::new("xdg-open").arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(_) => {
                let _ = opener::open(&path);
            }
        }
    });
}

/// Shows a file in the file manager. Linux: a D-Bus call (connection set-up included), off the UI thread.
pub(super) fn reveal_file(path: PathBuf) {
    #[cfg(windows)]
    let _ = opener::reveal(path);
    #[cfg(not(windows))]
    std::thread::spawn(move || {
        let _ = opener::reveal(path);
    });
}

/// Opens a web page (VirusTotal key, full report) in the default browser.
fn open_link(url: String) {
    std::thread::spawn(move || {
        let _ = opener::open_browser(url);
    });
}

fn scan_running(e: &Entry) -> bool {
    matches!(e.scan, Scan::Running(_))
}
