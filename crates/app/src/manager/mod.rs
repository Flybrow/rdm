//! Application service: owns the queues, schedules downloads and orchestrates the engine.

pub mod checksum;
pub mod clipboard;
mod add;
mod browsers;
mod entry;
mod files;
mod online;
mod recording;
mod routes;

pub use add::{AddRequest, ToConfirm, header_map};
pub use entry::{Entry, Retry, Scan, ScanRefused, Verify};
pub use recording::Track;
use browsers::FirefoxPairing;
use entry::{Launch, load_entries, to_header_map};
use files::{leftovers, mark_from_internet, planned, remove_recording_leftovers, target_for, unique_path};
use recording::Recording;

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

use domain::{Category, Download, DownloadId, Status};
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

const UPDATE_EVERY: Duration = Duration::from_secs(24 * 3600);

const STORE: &str = "downloads.json";
/// Speeds, idle recordings and the list on disk are refreshed at this pace.
const TICK: Duration = Duration::from_millis(500);
/// … and at this one once nothing has moved for `IDLE_AFTER` ticks.
const IDLE_TICK: Duration = Duration::from_secs(2);
const IDLE_AFTER: u32 = 4;
/// While transfers run, the list (with progress) is written every this many ticks (10 s).
const PROGRESS_SAVE_TICKS: u32 = 20;
/// Samples of the total speed kept for the chart: one minute.
pub const HISTORY: usize = 120;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// A recording that receives nothing for this long is considered abandoned.
const RECORDING_IDLE: Duration = Duration::from_secs(120);
/// A new link shows in the list at once; asking its server for the real file name takes at most this.
const NAME_TIMEOUT: Duration = Duration::from_secs(10);
/// The same link sent again within this window is the same request (double click, page retrying).
const DUPLICATE_WINDOW: Duration = Duration::from_secs(5);
/// Browser downloads waiting for the user's go-ahead at most (the rest are dropped).
const MAX_TO_CONFIRM: usize = 50;
/// Transient failures (network down, server busy) are retried on their own this many times in a
/// row without progress — about half an hour — before the download is reported as failed.
const AUTO_RETRIES: u32 = 15;

/// Automatic proxy mode: a download that runs below `SLOW_SPEED` (bytes/s) for `SLOW_FOR`, once
/// past its first `SLOW_GRACE`, switches to the proxy.
const SLOW_SPEED: f64 = 32.0 * 1024.0;
const SLOW_FOR: Duration = Duration::from_secs(20);
const SLOW_GRACE: Duration = Duration::from_secs(15);

/// 5 s, 10 s, 20 s, 40 s, 80 s, then every 2 minutes.
fn retry_delay(retries: u32) -> Duration {
    Duration::from_secs((5u64 << retries.min(5)).min(120))
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub running: usize,
    pub queued: usize,
    pub speed: f64,
    /// Bytes done / known total over running downloads (for the tray tooltip).
    pub done: u64,
    pub total: u64,
}

type Callback = Box<dyn Fn() + Send + Sync>;

pub struct Manager {
    rt: Handle,
    /// One client per route (proxy or not, certificate exemption), built on first use.
    clients: Mutex<HashMap<engine::ClientOptions, Client>>,
    /// Site logins and the proxy password (see `secrets`).
    secrets: Mutex<Secrets>,
    /// Links found in the clipboard, waiting for a click.
    offer: Mutex<Option<clipboard::Offer>>,
    /// Downloads sent by the browser, waiting for the user's go-ahead (`confirm_browser`).
    to_confirm: Mutex<VecDeque<AddRequest>>,
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
    naming: Mutex<()>,
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
}

const BROWSERS_FILE: &str = "browsers.json";
/// Extensions to remove (see `Manager::remove_extension`).
const UNINSTALLS_FILE: &str = "uninstalls.json";
const UNINSTALL_TTL_SECS: u64 = 24 * 3600;
/// Browsers added by hand (see `Manager::add_browser`).
const CUSTOM_BROWSERS_FILE: &str = "custom_browsers.json";
/// How long after RDM opened a browser to install the extension its tab is still taken as RDM's.
const INSTALL_TAB_TTL: Duration = Duration::from_secs(15 * 60);
/// Messages kept for the window while it is closed.
const MAX_NOTICES: usize = 20;

/// A message for the window (a toast): the outcome of something done in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub warning: bool,
    pub text: String,
}

/// Installing the extension into one browser, as the extension window shows it.
#[derive(Debug, Clone)]
pub enum Install {
    Working,
    Done(Installed),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Installed {
    /// The unpacked extension (Chromium-based browsers: "load unpacked" from here).
    pub folder: PathBuf,
    /// The browser was opened on its extensions page (or on the package to confirm).
    pub launched: bool,
    /// Firefox: opened on the extension's page in the Firefox store (addons.mozilla.org).
    pub store: bool,
    /// Firefox: the signed package is installing for good (Firefox asks to confirm).
    pub signed: bool,
    /// Firefox: the unsigned package, for the editions that accept one.
    pub xpi: Option<PathBuf>,
}

impl Manager {
    /// `settings`: as loaded at start (their language already applied).
    pub fn new(rt: Handle, mut settings: Settings) -> Arc<Self> {
        if let Some(dir) = crate::settings::config_file(STORE).parent() {
            let _ = crate::settings::create_private_dir(dir);
        }
        let mut secrets = Secrets::load();
        if secrets.adopt_virustotal_key(&mut settings, Secrets::save) {
            settings.save();
        }
        let limit = Arc::new(RateLimit::default());
        limit.set(u64::from(settings.speed_limit_kib) * 1024);
        let this = Arc::new(Self {
            rt,
            clients: Mutex::default(),
            secrets: Mutex::new(secrets),
            offer: Mutex::default(),
            to_confirm: Mutex::default(),
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
            settings_dirty: AtomicBool::new(false),
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
        });
        this.spawn_ticker();
        this.spawn_update_checks();
        this.spawn_clipboard_watch();
        this.schedule();
        this
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

    /// The saved passwords (site logins, proxy), for the settings.
    pub fn secrets(&self) -> Secrets {
        lock(&self.secrets).clone()
    }

    /// Replaces the saved passwords; downloads started from now on use them.
    pub fn set_secrets(&self, secrets: Secrets) {
        let changed = {
            let mut current = lock(&self.secrets);
            let changed = *current != secrets;
            *current = secrets;
            changed
        };
        if changed {
            lock(&self.secrets).save();
            self.forget_clients(); // the proxy password may have changed
        }
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

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    /// One field without cloning the whole settings (read every frame).
    pub fn with_settings<R>(&self, f: impl FnOnce(&Settings) -> R) -> R {
        f(&lock(&self.settings))
    }

    /// Takes effect immediately; `save_settings` persists (the UI debounces it, shutdown flushes it).
    pub fn apply_settings(self: &Arc<Self>, mut new: Settings) {
        new.sanitize();
        self.limit.set(u64::from(new.speed_limit_kib) * 1024);
        crate::i18n::set(new.language);
        let (proxy_changed, queues) = {
            let mut current = lock(&self.settings);
            let proxy_changed = current.proxy != new.proxy;
            let queues: HashSet<u32> = new.queues.iter().map(|q| q.id).collect();
            *current = new;
            (proxy_changed, queues)
        };
        if proxy_changed {
            self.forget_clients();
        }
        // Downloads of a deleted queue go back to the main one.
        let mut moved = false;
        for e in lock(&self.entries).iter_mut() {
            if e.download.queue != 0 && !queues.contains(&e.download.queue) {
                e.download.queue = 0;
                moved = true;
            }
        }
        if moved {
            self.changed();
        }
        self.settings_dirty.store(true, Release);
        self.schedule();
    }

    /// Writes the settings if they changed since the last save.
    pub fn save_settings(&self) {
        if self.settings_dirty.swap(false, AcqRel) {
            lock(&self.settings).save(); // under the lock: saves land in the order of the changes
        }
    }

    pub fn stats(&self) -> Stats {
        lock(&self.entries).iter().fold(Stats::default(), |mut s, e| {
            match e.download.status() {
                Status::Running => {
                    let (done, total, _) = e.progress.snapshot();
                    s.running += 1;
                    s.speed += e.speed;
                    s.done += done.min(total);
                    s.total += total;
                }
                Status::Queued => s.queued += 1,
                _ => {}
            }
            s
        })
    }

    /// ▶ on a paused / failed download (or one waiting to retry): back in the queue, now.
    pub fn resume(self: &Arc<Self>, id: DownloadId) {
        self.update(id, Entry::resume);
        self.schedule();
    }

    pub fn pause(self: &Arc<Self>, id: DownloadId) {
        self.update(id, Entry::pause);
        self.schedule();
    }

    /// One pass under one lock (queued downloads are paused too: nothing starts afterwards).
    pub fn pause_all(&self) {
        lock(&self.entries).iter_mut().for_each(Entry::pause);
        self.changed();
    }

    /// Paused and failed downloads go back in the queue, in their list order.
    pub fn resume_all(self: &Arc<Self>) {
        lock(&self.entries).iter_mut().for_each(Entry::resume);
        self.changed();
        self.schedule();
    }

    /// Removes from the list; `delete_file` also erases the file (always for unfinished parts).
    pub fn remove(self: &Arc<Self>, id: DownloadId, delete_file: bool) {
        if let Some(token) = self.token_of(id) {
            self.cancel_recording(&token, tr!("annulé", "cancelled"));
        }
        let removed = {
            let mut entries = lock(&self.entries);
            entries.iter().position(|e| e.download.id == id).map(|i| entries.remove(i))
        };
        if let Some(e) = removed {
            if let Some(cancel) = &e.cancel {
                cancel.cancel();
            }
            if delete_file || *e.download.status() != Status::Completed {
                let this = self.clone();
                let (target, replaces) = (e.download.target.clone(), e.replaces);
                self.rt.spawn(async move {
                    // Wait for the task to stop writing (bounded), then clean up.
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while lock(&this.busy).contains(&id) && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    for path in leftovers(&target, delete_file, replaces) {
                        let _ = tokio::fs::remove_file(path).await;
                    }
                    remove_recording_leftovers(&target).await;
                });
            }
        }
        self.changed();
        self.schedule();
    }

    pub fn clear_completed(&self) {
        lock(&self.entries).retain(|e| *e.download.status() != Status::Completed);
        self.changed();
    }

    /// SHA-256 of a finished file, computed off the UI thread, then shown by the UI.
    pub fn compute_sha256(self: &Arc<Self>, id: DownloadId) {
        let Some(path) = self.view(|es| {
            es.iter().find(|e| e.download.id == id && *e.download.status() == Status::Completed).map(|e| e.download.target.clone())
        }) else {
            return;
        };
        let this = self.clone();
        self.rt.spawn_blocking(move || {
            if let Ok(hash) = checksum::digest(&path, checksum::Algo::Sha256) {
                this.update(id, |e| e.sha256 = Some(hash));
            }
        });
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

    /// Starts queued downloads (FIFO) while fewer than `max_parallel` are running. Choosing and
    /// marking them running happen under one lock: concurrent calls can never start one twice nor
    /// exceed the limit.
    fn schedule(self: &Arc<Self>) {
        if self.is_closing() {
            return;
        }
        // Each queue has its own number of places; a download of an unknown queue counts as the
        // main queue's.
        let (limits, main_limit) = self.with_settings(|s| {
            let limits: HashMap<u32, usize> = s.queues.iter().map(|q| (q.id, usize::from(q.max_parallel))).collect();
            (limits, usize::from(s.max_parallel))
        });
        let queue_of = |q: u32| if limits.contains_key(&q) { q } else { 0 };
        let limit_of = |q: u32| limits.get(&q).copied().unwrap_or(main_limit);
        let now = Instant::now();
        let launches: Vec<Launch> = {
            let mut entries = lock(&self.entries);
            let mut busy = lock(&self.busy);
            let mut running: HashMap<u32, usize> = HashMap::new();
            for e in entries.iter().filter(|e| e.occupies_slot()) {
                *running.entry(queue_of(e.download.queue)).or_default() += 1;
            }
            let mut launches = Vec::new();
            for e in entries.iter_mut() {
                if !e.startable(now) || busy.contains(&e.download.id) {
                    continue;
                }
                let queue = queue_of(e.download.queue);
                let taken = running.entry(queue).or_default();
                if *taken >= limit_of(queue) {
                    continue;
                }
                if let Some(launch) = e.start(&self.limit) {
                    busy.insert(launch.id);
                    launches.push(launch);
                    *taken += 1;
                }
            }
            launches
        };
        if launches.is_empty() {
            return;
        }
        self.changed();
        launches.into_iter().for_each(|launch| self.run(launch));
    }

    fn run(self: &Arc<Self>, launch: Launch) {
        let Launch { id, mut job, progress, cancel, via_proxy, insecure } = launch;
        self.inflight.fetch_add(1, AcqRel);
        let this = self.clone();
        self.rt.spawn(async move {
            this.add_login(&job.url, &mut job.headers);
            let before = progress.downloaded.load(Relaxed);
            let result = match this.client_for_url(&job.url, via_proxy, insecure).await {
                Ok(client) => Ok(engine::run(&client, &job, progress.clone(), cancel).await),
                Err(reason) => Err(reason),
            };
            let progressed = progress.downloaded.load(Relaxed) > before;
            let auto_proxy = this.auto_proxy();
            let (mut finished, mut verify) = (None, false);
            this.update(id, |e| {
                e.cancel = None;
                let restart = std::mem::take(&mut e.restart);
                if progressed {
                    e.retries = 0;
                }
                let _ = match &result {
                    Err(reason) => e.download.fail(reason.clone()),
                    Ok(Ok(Outcome::Completed)) => {
                        mark_from_internet(&e.download.target);
                        let _ = e.download.start(); // a pause may have raced the last byte
                        finished = Some(e.name.clone());
                        verify = e.download.checksum.is_some();
                        e.retries = 0;
                        e.download.complete()
                    }
                    // Stopped to start again (new route, link, certificate choice): straight back.
                    Ok(Ok(Outcome::Paused)) if restart => e.download.retry_later(),
                    Ok(Ok(Outcome::Paused)) => Ok(()),
                    // Network down, server busy: back in the queue, retried later on its own.
                    Ok(Err(err)) if !err.is_permanent() && e.retries < AUTO_RETRIES && e.download.retry_later().is_ok() => {
                        // Automatic proxy mode: a server the direct route cannot reach is tried
                        // through the proxy, at once.
                        let unreachable = matches!(err, engine::EngineError::Http(h) if h.is_connect() || h.is_timeout());
                        let delay = if auto_proxy && !e.via_proxy && unreachable {
                            e.via_proxy = true;
                            Duration::ZERO
                        } else {
                            retry_delay(e.retries)
                        };
                        e.retry = Some(Retry { at: Instant::now() + delay, reason: describe(err) });
                        e.retries += 1;
                        Ok(())
                    }
                    Ok(Err(err)) => e.download.fail(describe(err)),
                };
            });
            lock(&this.busy).remove(&id);
            this.inflight.fetch_sub(1, AcqRel);
            if verify {
                this.verify(id);
            }
            if let Some(name) = finished
                && this.with_settings(|s| s.notify)
            {
                notify::completed(&name);
            }
            this.schedule();
        });
    }

    /// Replaces the link of a download that stopped working (expired, moved) and resumes it where
    /// it was. The new link must lead to the same file: when the server tells its size and it
    /// differs, the change is refused (the parts already downloaded would not fit).
    pub fn change_url(self: &Arc<Self>, id: DownloadId, url: Url) {
        let Some((total, headers, via_proxy, insecure)) =
            self.view(|es| es.iter().find(|e| e.download.id == id).map(|e| (e.progress.total.load(Relaxed), to_header_map(&e.headers), e.via_proxy, e.download.insecure)))
        else {
            return;
        };
        let this = self.clone();
        self.rt.spawn(async move {
            let size = match this.client_for_url(&url, via_proxy, insecure).await {
                Ok(client) => engine::probe_once(&client, &url, &headers).await.ok().and_then(|p| p.size),
                Err(_) => None,
            };
            if total > 0 && size.is_some_and(|s| s != total) {
                let (have, got) = (crate::ui::size_text(total), crate::ui::size_text(size.unwrap_or(0)));
                this.notice(true, &trf!(
                    "Lien refusé : le fichier fait {got}, pas {have} (ce n'est pas le même)",
                    "Link refused: the file is {got}, not {have} (not the same file)",
                    got = got,
                    have = have
                ));
                return;
            }
            let changed = this.update(id, |e| {
                if e.download.is_recording() || *e.download.status() == Status::Completed {
                    return false;
                }
                e.download.url = url.clone();
                e.retries = 0;
                e.retry = None;
                match e.download.status() {
                    Status::Running => e.restart(),
                    Status::Failed(_) => {
                        let _ = e.download.enqueue();
                    }
                    _ => {}
                }
                true
            });
            if changed == Some(true) {
                this.notice(false, tr!("Lien remplacé : le téléchargement reprend où il en était", "Link replaced: the download resumes where it was"));
                this.schedule();
            }
        });
    }

    /// This download's own speed cap (KiB/s, 0 = none), applied at once if it runs.
    pub fn set_speed_limit(&self, id: DownloadId, kib: u32) {
        self.update(id, |e| {
            e.download.speed_limit_kib = kib;
            e.own_limit.set(u64::from(kib) * 1024);
        });
    }

    pub fn move_to_queue(self: &Arc<Self>, id: DownloadId, queue: u32) {
        self.update(id, |e| e.download.queue = queue);
        self.schedule();
    }

    /// Accepts (or not) an invalid TLS certificate for this download only, and retries it.
    pub fn set_insecure(self: &Arc<Self>, id: DownloadId, insecure: bool) {
        self.update(id, |e| {
            e.download.insecure = insecure;
            match e.download.status() {
                Status::Running => e.restart(),
                Status::Failed(_) if insecure => {
                    let _ = e.download.enqueue();
                }
                _ => {}
            }
        });
        self.schedule();
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

    /// Total speed over the last minute, oldest first (bytes per second).
    pub fn speed_history(&self) -> Vec<f32> {
        lock(&self.history).iter().copied().collect()
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
        if *saved < generation {
            save_json(STORE, &stored);
            *saved = generation;
        }
    }

    fn repaint(&self) {
        if let Some(f) = self.repaint.get() {
            f();
        }
    }

    fn spawn_ticker(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.rt.spawn(async move {
            let (mut ticks, mut idle, mut last) = (0u32, 0u32, Instant::now());
            loop {
                // Nothing moving for a while: fewer wake-ups (a new download is started by the
                // scheduler itself, not by this tick).
                tokio::time::sleep(if idle >= IDLE_AFTER { IDLE_TICK } else { TICK }).await;
                let Some(this) = weak.upgrade() else { return };
                this.reap_idle_recordings();
                let elapsed = std::mem::replace(&mut last, Instant::now()).elapsed();
                let (moving, retry_due) = this.tick(elapsed);
                let recording = !lock(&this.recordings).is_empty();
                idle = if moving || retry_due || recording { 0 } else { idle.saturating_add(1) };
                if moving {
                    this.repaint();
                }
                if retry_due {
                    this.schedule();
                }
                // Progress written now and then while transfers run: after a crash the list shows
                // where each download was (the resume point itself lives in its `.rdm` file).
                ticks = ticks.wrapping_add(1);
                if moving && ticks.is_multiple_of(PROGRESS_SAVE_TICKS) {
                    this.dirty.store(true, Release);
                }
                if this.dirty.load(Acquire) {
                    let _ = tokio::task::spawn_blocking(move || this.persist()).await;
                }
            }
        });
    }

    /// Updates speeds and the history. Returns whether the UI has something moving to show (a
    /// running download, a retry countdown, the chart still scrolling back to zero), and whether
    /// a download waiting to retry is due.
    fn tick(&self, elapsed: Duration) -> (bool, bool) {
        let (mut active, mut total, mut due) = (false, 0.0, false);
        let now = Instant::now();
        let auto_proxy = self.auto_proxy();
        for e in lock(&self.entries).iter_mut() {
            let (done, total_size, _) = e.progress.snapshot();
            let running = *e.download.status() == Status::Running;
            let instant = done.saturating_sub(e.last) as f64 / elapsed.as_secs_f64().max(0.05);
            e.speed = if running { e.speed * 0.6 + instant * 0.4 } else { 0.0 };
            e.last = done;
            let waiting = *e.download.status() == Status::Queued && e.retry.is_some();
            active |= running || waiting || e.resolving;
            due |= waiting && e.startable(now);
            total += e.speed;
            // Automatic proxy mode: a download stuck slow on the direct route switches (it keeps
            // its progress). Not near its end: a nearly finished file is not worth a restart.
            let settled = e.started.is_some_and(|t| t.elapsed() > SLOW_GRACE);
            let far_from_done = total_size == 0 || done.saturating_mul(10) < total_size.saturating_mul(9);
            // Slow because the user capped it: the proxy would not help.
            let capped = engine::Job::effective_limit(&self.limit, &e.own_limit);
            let chosen = capped > 0 && (capped as f64) < SLOW_SPEED * 4.0;
            if auto_proxy && running && !e.via_proxy && !e.download.is_recording() && settled && far_from_done && !chosen && e.speed < SLOW_SPEED {
                let since = *e.slow_since.get_or_insert(now);
                if now.duration_since(since) >= SLOW_FOR {
                    e.via_proxy = true;
                    e.restart();
                }
            } else {
                e.slow_since = None;
            }
        }
        let mut history = lock(&self.history);
        history.pop_front();
        history.push_back(total as f32);
        (active || history.iter().any(|&s| s > 0.0), due)
    }
}

/// Engine error → a short sentence in the interface language (no URLs: they may carry tokens).
fn describe(err: &engine::EngineError) -> String {
    use engine::EngineError as E;
    match err {
        E::Http(e) if e.is_redirect() => tr!("redirection refusée (boucle ou vers le réseau local)", "redirect refused (loop, or into the local network)").into(),
        _ if err.is_certificate() => tr!(
            "certificat du site non valide (clic droit › Accepter un certificat non valide, si vous faites confiance au site)",
            "the site's certificate is not valid (right-click › Accept an invalid certificate, if you trust the site)"
        )
        .into(),
        E::Http(e) => match e.status().map(|s| s.as_u16()) {
            Some(401) => tr!(
                "identifiants requis (Paramètres › Identifiants des sites)",
                "login required (Settings › Site logins)"
            )
            .into(),
            Some(403) => tr!("accès refusé par le serveur (lien expiré ou protégé)", "access denied by the server (link expired or protected)").into(),
            Some(404) => tr!("fichier introuvable (404)", "file not found (404)").into(),
            Some(410) => tr!("lien expiré (410)", "link expired (410)").into(),
            Some(429) => tr!("le serveur limite les connexions (429)", "the server limits connections (429)").into(),
            Some(s @ 500..=599) => trf!("erreur du serveur ({s})", "server error ({s})", s = s),
            Some(s) => trf!("le serveur a répondu {s}", "the server answered {s}", s = s),
            None if e.is_timeout() => tr!("délai dépassé, connexion trop lente", "timed out, connection too slow").into(),
            None if e.is_connect() => tr!("connexion impossible au serveur", "cannot connect to the server").into(),
            None => tr!("connexion interrompue", "connection interrupted").into(),
        },
        E::Io(e) => trf!("erreur disque : {e}", "disk error: {e}", e = e),
        E::RangeIgnored => tr!("le serveur a renvoyé une plage incohérente", "the server sent an inconsistent range").into(),
        E::Empty => tr!("le serveur n'a renvoyé aucune donnée (lien expiré ou protégé)", "the server sent no data (link expired or protected)").into(),
        E::Truncated => tr!("connexion coupée avant la fin", "connection cut before the end").into(),
        E::Stalled => tr!("connexion interrompue", "connection interrupted").into(),
        E::LocalNetwork => tr!("bloqué : un contenu Internet visait votre réseau local", "blocked: Internet content pointing into your local network").into(),
        E::Playlist(m) => trf!("flux vidéo : {m}", "video stream: {m}", m = m),
        E::Mux(e) => trf!("fusion audio/vidéo impossible : {e}", "cannot merge audio and video: {e}", e = e),
    }
}


/// Seconds since 1970 (what the files of the settings folder record).
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

