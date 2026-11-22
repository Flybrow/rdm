//! Application service: owns the queues, schedules downloads and orchestrates the engine.

pub mod checksum;
pub mod clipboard;
mod add;
mod browsers;
mod control;
mod entry;
mod files;
mod online;
mod preferences;
mod recording;
mod routes;
mod scheduler;
mod ticker;
mod windows;

pub use add::{AddRequest, ToConfirm, header_map};
pub use browsers::{Install, Installed};
pub use entry::{Entry, Retry, Scan, ScanRefused, Verify};
pub use recording::Track;
pub use routes::RouteError;
pub use ticker::Stats;
pub use windows::{Choice, DownloadWindow, Inspected, Inspection, Ticket};
use browsers::{BROWSERS_FILE, CUSTOM_BROWSERS_FILE, FirefoxPairing, UNINSTALLS_FILE};
use entry::{Launch, load_entries, to_header_map};
use files::{leftovers, mark_from_internet, planned, remove_recording_leftovers, target_for, unique_path};
use recording::Recording;
use ticker::HISTORY;

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, Once, OnceLock, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*},
    },
    time::{Duration, Instant},
};

use domain::{Category, Download, DownloadId, Failure, Status};
use engine::{
    CancellationToken, Client, HeaderMap, HeaderValue, Outcome, Progress, RateLimit, header::HeaderName,
};
use serde::{Deserialize, Serialize};
use tokio::{runtime::Handle, sync::Semaphore};
use url::Url;

use crate::{
    extension::{self, Browser, Flavour},
    notify,
    secrets::Secrets,
    settings::{ExistingFile, Settings, load_json, save_json, with_suffix},
    tr, trf, update,
    virustotal::{self, Report, Stage},
};

const STORE: &str = "downloads.json";
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// Messages kept for the window while it is closed.
const MAX_NOTICES: usize = 20;

type Callback = Box<dyn Fn() + Send + Sync>;

pub struct Manager {
    rt: Handle,
    /// One client per route (proxy or not, certificate exemption), built on first use.
    clients: Mutex<HashMap<engine::ClientOptions, Client>>,
    /// Site logins and the proxy password (see `secrets`).
    secrets: Mutex<Secrets>,
    /// Links found in the clipboard, waiting for a click.
    offer: Mutex<Option<clipboard::Offer>>,
    /// Pasted or copied links whose file is already there, waiting for the user's answer.
    to_confirm: Mutex<VecDeque<AddRequest>>,
    /// Browser downloads waiting in their own window, and the windows to open (see `windows`).
    windows: Mutex<windows::Windows>,
    window_hook: OnceLock<Callback>,
    /// Text RDM copied itself (not offered back).
    own_copy: Mutex<Option<String>>,
    /// Short messages for the window (results of background actions).
    notices: Mutex<Vec<Notice>>,
    /// Quit to start the new version (Linux self-update): `main` launches it once all is saved.
    restart_after_exit: AtomicBool,
    settings: Mutex<Settings>,
    entries: Mutex<Vec<Entry>>,
    /// Held while a new download's file name is chosen: the disk is looked at outside the list's
    /// lock (the window reads the list at every frame), and two names chosen at once could
    /// otherwise be the same. Taken before `entries`, never while holding it.
    /// Removed downloads keep their names reserved here until their writers and cleanup finish.
    naming: Mutex<Vec<files::Planned>>,
    /// Downloads whose engine task is still alive (even if already paused): never start a second
    /// task on the same file, never delete files under a task that is still writing them.
    busy: Mutex<HashSet<DownloadId>>,
    /// Browser recordings in progress, by their secret token.
    recordings: Mutex<HashMap<String, Recording>>,
    limit: Arc<RateLimit>,
    /// Engine tasks and merges still writing files: shutdown waits (bounded) for them.
    inflight: AtomicUsize,
    repaint: OnceLock<Callback>,
    show: OnceLock<Callback>,
    quit: OnceLock<Callback>,
    firefox: Mutex<FirefoxPairing>,
    /// The list changed since it was last written. Writing rewrites the whole file, so it happens
    /// off the UI thread, at most once per `TICK` (and at shutdown), not on every change.
    dirty: AtomicBool,
    /// Snapshot numbering: an older snapshot never overwrites a newer one on disk.
    generation: AtomicU64,
    saved_generation: Mutex<u64>,
    settings_dirty: AtomicBool,
    /// The last write of RDM's own files failed (said once, until one succeeds).
    write_failing: AtomicBool,
    closing: AtomicBool,
    closed: Once,
    /// Total speed over the last minute, one sample per `TICK` (the UI's chart).
    history: Mutex<VecDeque<f32>>,
    /// Client for VirusTotal and GitHub, and the route it was built for.
    web: Mutex<Option<(engine::Route, reqwest::Client)>>,
    /// One VirusTotal analysis at a time: the free API allows 4 requests per minute.
    scan_gate: Arc<Semaphore>,
    update: Mutex<update::State>,
    /// Browsers the extension has talked from (key → Unix time): the extension window shows which
    /// ones are connected.
    browsers: Mutex<BTreeMap<String, u64>>,
    /// Browsers whose extension the user asked to remove (Unix time of the request): it uninstalls
    /// itself at its next check-in. Kept a day: past that, an extension heard from again is taken
    /// as reinstalled by hand.
    uninstalls: Mutex<BTreeMap<String, u64>>,
    installs: Mutex<HashMap<Browser, Install>>,
    /// Browsers the user added by their executable (portable ones, or any Windows does not list).
    custom_browsers: Mutex<Vec<PathBuf>>,
    /// Browsers RDM just opened on the extension's package or page, and when: the tab it opened is
    /// closed once the extension is installed (see `Manager::installed_by_rdm`).
    opened_to_install: Mutex<HashMap<String, Instant>>,
    /// RDM started the extension's installation in this browser on its own, and it takes more than
    /// the browser's own prompt: the extension window opens on it.
    extension_offer: Mutex<Option<Browser>>,
}

/// A message for the window (a toast): the outcome of something done in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub warning: bool,
    pub text: String,
}

impl Manager {
    /// `settings`: as loaded at start (their language already applied).
    pub fn new(rt: Handle, mut settings: Settings) -> Arc<Self> {
        if let Some(dir) = crate::settings::config_file(STORE).parent() {
            let _ = crate::settings::create_private_dir(dir);
        }
        let mut secrets = Secrets::load();
        let key_moved = secrets.adopt_virustotal_key(&mut settings, Secrets::save);
        let limit = Arc::new(RateLimit::default());
        limit.set(u64::from(settings.speed_limit_kib) * 1024);
        let this = Arc::new(Self {
            rt,
            clients: Mutex::default(),
            secrets: Mutex::new(secrets),
            offer: Mutex::default(),
            to_confirm: Mutex::default(),
            windows: Mutex::default(),
            window_hook: OnceLock::new(),
            own_copy: Mutex::default(),
            notices: Mutex::default(),
            restart_after_exit: AtomicBool::new(false),
            settings: Mutex::new(settings),
            entries: Mutex::new(load_entries()),
            naming: Mutex::default(),
            busy: Mutex::default(),
            recordings: Mutex::default(),
            limit,
            inflight: AtomicUsize::new(0),
            repaint: OnceLock::new(),
            show: OnceLock::new(),
            quit: OnceLock::new(),
            firefox: Mutex::new(FirefoxPairing { paired: FirefoxPairing::load(), ..FirefoxPairing::default() }),
            dirty: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            saved_generation: Mutex::new(0),
            settings_dirty: AtomicBool::new(key_moved),
            write_failing: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            closed: Once::new(),
            history: Mutex::new(std::iter::repeat_n(0.0, HISTORY).collect()),
            web: Mutex::default(),
            scan_gate: Arc::new(Semaphore::new(1)),
            update: Mutex::default(),
            browsers: Mutex::new(load_json(BROWSERS_FILE)),
            uninstalls: Mutex::new(load_json(UNINSTALLS_FILE)),
            installs: Mutex::default(),
            custom_browsers: Mutex::new(load_json(CUSTOM_BROWSERS_FILE)),
            opened_to_install: Mutex::default(),
            extension_offer: Mutex::default(),
        });
        this.save_settings();
        this.spawn_ticker();
        this.spawn_update_checks();
        this.spawn_clipboard_watch();
        this.schedule();
        this
    }

    /// The outcome of a write of RDM's own files (settings, the list, passwords): a failure is
    /// said once — a full or read-only disk fails every retry — until a write succeeds again.
    pub(crate) fn saved(&self, result: std::io::Result<()>) {
        match result {
            Ok(()) => self.write_failing.store(false, Release),
            Err(e) if !self.write_failing.swap(true, AcqRel) => {
                self.notice(true, &trf!("RDM ne peut pas enregistrer ses fichiers : {e}", "RDM cannot save its files: {e}", e = e));
            }
            Err(_) => {}
        }
    }

    /// A message for the window's next frame (kept, a few, while it is closed).
    pub(crate) fn notice(&self, warning: bool, text: &str) {
        let mut notices = lock(&self.notices);
        if notices.len() >= MAX_NOTICES {
            notices.remove(0);
        }
        notices.push(Notice { warning, text: text.to_owned() });
        drop(notices);
        self.repaint();
    }

    /// The messages not shown yet.
    pub fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(&mut *lock(&self.notices))
    }

    /// Quit to let the new version start (see `restart_after_exit`).
    pub fn restart_requested(&self) -> bool {
        self.restart_after_exit.load(Acquire)
    }

    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.repaint.set(Box::new(f));
    }

    /// How the bridge brings the window to the front (second launch, browser).
    pub fn on_show(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.show.set(Box::new(f));
    }

    pub fn show(&self) {
        if let Some(f) = self.show.get() {
            f();
        }
    }

    /// How the UI quits like the tray's "Quitter" when the system asks (session closing).
    pub fn on_quit(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.quit.set(Box::new(f));
    }

    pub fn request_quit(&self) {
        if let Some(f) = self.quit.get() {
            f();
        }
    }

    pub fn view<R>(&self, f: impl FnOnce(&[Entry]) -> R) -> R {
        f(&lock(&self.entries))
    }

    /// Stops every transfer, waits (bounded) for each to persist its resume state, then writes the
    /// list and the settings. Idempotent and callable from any thread: a second caller waits for
    /// the first to finish. Nothing new starts afterwards.
    pub fn shutdown(&self) {
        self.closing.store(true, SeqCst);
        self.closed.call_once(|| {
            lock(&self.entries).iter().filter_map(|e| e.cancel.as_ref()).for_each(CancellationToken::cancel);
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            while self.inflight.load(Acquire) > 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            self.dirty.store(true, Release);
            self.persist();
            self.save_settings();
        });
    }

    /// Shutdown has begun.
    pub fn is_closing(&self) -> bool {
        self.closing.load(SeqCst)
    }

    fn update<R>(&self, id: DownloadId, f: impl FnOnce(&mut Entry) -> R) -> Option<R> {
        let r = lock(&self.entries).iter_mut().find(|e| e.download.id == id).map(f);
        if r.is_some() {
            self.changed();
        }
        r
    }

    /// For transient state (scan progress): redraw, but nothing worth writing to disk.
    fn update_quiet<R>(&self, id: DownloadId, f: impl FnOnce(&mut Entry) -> R) -> Option<R> {
        let r = lock(&self.entries).iter_mut().find(|e| e.download.id == id).map(f);
        self.repaint();
        r
    }

    /// The list changed: redraw now, write soon (see `dirty`).
    fn changed(&self) {
        self.dirty.store(true, Release);
        self.repaint();
    }

    /// Writes the list if it changed since the last write.
    fn persist(&self) {
        if !self.dirty.swap(false, AcqRel) {
            return;
        }
        let (generation, stored) = {
            let entries = lock(&self.entries);
            (self.generation.fetch_add(1, Relaxed) + 1, entries.iter().map(Entry::stored).collect::<Vec<_>>())
        };
        let mut saved = lock(&self.saved_generation);
        if *saved >= generation {
            return;
        }
        let written = save_json(STORE, &stored);
        if written.is_ok() {
            *saved = generation;
        } else {
            self.dirty.store(true, Release);
        }
        drop(saved);
        self.saved(written);
    }

    fn repaint(&self) {
        if let Some(f) = self.repaint.get() {
            f();
        }
    }
}

/// Seconds since 1970 (what the files of the settings folder record).
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

