//! Application service: owns the queues, schedules downloads and orchestrates the engine.

pub mod checksum;
pub mod clipboard;
mod routes;

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
    settings::{ExistingFile, Settings, save_json, with_suffix},
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
/// Never written to disk: session cookies stay in memory only.
const SECRET_HEADERS: [&str; 2] = ["cookie", "authorization"];

/// What the browser (or the UI) hands us.
#[derive(Debug, Deserialize)]
pub struct AddRequest {
    pub url: Url,
    pub audio_url: Option<Url>,
    pub filename: Option<String>,
    pub referrer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    /// The user's answer for this download when its file already exists (else the settings').
    #[serde(skip)]
    pub existing: Option<ExistingFile>,
}

impl AddRequest {
    pub fn from_url(url: Url) -> Self {
        Self { url, audio_url: None, filename: None, referrer: None, cookies: None, user_agent: None, existing: None }
    }

    /// The request behind a download of the list, from its link and headers.
    fn from_parts(url: Url, audio_url: Option<Url>, headers: &[(String, String)]) -> Self {
        let get = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        Self { audio_url, referrer: get("referer"), cookies: get("cookie"), user_agent: get("user-agent"), ..Self::from_url(url) }
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        [("referer", &self.referrer), ("cookie", &self.cookies), ("user-agent", &self.user_agent)]
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_owned(), v.clone().filter(|v| !v.is_empty())?)))
            .collect()
    }
}

/// A download waiting for the user's answer, as the window shows it.
pub struct ToConfirm {
    pub url: Url,
    /// Its name, when known.
    pub name: Option<String>,
    /// A file of that name is already there, and the user wants to be asked (`ExistingFile::Ask`).
    pub ask_existing: bool,
    /// Every browser download is confirmed (`confirm_browser`); otherwise only the file is asked about.
    pub confirming: bool,
    pub waiting: usize,
}

pub struct Entry {
    pub download: Download,
    pub progress: Arc<Progress>,
    pub speed: f64,
    /// SHA-256 of the finished file, once computed (on request, or for VirusTotal).
    pub sha256: Option<String>,
    /// VirusTotal analysis of the finished file.
    pub scan: Scan,
    /// Derived once from the (immutable) target: the list redraws them every frame.
    pub name: String,
    /// `name` in lower case, for the search box.
    pub search_key: String,
    pub category: Category,
    /// Just added: the server is being asked for the file's real name; not started before.
    pub resolving: bool,
    /// Waiting to retry after a transient failure, and why it failed.
    pub retry: Option<Retry>,
    /// Checksum verification of the finished file (see `checksum`).
    pub verify: Verify,
    /// Automatic proxy mode: this download goes through the proxy (slow or unreachable directly).
    pub via_proxy: bool,
    headers: Vec<(String, String)>,
    cancel: Option<CancellationToken>,
    last: u64,
    /// Automatic retries in a row without progress.
    retries: u32,
    /// When it was added in this session (`None`: loaded from disk).
    added: Option<Instant>,
    /// Its own speed limiter, shared with its running job: a change applies at once.
    own_limit: Arc<RateLimit>,
    /// When it last started, and since when it has been too slow (automatic proxy).
    started: Option<Instant>,
    slow_since: Option<Instant>,
    /// Stopped to start again right away (new route, new link, certificate choice).
    restart: bool,
    /// Its target was an existing file, to overwrite ("overwrite" in the settings): removing the
    /// download must not take that file along before the transfer wrote over it.
    replaces: bool,
}

/// Checking a finished file against the checksum the user gave.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Verify {
    #[default]
    None,
    Running,
    Ok,
    /// The file's actual checksum.
    Mismatch(String),
    Failed(String),
}

/// A download back in the queue after a transient failure (network down, busy server).
#[derive(Debug, Clone)]
pub struct Retry {
    pub at: Instant,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Scan {
    #[default]
    None,
    Running(Stage),
    Done(Report),
    Failed(String),
}

/// Why a VirusTotal analysis cannot start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanRefused {
    /// No API key yet: the settings must ask for one.
    NoKey,
    /// Not finished, too large for VirusTotal, or already being analysed.
    NotEligible,
}

/// A download about to start: its job, and how to reach its server.
struct Launch {
    id: DownloadId,
    job: engine::Job,
    progress: Arc<Progress>,
    cancel: CancellationToken,
    via_proxy: bool,
    insecure: bool,
}

impl Entry {
    fn new(download: Download, headers: Vec<(String, String)>, downloaded: u64, total: u64) -> Self {
        let progress = Arc::new(Progress::default());
        progress.downloaded.store(downloaded, Relaxed);
        progress.total.store(total, Relaxed);
        let own_limit = Arc::new(RateLimit::default());
        own_limit.set(u64::from(download.speed_limit_kib) * 1024);
        let mut entry = Self {
            search_key: String::new(),
            name: String::new(),
            category: download.category(),
            download,
            progress,
            speed: 0.0,
            sha256: None,
            scan: Scan::None,
            resolving: false,
            retry: None,
            verify: Verify::None,
            via_proxy: false,
            headers,
            cancel: None,
            last: downloaded,
            retries: 0,
            added: None,
            own_limit,
            started: None,
            slow_since: None,
            restart: false,
            replaces: false,
        };
        entry.named();
        entry
    }

    /// Stops a running transfer to start it again at once (it keeps its progress).
    fn restart(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            self.restart = true;
            cancel.cancel();
        }
    }

    /// Derives what the list shows from the target (after creation, or a rename before start).
    fn named(&mut self) {
        let d = &self.download;
        self.name = d.target.file_name().map_or_else(|| d.url.to_string(), |n| n.to_string_lossy().into_owned());
        self.search_key = self.name.to_lowercase();
        self.category = d.category();
    }

    /// Seconds left at the current speed.
    pub fn eta(&self) -> Option<u64> {
        let (done, total, _) = self.progress.snapshot();
        (self.speed > 1.0 && total > done).then(|| ((total - done) as f64 / self.speed) as u64)
    }

    /// A finished file VirusTotal accepts (not empty, 650 MB at most).
    pub fn scannable(&self) -> bool {
        let size = self.progress.total.load(Relaxed).max(self.progress.downloaded.load(Relaxed));
        *self.download.status() == Status::Completed && (1..=virustotal::MAX_UPLOAD).contains(&size)
    }

    /// Takes a download slot (browser recordings do not: the browser paces them).
    fn occupies_slot(&self) -> bool {
        *self.download.status() == Status::Running && !self.download.is_recording()
    }

    /// A recording follows the browser's playback: it cannot be paused from here.
    fn pause(&mut self) {
        if !self.download.is_recording() && self.download.pause().is_ok() {
            self.retry = None;
            if let Some(cancel) = self.cancel.take() {
                cancel.cancel();
            }
        }
    }

    /// The user asked: no waiting for an automatic retry, and a fresh retry budget.
    fn resume(&mut self) {
        if self.download.enqueue().is_ok() || self.retry.is_some() {
            self.retry = None;
            self.retries = 0;
        }
    }

    /// Can the scheduler start it now?
    fn startable(&self, now: Instant) -> bool {
        *self.download.status() == Status::Queued && !self.resolving && self.retry.as_ref().is_none_or(|r| r.at <= now)
    }

    /// Queued → running: the engine job, with a fresh cancel token.
    fn start(&mut self, limit: &Arc<RateLimit>) -> Option<Launch> {
        self.download.start().ok()?;
        self.retry = None;
        self.restart = false;
        self.started = Some(Instant::now());
        self.slow_since = None;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let job = engine::Job {
            url: self.download.url.clone(),
            target: self.download.target.clone(),
            connections: self.download.connections,
            headers: to_header_map(&self.headers),
            audio: self.download.audio.clone(),
            limit: limit.clone(),
            own_limit: self.own_limit.clone(),
        };
        Some(Launch {
            id: self.download.id,
            job,
            progress: self.progress.clone(),
            cancel,
            via_proxy: self.via_proxy,
            insecure: self.download.insecure,
        })
    }

    /// What goes to disk: never the session secrets.
    fn stored(&self) -> Stored {
        let (downloaded, total, _) = self.progress.snapshot();
        let headers = self.headers.iter().filter(|(k, _)| !SECRET_HEADERS.contains(&k.as_str())).cloned().collect();
        let scan = if let Scan::Done(report) = &self.scan { Some(report.clone()) } else { None };
        Stored { download: self.download.clone(), headers, downloaded, total, sha256: self.sha256.clone(), scan, replaces: self.replaces }
    }
}

/// Track of a browser recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Track {
    Video,
    Audio,
}

impl Track {
    const fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// A recording fed by the browser's own player (extension/capture.js): the bytes arrive per
/// media source (the page may create several: ads, quality restarts) and per track.
struct Recording {
    id: DownloadId,
    target: PathBuf,
    parts: HashMap<(u32, Track), u64>,
    last_data: Instant,
}

impl Recording {
    fn part(&self, ms: u32, track: Track) -> PathBuf {
        with_suffix(&self.target, &format!(".rec{ms}.{}", track.name()))
    }

    /// The main programme: the media source with both tracks and the most data (not an ad).
    fn best_source(&self) -> Option<u32> {
        let sources: HashSet<u32> = self.parts.keys().map(|(ms, _)| *ms).collect();
        sources
            .into_iter()
            .filter(|ms| self.parts.contains_key(&(*ms, Track::Video)) && self.parts.contains_key(&(*ms, Track::Audio)))
            .max_by_key(|ms| self.parts[&(*ms, Track::Video)] + self.parts[&(*ms, Track::Audio)])
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    download: Download,
    headers: Vec<(String, String)>,
    downloaded: u64,
    total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    /// Last VirusTotal verdict: the badge survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scan: Option<Report>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    replaces: bool,
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
    /// Used only if no configured client can be built.
    fallback: Client,
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
    /// Firefox: the signed package is installing for good (Firefox asks to confirm).
    pub signed: bool,
    /// Firefox: the unsigned package, for the editions that accept one.
    pub xpi: Option<PathBuf>,
}

/// Firefox gives each install of an extension a random origin (`moz-extension://<uuid>`), which
/// cannot be pinned like Chrome's: the native connector pairs it (the browser vouches for the
/// extension), or, without the connector, the user approves it once in the RDM window.
#[derive(Default)]
struct FirefoxPairing {
    /// Firefox, Waterfox, LibreWolf… each profile has its own origin; the latest few are kept.
    paired: VecDeque<String>,
    pending: Option<String>,
    /// Refused this session: never asked again until RDM restarts.
    refused: HashSet<String>,
    /// "Later" (✕): the extension's periodic check-ins do not bring the question back before this.
    snoozed_until: Option<Instant>,
}

/// How long "later" on the Firefox question lasts, unless the user clicks the extension's button.
const FIREFOX_SNOOZE: Duration = Duration::from_secs(30 * 60);

const FIREFOX_FILE: &str = "firefox.json";
const MAX_PAIRED: usize = 8;

impl FirefoxPairing {
    /// `firefox.json`: a list of origins (RDM 0.2 wrote a single one).
    fn load() -> VecDeque<String> {
        let bytes = std::fs::read(crate::settings::config_file(FIREFOX_FILE)).unwrap_or_default();
        let origins = serde_json::from_slice::<VecDeque<String>>(&bytes)
            .or_else(|_| serde_json::from_slice::<String>(&bytes).map(|o| VecDeque::from([o])))
            .unwrap_or_default();
        origins.into_iter().filter(|o| is_firefox_origin(o)).take(MAX_PAIRED).collect()
    }

    fn pair(&mut self, origin: String) {
        self.refused.remove(&origin);
        if self.pending.as_ref() == Some(&origin) {
            self.pending = None;
        }
        if !self.paired.contains(&origin) {
            self.paired.push_back(origin);
            while self.paired.len() > MAX_PAIRED {
                self.paired.pop_front();
            }
            save_json(FIREFOX_FILE, &self.paired);
        }
    }
}

fn is_firefox_origin(origin: &str) -> bool {
    origin.strip_prefix("moz-extension://").is_some_and(|id| {
        id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    })
}

impl Manager {
    pub fn new(rt: Handle, fallback: Client) -> Arc<Self> {
        if let Some(dir) = crate::settings::config_file(STORE).parent() {
            let _ = crate::settings::create_private_dir(dir);
        }
        let settings = Settings::load();
        crate::i18n::set(settings.language);
        let limit = Arc::new(RateLimit::default());
        limit.set(u64::from(settings.speed_limit_kib) * 1024);
        let this = Arc::new(Self {
            rt,
            fallback,
            clients: Mutex::default(),
            secrets: Mutex::new(Secrets::load()),
            offer: Mutex::default(),
            to_confirm: Mutex::default(),
            own_copy: Mutex::default(),
            notices: Mutex::default(),
            restart_after_exit: AtomicBool::new(false),
            settings: Mutex::new(settings),
            entries: Mutex::new(load_entries()),
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
            browsers: Mutex::new(
                fs::read(crate::settings::config_file(BROWSERS_FILE))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default(),
            ),
            uninstalls: Mutex::new(
                fs::read(crate::settings::config_file(UNINSTALLS_FILE))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default(),
            ),
            installs: Mutex::default(),
            custom_browsers: Mutex::new(
                fs::read(crate::settings::config_file(CUSTOM_BROWSERS_FILE))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default(),
            ),
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

    /// Whether this Firefox extension origin may use the bridge; an unknown one is put up for the
    /// user's approval (the window comes to the front) and refused meanwhile.
    pub fn firefox_allowed(&self, origin: &str) -> bool {
        if !is_firefox_origin(origin) {
            return false;
        }
        let mut ff = lock(&self.firefox);
        if ff.paired.iter().any(|o| o == origin) {
            return true;
        }
        let snoozed = ff.snoozed_until.is_some_and(|t| Instant::now() < t);
        if ff.pending.is_none() && !ff.refused.contains(origin) && !snoozed {
            ff.pending = Some(origin.to_owned());
            drop(ff);
            self.show();
            self.repaint();
        }
        false
    }

    /// The user clicked the extension's button: a question put off with "later" comes back now.
    pub fn firefox_wake(&self) {
        lock(&self.firefox).snoozed_until = None;
    }

    /// The Firefox origin waiting for the user's approval, if any.
    pub fn firefox_pending(&self) -> Option<String> {
        lock(&self.firefox).pending.clone()
    }

    /// `None`: dismissed without answering — asked again at the extension's next request.
    pub fn answer_firefox(&self, allow: Option<bool>) {
        let mut ff = lock(&self.firefox);
        let Some(origin) = ff.pending.take() else { return };
        match allow {
            Some(true) => ff.pair(origin),
            Some(false) => {
                ff.refused.insert(origin);
            }
            None => ff.snoozed_until = Some(Instant::now() + FIREFOX_SNOOZE),
        }
    }

    /// The native connector vouches for this Firefox extension origin (the browser started the
    /// connector for the RDM extension only): paired without asking.
    pub fn pair_firefox(&self, origin: &str) -> bool {
        if !is_firefox_origin(origin) {
            return false;
        }
        lock(&self.firefox).pair(origin.to_owned());
        self.repaint();
        true
    }

    /// The extension reported which browser it runs in (`x-rdm-browser`).
    pub fn browser_seen(&self, key: &str) {
        let Some(browser) = Browser::from_key(key) else { return };
        if self.uninstall_requested(key) {
            return; // on its way out: not "installed" again
        }
        let now = unix_now();
        let mut seen = lock(&self.browsers);
        let before = seen.insert(browser.key().to_owned(), now);
        // Written when news, not on every request (recordings post chunks many times a second).
        if before.is_none_or(|t| now.saturating_sub(t) > 600) {
            let copy = seen.clone();
            drop(seen);
            save_json(BROWSERS_FILE, &copy);
            self.repaint();
        }
    }

    /// When the extension was last heard from, per browser (Unix time).
    pub fn browser_last_seen(&self, browser: Browser) -> Option<u64> {
        lock(&self.browsers).get(browser.key()).copied()
    }

    /// "Remove the extension": RDM forgets it in `browser` at once, and the extension, if still
    /// there, uninstalls itself at its next check-in (at the browser's start, then every few minutes).
    pub fn remove_extension(&self, browser: Browser) {
        let key = browser.key().to_owned();
        let seen = {
            let mut seen = lock(&self.browsers);
            seen.remove(&key);
            seen.clone()
        };
        save_json(BROWSERS_FILE, &seen);
        let pending = {
            let mut pending = lock(&self.uninstalls);
            pending.insert(key, unix_now());
            pending.clone()
        };
        save_json(UNINSTALLS_FILE, &pending);
        lock(&self.installs).remove(&browser);
        self.repaint();
    }

    /// Whether the extension in the browser named `key` must uninstall itself.
    pub fn uninstall_requested(&self, key: &str) -> bool {
        lock(&self.uninstalls).get(key).is_some_and(|&t| unix_now().saturating_sub(t) < UNINSTALL_TTL_SECS)
    }

    /// The request is over: the extension is uninstalling itself, or the user installs it again.
    fn forget_uninstall(&self, key: &str) {
        let pending = {
            let mut pending = lock(&self.uninstalls);
            if pending.remove(key).is_none() {
                return;
            }
            pending.clone()
        };
        save_json(UNINSTALLS_FILE, &pending);
    }

    /// The extension of the browser named `key` got the request and uninstalls itself now.
    pub fn extension_uninstalled(&self, key: &str) {
        self.forget_uninstall(key);
        self.repaint();
    }

    /// Every browser the extension was heard from, with when (Unix time).
    pub fn browsers_seen(&self) -> Vec<(Browser, u64)> {
        lock(&self.browsers).iter().filter_map(|(key, &t)| Some((Browser::from_key(key)?, t))).collect()
    }

    /// The browsers of this computer (Windows' list, the well-known ones, those added by hand),
    /// then those the extension was heard from that are not among them (a portable browser).
    pub fn browsers(&self) -> Vec<Browser> {
        let mut list = extension::installed(&lock(&self.custom_browsers));
        for (b, _) in self.browsers_seen() {
            if !list.contains(&b) {
                list.push(b);
            }
        }
        list
    }

    /// "Add a browser…": the browser at `exe`, remembered. `None`: not a browser the extension
    /// can run in (neither Chromium- nor Firefox-based).
    pub fn add_browser(&self, exe: PathBuf) -> Option<Browser> {
        let browser = Browser::at(&exe)?;
        let list = {
            let mut list = lock(&self.custom_browsers);
            if !list.contains(&exe) {
                list.push(exe);
            }
            list.clone()
        };
        save_json(CUSTOM_BROWSERS_FILE, &list);
        Some(browser)
    }

    /// Whether an installation is being prepared (the window shows a spinner).
    pub fn installing(&self) -> bool {
        lock(&self.installs).values().any(|i| matches!(i, Install::Working))
    }

    /// The extension just installed in the browser named `key`: whether RDM opened that browser
    /// for it a moment ago (then the tab it opened can go). Answered once.
    pub fn installed_by_rdm(&self, key: &str) -> bool {
        lock(&self.opened_to_install).remove(key).is_some_and(|at| at.elapsed() < INSTALL_TAB_TTL)
    }

    pub fn install_state(&self, browser: Browser) -> Option<Install> {
        lock(&self.installs).get(&browser).cloned()
    }

    /// Prepares the extension for `browser` and opens the browser where the user confirms it.
    pub fn install_extension(self: &Arc<Self>, browser: Browser) {
        self.forget_uninstall(browser.key());
        if matches!(lock(&self.installs).insert(browser, Install::Working), Some(Install::Working)) {
            return; // already on it
        }
        self.repaint();
        let this = self.clone();
        self.rt.spawn(async move {
            let state = match this.install(browser).await {
                Ok(done) => Install::Done(done),
                Err(reason) => Install::Failed(reason),
            };
            if matches!(&state, Install::Done(done) if done.launched) {
                lock(&this.opened_to_install).insert(browser.key().to_owned(), Instant::now());
            }
            lock(&this.installs).insert(browser, state);
            this.repaint();
        });
    }

    async fn install(&self, browser: Browser) -> Result<Installed, String> {
        let flavour = browser.flavour();
        let blocking = |e: tokio::task::JoinError| e.to_string();
        let exe = tokio::task::spawn_blocking(move || browser.find()).await.map_err(blocking)?;
        // The connector too (a browser installed since RDM started has not got it yet).
        let _ = tokio::task::spawn_blocking(crate::native::register).await;
        let folder = tokio::task::spawn_blocking(move || extension::write(flavour))
            .await
            .map_err(blocking)?
            .map_err(|e| trf!("impossible d'écrire l'extension : {e}", "cannot write the extension: {e}", e = e))?;
        let mut done = Installed { folder, launched: false, signed: false, xpi: None };
        let open = |target: &str| exe.as_deref().is_some_and(|exe| extension::launch(exe, target).is_ok());
        if flavour == Flavour::Firefox {
            done.xpi = tokio::task::spawn_blocking(extension::write_xpi).await.map_err(blocking)?.ok();
            let signed = extension::base().join("rdm-firefox-signed.xpi");
            if let Some(client) = self.web()
                && update::signed_firefox_xpi(&client, &signed).await.unwrap_or(false)
            {
                done.signed = true;
                done.launched = open(&signed.to_string_lossy());
                return Ok(done);
            }
            // Waterfox can install the unsigned package for good (signature check off: see the steps).
            if browser.key() == "waterfox"
                && let Some(xpi) = &done.xpi
            {
                done.launched = open(&xpi.to_string_lossy());
                return Ok(done);
            }
        }
        done.launched = open(browser.extensions_page());
        Ok(done)
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

    /// A download from the browser extension: added at once, or first shown in the (raised)
    /// window for the user's go-ahead, as the settings say.
    pub fn add_from_browser(self: &Arc<Self>, req: AddRequest) {
        let (confirm, ask) = self.with_settings(|s| (s.confirm_browser, s.existing == ExistingFile::Ask));
        let exists = req
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(engine::sanitize_file_name)
            .is_some_and(|n| self.with_settings(|s| s.target_dir(&n)).join(&n).is_file());
        // Without confirmation, still asked when the file is already there (`ExistingFile::Ask`).
        if confirm || (ask && exists) {
            self.wait_for_answer(req);
        } else {
            self.add(req);
        }
    }

    /// Shown in the (raised) window until the user answers (see `to_confirm`).
    fn wait_for_answer(&self, req: AddRequest) {
        {
            let mut waiting = lock(&self.to_confirm);
            if waiting.iter().any(|w| w.url == req.url) {
                return; // the very same link, just sent twice
            }
            if waiting.len() >= MAX_TO_CONFIRM {
                return; // a page flooding the extension: the user has enough to answer already
            }
            waiting.push_back(req);
        }
        self.show();
        self.repaint();
    }

    /// The first download waiting for the user's answer.
    pub fn to_confirm(&self) -> Option<ToConfirm> {
        let waiting = lock(&self.to_confirm);
        let req = waiting.front()?;
        let name = req.filename.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(engine::sanitize_file_name);
        let (confirming, ask) = self.with_settings(|s| (s.confirm_browser, s.existing == ExistingFile::Ask));
        let exists = name.as_deref().is_some_and(|n| self.with_settings(|s| s.target_dir(n)).join(n).is_file());
        Some(ToConfirm { url: req.url.clone(), name, ask_existing: ask && exists, confirming, waiting: waiting.len() })
    }

    /// The user answered for the first waiting download: `download` it or not, with `existing`
    /// as the answer for a file already there; `always`: stop asking (the ones still waiting then
    /// start too).
    pub fn answer_confirm(self: &Arc<Self>, download: bool, existing: Option<ExistingFile>, always: bool) {
        let Some(mut req) = lock(&self.to_confirm).pop_front() else { return };
        if download {
            req.existing = existing;
            self.add(req);
        }
        if always {
            let mut settings = self.settings();
            settings.confirm_browser = false;
            self.apply_settings(settings);
            self.save_settings();
            let rest: Vec<_> = lock(&self.to_confirm).drain(..).collect();
            rest.into_iter().for_each(|r| self.add(r));
        }
        self.repaint();
    }

    /// Shows the download in the list at once; without a name from the page, the server is asked
    /// for the real one in the background (bounded), and the download starts right after.
    pub fn add(self: &Arc<Self>, req: AddRequest) {
        let headers = req.headers();
        let given = req.filename.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(engine::sanitize_file_name);
        let provisional = given.clone().unwrap_or_else(|| engine::suggest_file_name(&req.url, None));
        let Some(id) = self.insert(req.url.clone(), req.audio_url, &provisional, headers.clone(), given.is_none(), req.existing) else {
            return; // the very same link, just sent twice
        };
        if given.is_some() {
            self.schedule();
            return;
        }
        let this = self.clone();
        self.rt.spawn(async move {
            let name = tokio::time::timeout(NAME_TIMEOUT, this.suggest_name(&req.url, &to_header_map(&headers))).await.ok().flatten();
            this.resolved(id, name);
            this.schedule();
        });
    }

    /// Name from the server; HLS gets the extension of what will actually be written.
    async fn suggest_name(&self, url: &Url, headers: &HeaderMap) -> Option<String> {
        let client = self.client(url).await;
        // The site's saved login too: without it a protected server only answers 401.
        let mut headers = headers.clone();
        self.add_login(url, &mut headers);
        let headers = &headers;
        let probe = engine::probe_once(&client, url, headers).await.ok()?;
        if !probe.hls {
            return Some(probe.file_name);
        }
        let fmp4 = engine::hls_info(&client, url, headers).await.is_ok_and(|i| i.fmp4);
        let stem = probe.file_name.rsplit_once('.').map_or(probe.file_name.as_str(), |(s, _)| s);
        Some(format!("{stem}.{}", if fmp4 { "mp4" } else { "ts" }))
    }

    /// The server's name for a new download (if it gave one): its target follows, unless it
    /// already started meanwhile. Either way it may start now — or, when a file of that name
    /// exists and the settings say to skip it, it goes away.
    fn resolved(&self, id: DownloadId, name: Option<String>) {
        let settings = self.settings();
        let mut entries = lock(&self.entries);
        let Some(i) = entries.iter().position(|e| e.download.id == id) else { return };
        // Its name known at last (a pasted or copied link): a file of that name already there is
        // the user's to decide, before anything is written (`ExistingFile::Ask`).
        let final_name = name.clone().unwrap_or_else(|| entries[i].name.clone());
        if settings.existing == ExistingFile::Ask
            && matches!(entries[i].download.status(), Status::Queued)
            && settings.target_dir(&final_name).join(&final_name).is_file()
        {
            let e = entries.remove(i);
            drop(entries);
            let mut req = AddRequest::from_parts(e.download.url.clone(), e.download.audio.clone(), &e.headers);
            req.filename = Some(final_name);
            self.wait_for_answer(req);
            self.changed();
            return;
        }
        if let Some(name) = name.filter(|n| *n != entries[i].name)
            && matches!(entries[i].download.status(), Status::Queued | Status::Paused)
        {
            match target_for(&settings, &name, &entries, Some(id)) {
                Some(target) => {
                    entries[i].replaces = target.is_file();
                    entries[i].download.target = target;
                    entries[i].named();
                }
                None => {
                    entries.remove(i);
                    drop(entries);
                    self.notice(false, &trf!("Déjà téléchargé, ignoré : {name}", "Already downloaded, skipped: {name}", name = name));
                    self.changed();
                    return;
                }
            }
        }
        entries[i].resolving = false;
        drop(entries);
        self.changed();
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

    /// Client for VirusTotal and GitHub (not the download engine's: see `virustotal::client`),
    /// through the proxy of the settings like downloads; rebuilt when that changes.
    fn web(&self) -> Option<reqwest::Client> {
        let route = self.route(false);
        let mut web = lock(&self.web);
        if let Some((built_for, client)) = web.as_ref()
            && *built_for == route
        {
            return Some(client.clone());
        }
        let client = virustotal::client(&route).or_else(|_| virustotal::client(&engine::Route::System)).ok()?;
        *web = Some((route, client.clone()));
        Some(client)
    }

    pub fn update_state(&self) -> update::State {
        lock(&self.update).clone()
    }

    fn set_update(&self, state: update::State) {
        *lock(&self.update) = state;
        self.repaint();
    }

    /// At start (after a few seconds) and once a day, if enabled in the settings; sooner when the
    /// release was seen before its package was attached. With automatic updates on, a release RDM
    /// installs without asking anything is installed as soon as nothing is downloading.
    fn spawn_update_checks(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            loop {
                let Some(this) = weak.upgrade() else { return };
                if this.with_settings(|s| s.check_updates) {
                    this.check_updates(false);
                }
                drop(this);
                // The check runs in the background: give it time to land before looking at it.
                tokio::time::sleep(Duration::from_secs(60)).await;
                let incomplete = weak.upgrade().is_some_and(|this| {
                    update::method() != update::Method::Manual && this.update_state().release().is_some_and(|r| r.package.is_none())
                });
                let next = tokio::time::Instant::now() + if incomplete { Duration::from_secs(15 * 60) } else { UPDATE_EVERY };
                // Meanwhile, once a minute: install when idle, if allowed.
                while tokio::time::Instant::now() < next {
                    let Some(this) = weak.upgrade() else { return };
                    this.auto_install();
                    drop(this);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            }
        });
    }

    /// Automatic updates: installs the release on offer when RDM can do it without any prompt
    /// (Windows installer, Linux binary in the user's folders) and nothing is downloading.
    fn auto_install(self: &Arc<Self>) {
        let silent = matches!(update::method(), update::Method::Msi | update::Method::Binary);
        let ready = matches!(self.update_state(), update::State::Available(ref r) if update::installs_itself(r));
        if !silent || !ready || !self.with_settings(|s| s.auto_update) || self.is_closing() {
            return;
        }
        let stats = self.stats();
        let recording = lock(&self.entries).iter().any(|e| e.download.is_recording() && *e.download.status() == Status::Running);
        if stats.running == 0 && stats.queued == 0 && !recording {
            self.install_update();
        }
    }

    /// Asks GitHub for a newer release. `manual`: the user clicked, so "up to date" and errors show.
    pub fn check_updates(self: &Arc<Self>, manual: bool) {
        let current = self.update_state();
        // A release first seen without its package (published a minute before the build finished
        // uploading it) is looked at again.
        if current.busy() || (!manual && current.release().is_some_and(|r| r.package.is_some())) {
            return;
        }
        if manual {
            self.set_update(update::State::Checking);
        }
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(client) = this.web() else { return };
            let state = match update::check(&client).await {
                Ok(Some(release)) => update::State::Available(release),
                Ok(None) if manual => update::State::UpToDate,
                Err(reason) if manual => update::State::Failed(reason),
                _ => update::State::Idle,
            };
            this.set_update(state);
        });
    }

    /// Downloads and checks the package (signature included), then installs it without any window:
    /// on Windows the installation assistant takes over once RDM has quit; on Linux the package is
    /// installed first, then RDM quits and starts again. `false` when RDM cannot install this
    /// release itself (the UI then opens the release page).
    pub fn install_update(self: &Arc<Self>) -> bool {
        let Some(release) = self.update_state().release().cloned() else { return false };
        let Some(package) = release.package.clone().filter(|_| update::installs_itself(&release)) else { return false };
        self.set_update(update::State::Downloading(0.0));
        let this = self.clone();
        self.rt.spawn(async move {
            // The update stays on offer: the card lets the user try again.
            let fail = |reason: String| this.set_update(update::State::InstallFailed(release.clone(), reason));
            let Some(client) = this.web() else {
                return fail(tr!("client HTTP indisponible", "HTTP client unavailable").into());
            };
            let progress = {
                let this = this.clone();
                move |f: f32| {
                    let mut state = lock(&this.update);
                    // One repaint per percent is plenty.
                    if matches!(*state, update::State::Downloading(old) if (f - old).abs() < 0.01 && f < 1.0) {
                        return;
                    }
                    *state = update::State::Downloading(f);
                    drop(state);
                    this.repaint();
                }
            };
            let file = match update::download(&client, &release.version, &package, progress).await {
                Ok(file) => file,
                Err(reason) => return fail(reason),
            };
            this.set_update(update::State::Installing);
            if update::method() == update::Method::Msi {
                match update::start_installation(&file) {
                    Ok(()) => this.request_quit(),
                    Err(e) => fail(trf!("impossible de lancer l'installation : {e}", "cannot start the installation: {e}", e = e)),
                }
                return;
            }
            let result = update::install_linux(file.clone()).await;
            let _ = tokio::fs::remove_file(&file).await;
            match result {
                Ok(()) => {
                    this.restart_after_exit.store(true, Release);
                    this.request_quit();
                }
                Err(reason) => fail(reason),
            }
        });
        true
    }

    /// Has VirusTotal analyse a finished file (looked up by hash first, uploaded only if unknown),
    /// entirely in the background; the verdict lands in the entry and in a desktop notification.
    pub fn scan_virustotal(self: &Arc<Self>, id: DownloadId) -> Result<(), ScanRefused> {
        let key = self.with_settings(|s| s.virustotal_key.clone());
        if key.is_empty() {
            return Err(ScanRefused::NoKey);
        }
        let job = self.update_quiet(id, |e| {
            if !e.scannable() || matches!(e.scan, Scan::Running(_)) {
                return None;
            }
            e.scan = Scan::Running(Stage::Queued);
            Some((e.download.target.clone(), e.sha256.clone(), e.name.clone()))
        });
        let Some(Some((path, known_hash, name))) = job else { return Err(ScanRefused::NotEligible) };

        let this = self.clone();
        let gate = self.scan_gate.clone();
        self.rt.spawn(async move {
            let _turn = gate.acquire_owned().await;
            let stage: virustotal::OnStage = {
                let this = this.clone();
                Arc::new(move |stage| {
                    this.update_quiet(id, |e| e.scan = Scan::Running(stage));
                })
            };
            let result = async {
                let sha256 = match known_hash {
                    Some(hash) => hash,
                    None => {
                        stage(Stage::Hashing);
                        let file = path.clone();
                        let hash = tokio::task::spawn_blocking(move || checksum::digest(&file, checksum::Algo::Sha256))
                            .await
                            .map_err(|_| virustotal::Error::Io)?
                            .map_err(|_| virustotal::Error::Io)?;
                        this.update(id, |e| e.sha256 = Some(hash.clone()));
                        hash
                    }
                };
                let client = this.web().ok_or(virustotal::Error::Network)?;
                virustotal::scan(&client, &key, &path, &sha256, stage).await
            }
            .await;
            notify::virustotal(&name, result.as_ref().map_err(ToString::to_string));
            this.update(id, |e| {
                e.scan = match result {
                    Ok(report) => Scan::Done(report),
                    Err(err) => Scan::Failed(err.to_string()),
                };
            });
        });
        Ok(())
    }

    /// Opens a recording: a running entry the browser will feed. Returns its secret token.
    pub fn start_recording(&self, page: Url, name: &str) -> String {
        let name = engine::sanitize_file_name(name);
        let settings = self.settings();
        let mut entries = lock(&self.entries);
        let target = unique_path(&settings.target_dir(&name), &name, &entries, None);
        let download = Download::recording(page, target.clone());
        let id = download.id;
        let mut entry = Entry::new(download, Vec::new(), 0, 0);
        entry.added = Some(Instant::now());
        entries.push(entry);
        drop(entries);
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let recording = Recording { id, target, parts: HashMap::new(), last_data: Instant::now() };
        lock(&self.recordings).insert(token.clone(), recording);
        self.changed();
        token
    }

    /// Appends a chunk of one track; `false` if the token is unknown (finished, cancelled, forged).
    pub async fn record_append(&self, token: &str, ms: u32, track: Track, data: &[u8]) -> std::io::Result<bool> {
        let (path, id) = {
            let mut recordings = lock(&self.recordings);
            let Some(r) = recordings.get_mut(token) else { return Ok(false) };
            r.last_data = Instant::now();
            *r.parts.entry((ms, track)).or_default() += data.len() as u64;
            (r.part(ms, track), r.id)
        };
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, data).await?;
        self.view(|es| {
            if let Some(e) = es.iter().find(|e| e.download.id == id) {
                e.progress.downloaded.fetch_add(data.len() as u64, Relaxed);
            }
        });
        Ok(true)
    }

    /// Playback position (0..1): turns the bytes received so far into an estimated total.
    pub fn record_progress(&self, token: &str, fraction: f64) -> bool {
        let Some(id) = lock(&self.recordings).get(token).map(|r| r.id) else { return false };
        self.view(|es| {
            if let Some(e) = es.iter().find(|e| e.download.id == id) {
                let done = e.progress.downloaded.load(Relaxed);
                if fraction > 0.01 {
                    e.progress.total.store((done as f64 / fraction.min(1.0)) as u64, Relaxed);
                }
            }
        });
        true
    }

    /// End of playback: mux the main programme's tracks into the target, then clean up.
    pub fn finish_recording(self: &Arc<Self>, token: &str) -> bool {
        let Some(r) = lock(&self.recordings).remove(token) else { return false };
        let this = self.clone();
        self.inflight.fetch_add(1, AcqRel); // quitting waits (bounded) for the merge
        lock(&self.busy).insert(r.id); // so does removing it: its files are being written
        self.rt.spawn_blocking(move || {
            let result = match r.best_source() {
                Some(ms) => engine::mux::merge(&r.part(ms, Track::Video), &r.part(ms, Track::Audio), &r.target)
                    .map_err(|e| trf!("fusion audio/vidéo impossible : {e}", "cannot merge audio and video: {e}", e = e)),
                None => Err(tr!("aucune donnée vidéo et audio reçue", "no video or audio data received").to_owned()),
            };
            for (ms, track) in r.parts.keys() {
                let _ = fs::remove_file(r.part(*ms, *track));
            }
            let mut finished = None;
            this.update(r.id, |e| {
                let _ = match result {
                    Ok(()) => {
                        mark_from_internet(&r.target);
                        let size = fs::metadata(&r.target).map_or(0, |m| m.len());
                        e.progress.downloaded.store(size, Relaxed);
                        e.progress.total.store(size, Relaxed);
                        finished = r.target.file_name().map(|n| n.to_string_lossy().into_owned());
                        e.download.complete()
                    }
                    Err(reason) => e.download.fail(reason),
                };
            });
            lock(&this.busy).remove(&r.id);
            this.inflight.fetch_sub(1, AcqRel);
            if let Some(name) = finished
                && this.with_settings(|s| s.notify)
            {
                notify::completed(&name);
            }
        });
        true
    }

    /// Stops a recording (cancelled in the page, idle, or removed): drops its partial data.
    pub fn cancel_recording(&self, token: &str, reason: &str) -> bool {
        let Some(r) = lock(&self.recordings).remove(token) else { return false };
        for (ms, track) in r.parts.keys() {
            let _ = fs::remove_file(r.part(*ms, *track));
        }
        self.update(r.id, |e| {
            let _ = e.download.fail(reason);
        });
        true
    }

    fn token_of(&self, id: DownloadId) -> Option<String> {
        lock(&self.recordings).iter().find(|(_, r)| r.id == id).map(|(t, _)| t.clone())
    }

    /// Recordings whose tab went silent (closed, navigated away) fail instead of hanging forever.
    fn reap_idle_recordings(&self) {
        let idle: Vec<String> = lock(&self.recordings)
            .iter()
            .filter(|(_, r)| r.last_data.elapsed() > RECORDING_IDLE)
            .map(|(t, _)| t.clone())
            .collect();
        for token in idle {
            self.cancel_recording(&token, tr!("enregistrement interrompu (onglet fermé ou lecture arrêtée)", "recording interrupted (tab closed or playback stopped)"));
        }
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

    /// `None` when the same link was added a moment ago (a double click, a page asking twice).
    fn insert(
        &self,
        url: Url,
        audio: Option<Url>,
        name: &str,
        headers: Vec<(String, String)>,
        resolving: bool,
        existing: Option<ExistingFile>,
    ) -> Option<DownloadId> {
        let mut settings = self.settings();
        if let Some(existing) = existing {
            settings.existing = existing;
        }
        let mut entries = lock(&self.entries);
        let twice = entries.iter().rev().take_while(|e| e.added.is_some_and(|t| t.elapsed() < DUPLICATE_WINDOW)).any(|e| {
            e.download.url == url && e.download.audio == audio && !matches!(e.download.status(), Status::Failed(_))
        });
        if twice {
            return None;
        }
        // A name already known (the page gave it): an existing file may mean "skip".
        let target = if resolving {
            unique_path(&settings.target_dir(name), name, &entries, None)
        } else {
            let Some(target) = target_for(&settings, name, &entries, None) else {
                drop(entries);
                self.notice(false, &trf!("Déjà téléchargé, ignoré : {name}", "Already downloaded, skipped: {name}", name = name));
                return None;
            };
            target
        };
        // Only "overwrite" keeps the name of an existing file.
        let replaces = target.is_file();
        let mut download = Download::new(url, target, settings.connections);
        download.audio = audio;
        let id = download.id;
        let mut entry = Entry::new(download, headers, 0, 0);
        entry.resolving = resolving;
        entry.replaces = replaces;
        entry.added = Some(Instant::now());
        entries.push(entry);
        drop(entries);
        self.changed();
        Some(id)
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

/// The list saved by the previous session. Entries this version cannot read (written by a newer
/// RDM) are left out, not the whole list; an unreadable file is kept aside (`.bad`) rather than
/// overwritten by the next save.
fn load_entries() -> Vec<Entry> {
    let path = crate::settings::config_file(STORE);
    let Ok(bytes) = fs::read(&path) else { return Vec::new() };
    let Ok(values) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) else {
        let _ = fs::copy(&path, with_suffix(&path, ".bad"));
        return Vec::new();
    };
    values
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Stored>(v).ok())
        .map(|mut s| {
            // Interrupted by exit (quit, shutdown, crash): downloads pick up where they were, from
            // their resume point; a recording cannot continue without its browser tab.
            if *s.download.status() == Status::Running {
                let _ = if s.download.is_recording() {
                    s.download.fail(tr!("enregistrement interrompu (RDM fermé)", "recording interrupted (RDM closed)"))
                } else {
                    s.download.retry_later()
                };
            }
            let mut entry = Entry::new(s.download, s.headers, s.downloaded, s.total);
            entry.sha256 = s.sha256;
            entry.scan = s.scan.map_or(Scan::None, Scan::Done);
            entry.replaces = s.replaces;
            entry
        })
        .collect()
}

/// Every temporary file an unfinished download may leave behind, plus the file itself ("").
fn cleanup_suffixes() -> impl Iterator<Item = &'static str> {
    ["", engine::STATE_SUFFIX, ".rdm.tmp", ".video.part.rdm.tmp", ".audio.part.rdm.tmp"]
        .into_iter()
        .chain(engine::PART_SUFFIXES)
}

/// What removing an unfinished download (or deleting its file) erases: its temporary files, and
/// the file itself — unless it existed before ("overwrite") and the transfer has not written over
/// it yet (no resume point next to it: not started, failed before its first byte, or a split
/// download whose parts are still apart). That file is the user's, not this download's.
fn leftovers(target: &Path, delete_file: bool, replaces: bool) -> Vec<PathBuf> {
    let untouched = replaces && !with_suffix(target, engine::STATE_SUFFIX).exists();
    let keep_file = !delete_file && untouched;
    cleanup_suffixes().filter(|s| !(keep_file && s.is_empty())).map(|s| with_suffix(target, s)).collect()
}

/// Chunks of an interrupted recording (`<name>.rec<N>.<track>`), e.g. left by a previous session.
async fn remove_recording_leftovers(target: &Path) {
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else { return };
    let name = name.to_string_lossy();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if is_recording_part(&name, &entry.file_name().to_string_lossy()) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// `<name>.rec<digits>.video` / `.audio` exactly (see `Recording::part`): never another file
/// that merely starts the same way.
fn is_recording_part(name: &str, file: &str) -> bool {
    file.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix(".rec"))
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(ms, track)| !ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit()) && matches!(track, "video" | "audio"))
}


/// Mark-of-the-Web, as browsers do: SmartScreen / Office Protected View then warn before running
/// downloaded executables or macros. Zone only — the source URL is not recorded (privacy).
fn mark_from_internet(path: &Path) {
    if cfg!(windows) {
        let _ = fs::write(with_suffix(path, ":Zone.Identifier"), "[ZoneTransfer]\r\nZoneId=3\r\n");
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
        E::LocalNetwork => tr!("bloqué : un contenu Internet visait votre réseau local", "blocked: Internet content pointing into your local network").into(),
        E::Playlist(m) => trf!("flux vidéo : {m}", "video stream: {m}", m = m),
        E::Mux(e) => trf!("fusion audio/vidéo impossible : {e}", "cannot merge audio and video: {e}", e = e),
    }
}

fn to_header_map(pairs: &[(String, String)]) -> HeaderMap {
    pairs
        .iter()
        .filter_map(|(k, v)| Some((HeaderName::from_bytes(k.as_bytes()).ok()?, HeaderValue::from_str(v).ok()?)))
        .collect()
}

pub fn header_map(req: &AddRequest) -> HeaderMap {
    to_header_map(&req.headers())
}

/// Where a new download of `name` goes, following the settings when a file of that name exists
/// (`None`: skip it). Overwriting never follows a symbolic link (it could point anywhere), nor
/// takes a name another download of the list will write.
fn target_for(settings: &Settings, name: &str, taken: &[Entry], except: Option<DownloadId>) -> Option<PathBuf> {
    let dir = settings.target_dir(name);
    let path = dir.join(name);
    match settings.existing {
        // Asked beforehand when possible (see `resolved`, `add_from_browser`); otherwise a new name.
        ExistingFile::Rename | ExistingFile::Ask => Some(unique_path(&dir, name, taken, except)),
        ExistingFile::Skip if path.is_file() => None,
        ExistingFile::Skip => Some(unique_path(&dir, name, taken, except)),
        ExistingFile::Overwrite => {
            let link = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
            // Another download of the list still writing (or to write) that file keeps it: two
            // transfers into one file would corrupt both. A completed one gives it up.
            let planned = taken.iter().any(|e| {
                e.download.target == path && Some(e.download.id) != except && *e.download.status() != Status::Completed
            });
            if link || planned || path.is_dir() {
                return Some(unique_path(&dir, name, taken, except));
            }
            // Leftovers of an earlier, unrelated download of that name must not be resumed.
            for suffix in cleanup_suffixes().filter(|s| !s.is_empty()) {
                let _ = fs::remove_file(with_suffix(&path, suffix));
            }
            Some(path)
        }
    }
}

/// A free path for `name` in `dir`: neither on disk (with or without leftovers of an unfinished
/// download) nor planned by another entry of the list (`except`: the entry being renamed).
fn unique_path(dir: &Path, name: &str, taken: &[Entry], except: Option<DownloadId>) -> PathBuf {
    let (stem, ext) = name.rsplit_once('.').map_or((name, None), |(s, e)| (s, Some(e)));
    (0u32..)
        .map(|i| match (i, ext) {
            (0, _) => dir.join(name),
            (_, Some(ext)) => dir.join(format!("{stem} ({i}).{ext}")),
            (_, None) => dir.join(format!("{stem} ({i})")),
        })
        .find(|p| {
            !p.exists()
                && !with_suffix(p, engine::STATE_SUFFIX).exists()
                && !taken.iter().any(|e| &e.download.target == p && Some(e.download.id) != except)
        })
        .expect("unbounded range always yields a free name")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_covers_every_temporary_file() {
        let all: Vec<_> = cleanup_suffixes().collect();
        for s in [".rdm", ".video.part", ".video.part.ok", ".audio.part.rdm", ".mux.tmp"] {
            assert!(all.contains(&s), "{s}");
        }
    }

    #[test]
    fn only_recording_chunks_count_as_leftovers() {
        let target = PathBuf::from("dl").join("Clip.mp4");
        let recording = Recording { id: DownloadId::new(), target, parts: HashMap::new(), last_data: Instant::now() };
        let part = recording.part(3, Track::Audio);
        assert!(is_recording_part("Clip.mp4", &part.file_name().unwrap().to_string_lossy()));
        assert!(is_recording_part("Clip.mp4", "Clip.mp4.rec12.video"));
        for other in ["Clip.mp4.recipe.txt", "Clip.mp4.rec.video", "Clip.mp4.rec1.video.bak", "Clip.mp4.rec1x.audio", "Clip.mp4"] {
            assert!(!is_recording_part("Clip.mp4", other), "{other}");
        }
    }

    /// Regression: with "overwrite", removing a download that failed before writing anything
    /// deleted the user's existing file of that name.
    #[test]
    fn removal_spares_a_file_the_download_has_not_written_over() {
        let dir = std::env::temp_dir().join(format!("rdm-leftovers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("report.pdf");
        std::fs::write(&target, b"the user's file").unwrap();
        let erases_file = |delete_file, replaces| leftovers(&target, delete_file, replaces).contains(&target);

        assert!(!erases_file(false, true), "not written over yet: kept");
        assert!(erases_file(true, true), "\"delete the file\" deletes it");
        assert!(erases_file(false, false), "a partial file of this download's own");
        std::fs::write(with_suffix(&target, engine::STATE_SUFFIX), b"[]").unwrap();
        assert!(erases_file(false, true), "the transfer has written over it: a partial file now");
        assert!(leftovers(&target, false, true).contains(&with_suffix(&target, engine::STATE_SUFFIX)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
