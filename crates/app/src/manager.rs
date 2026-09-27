//! Application service: owns the queue, schedules downloads and orchestrates the engine.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::Read,
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
use sha2::{Digest, Sha256};
use tokio::{runtime::Handle, sync::Semaphore};
use url::Url;

use crate::{
    notify,
    settings::{Settings, save_json},
    update,
    virustotal::{self, Report, Stage},
};

const UPDATE_EVERY: Duration = Duration::from_secs(24 * 3600);

const STORE: &str = "downloads.json";
/// Speeds, idle recordings and the list on disk are refreshed at this pace.
const TICK: Duration = Duration::from_millis(500);
/// Samples of the total speed kept for the chart: one minute.
pub const HISTORY: usize = 120;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// A recording that receives nothing for this long is considered abandoned.
const RECORDING_IDLE: Duration = Duration::from_secs(120);
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
}

impl AddRequest {
    pub fn from_url(url: Url) -> Self {
        Self { url, audio_url: None, filename: None, referrer: None, cookies: None, user_agent: None }
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        [("referer", &self.referrer), ("cookie", &self.cookies), ("user-agent", &self.user_agent)]
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_owned(), v.clone().filter(|v| !v.is_empty())?)))
            .collect()
    }
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
    headers: Vec<(String, String)>,
    cancel: Option<CancellationToken>,
    last: u64,
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

type Launch = (DownloadId, engine::Job, Arc<Progress>, CancellationToken);

impl Entry {
    fn new(download: Download, headers: Vec<(String, String)>, downloaded: u64, total: u64) -> Self {
        let progress = Arc::new(Progress::default());
        progress.downloaded.store(downloaded, Relaxed);
        progress.total.store(total, Relaxed);
        let name = download.target.file_name().map_or_else(|| download.url.to_string(), |n| n.to_string_lossy().into_owned());
        Self {
            search_key: name.to_lowercase(),
            name,
            category: download.category(),
            download,
            progress,
            speed: 0.0,
            sha256: None,
            scan: Scan::None,
            headers,
            cancel: None,
            last: downloaded,
        }
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

    /// Queued → running: the engine job, with a fresh cancel token.
    fn start(&mut self, limit: &Arc<RateLimit>) -> Option<Launch> {
        self.download.start().ok()?;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let job = engine::Job {
            url: self.download.url.clone(),
            target: self.download.target.clone(),
            connections: self.download.connections,
            headers: to_header_map(&self.headers),
            audio: self.download.audio.clone(),
            limit: limit.clone(),
        };
        Some((self.download.id, job, self.progress.clone(), cancel))
    }

    /// What goes to disk: never the session secrets.
    fn stored(&self) -> Stored {
        let (downloaded, total, _) = self.progress.snapshot();
        let headers = self.headers.iter().filter(|(k, _)| !SECRET_HEADERS.contains(&k.as_str())).cloned().collect();
        let scan = if let Scan::Done(report) = &self.scan { Some(report.clone()) } else { None };
        Stored { download: self.download.clone(), headers, downloaded, total, sha256: self.sha256.clone(), scan }
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
    client: Client,
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
    virustotal: OnceLock<reqwest::Client>,
    /// One VirusTotal analysis at a time: the free API allows 4 requests per minute.
    scan_gate: Arc<Semaphore>,
    update: Mutex<update::State>,
}

/// Firefox gives each install of an extension a random origin (`moz-extension://<uuid>`), which
/// cannot be pinned like Chrome's: the user approves it once in the RDM window.
#[derive(Default)]
struct FirefoxPairing {
    paired: Option<String>,
    pending: Option<String>,
    /// Refused this session: never asked again until RDM restarts.
    refused: HashSet<String>,
}

const FIREFOX_FILE: &str = "firefox.json";

fn is_firefox_origin(origin: &str) -> bool {
    origin.strip_prefix("moz-extension://").is_some_and(|id| {
        id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    })
}

impl Manager {
    pub fn new(rt: Handle, client: Client) -> Arc<Self> {
        let settings = Settings::load();
        let limit = Arc::new(RateLimit::default());
        limit.set(u64::from(settings.speed_limit_kib) * 1024);
        let this = Arc::new(Self {
            rt,
            client,
            settings: Mutex::new(settings),
            entries: Mutex::new(load_entries()),
            busy: Mutex::default(),
            recordings: Mutex::default(),
            limit,
            inflight: AtomicUsize::new(0),
            repaint: OnceLock::new(),
            show: OnceLock::new(),
            quit: OnceLock::new(),
            firefox: Mutex::new(FirefoxPairing {
                paired: std::fs::read(crate::settings::config_file(FIREFOX_FILE))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<String>(&b).ok())
                    .filter(|o| is_firefox_origin(o)),
                ..FirefoxPairing::default()
            }),
            dirty: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            saved_generation: Mutex::new(0),
            settings_dirty: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            closed: Once::new(),
            history: Mutex::new(std::iter::repeat_n(0.0, HISTORY).collect()),
            virustotal: OnceLock::new(),
            scan_gate: Arc::new(Semaphore::new(1)),
            update: Mutex::default(),
        });
        this.spawn_ticker();
        this.spawn_update_checks();
        this.schedule();
        this
    }

    pub fn client(&self) -> &Client {
        &self.client
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
        if ff.paired.as_deref() == Some(origin) {
            return true;
        }
        if ff.pending.is_none() && !ff.refused.contains(origin) {
            ff.pending = Some(origin.to_owned());
            drop(ff);
            self.show();
            self.repaint();
        }
        false
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
            Some(true) => {
                save_json(FIREFOX_FILE, &origin);
                ff.paired = Some(origin);
            }
            Some(false) => {
                ff.refused.insert(origin);
            }
            None => {}
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
    pub fn apply_settings(self: &Arc<Self>, new: Settings) {
        self.limit.set(u64::from(new.speed_limit_kib) * 1024);
        *lock(&self.settings) = new;
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

    pub fn add(self: &Arc<Self>, req: AddRequest) {
        let this = self.clone();
        self.rt.spawn(async move {
            let headers = req.headers();
            let name = match req.filename.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                Some(n) => engine::sanitize_file_name(n),
                None => this.suggest_name(&req.url, &to_header_map(&headers)).await,
            };
            this.insert(req.url, req.audio_url, &name, headers);
            this.schedule();
        });
    }

    /// Name from the server; HLS gets the extension of what will actually be written.
    async fn suggest_name(&self, url: &Url, headers: &HeaderMap) -> String {
        let Ok(probe) = engine::probe(&self.client, url, headers).await else {
            return engine::suggest_file_name(url, None);
        };
        if !probe.hls {
            return probe.file_name;
        }
        let fmp4 = engine::hls_info(&self.client, url, headers).await.is_ok_and(|i| i.fmp4);
        let stem = probe.file_name.rsplit_once('.').map_or(probe.file_name.as_str(), |(s, _)| s);
        format!("{stem}.{}", if fmp4 { "mp4" } else { "ts" })
    }

    /// ▶ on a paused / failed download: back in the queue.
    pub fn resume(self: &Arc<Self>, id: DownloadId) {
        self.update(id, |e| {
            let _ = e.download.enqueue();
        });
        self.schedule();
    }

    pub fn pause(self: &Arc<Self>, id: DownloadId) {
        self.update(id, |e| {
            // A recording follows the browser's playback: it cannot be paused from here.
            if !e.download.is_recording()
                && e.download.pause().is_ok()
                && let Some(cancel) = e.cancel.take()
            {
                cancel.cancel();
            }
        });
        self.schedule();
    }

    /// One pass under one lock (queued downloads are paused too: nothing starts afterwards).
    pub fn pause_all(&self) {
        for e in lock(&self.entries).iter_mut().filter(|e| !e.download.is_recording()) {
            if e.download.pause().is_ok()
                && let Some(cancel) = e.cancel.take()
            {
                cancel.cancel();
            }
        }
        self.changed();
    }

    /// Paused and failed downloads go back in the queue, in their list order.
    pub fn resume_all(self: &Arc<Self>) {
        for e in lock(&self.entries).iter_mut() {
            let _ = e.download.enqueue();
        }
        self.changed();
        self.schedule();
    }

    /// Removes from the list; `delete_file` also erases the file (always for unfinished parts).
    pub fn remove(self: &Arc<Self>, id: DownloadId, delete_file: bool) {
        if let Some(token) = self.token_of(id) {
            self.cancel_recording(&token, "annulé");
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
                let target = e.download.target.clone();
                self.rt.spawn(async move {
                    // Wait for the task to stop writing (bounded), then clean up.
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while lock(&this.busy).contains(&id) && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    for suffix in cleanup_suffixes() {
                        let _ = tokio::fs::remove_file(with_suffix(&target, suffix)).await;
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
            if let Ok(hash) = sha256_file(&path) {
                this.update(id, |e| e.sha256 = Some(hash));
            }
        });
    }

    /// Client for VirusTotal and GitHub (not the download engine's: see `virustotal::client`).
    fn web(&self) -> Option<reqwest::Client> {
        if let Some(c) = self.virustotal.get() {
            return Some(c.clone());
        }
        let client = virustotal::client().ok()?;
        Some(self.virustotal.get_or_init(|| client).clone())
    }

    pub fn update_state(&self) -> update::State {
        lock(&self.update).clone()
    }

    fn set_update(&self, state: update::State) {
        *lock(&self.update) = state;
        self.repaint();
    }

    /// At start (after a few seconds) and once a day, if enabled in the settings.
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
                tokio::time::sleep(UPDATE_EVERY).await;
            }
        });
    }

    /// Asks GitHub for a newer release. `manual`: the user clicked, so "up to date" and errors show.
    pub fn check_updates(self: &Arc<Self>, manual: bool) {
        let current = self.update_state();
        if matches!(current, update::State::Checking | update::State::Downloading(_))
            || (!manual && matches!(current, update::State::Available(_)))
        {
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

    /// Windows: downloads and installs the new `.msi`, then quits (the installer restarts RDM).
    /// `false` when there is no installer to run (the UI then opens the release page).
    pub fn install_update(self: &Arc<Self>) -> bool {
        let update::State::Available(release) = self.update_state() else { return false };
        let Some(url) = release.msi.clone().filter(|_| cfg!(windows)) else { return false };
        self.set_update(update::State::Downloading(0.0));
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(client) = this.web() else { return };
            let progress = {
                let this = this.clone();
                move |f: f32| *lock(&this.update) = update::State::Downloading(f)
            };
            match update::download(&client, &url, progress).await {
                Ok(msi) => match update::install_after_exit(&msi) {
                    Ok(()) => this.request_quit(),
                    Err(e) => this.set_update(update::State::Failed(format!("installation impossible : {e}"))),
                },
                Err(reason) => this.set_update(update::State::Failed(reason)),
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
                        let hash = tokio::task::spawn_blocking(move || sha256_file(&file))
                            .await
                            .map_err(|_| virustotal::Error::Io)?
                            .map_err(|_| virustotal::Error::Io)?;
                        this.update(id, |e| e.sha256 = Some(hash.clone()));
                        hash
                    }
                };
                let client = match this.virustotal.get() {
                    Some(client) => client.clone(),
                    None => {
                        let client = virustotal::client().map_err(|_| virustotal::Error::Network)?;
                        this.virustotal.get_or_init(|| client).clone()
                    }
                };
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
        let target = unique_path(&settings.target_dir(&name), &name, &entries);
        let download = Download::recording(page, target.clone());
        let id = download.id;
        entries.push(Entry::new(download, Vec::new(), 0, 0));
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
        self.rt.spawn_blocking(move || {
            let result = match r.best_source() {
                Some(ms) => engine::mux::merge(&r.part(ms, Track::Video), &r.part(ms, Track::Audio), &r.target)
                    .map_err(|e| format!("fusion audio/vidéo impossible : {e}")),
                None => Err("aucune donnée vidéo et audio reçue".to_owned()),
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
            self.cancel_recording(&token, "enregistrement interrompu (onglet fermé ou lecture arrêtée)");
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
        let max = usize::from(self.with_settings(|s| s.max_parallel));
        let launches: Vec<Launch> = {
            let mut entries = lock(&self.entries);
            let mut busy = lock(&self.busy);
            let mut free = max.saturating_sub(entries.iter().filter(|e| e.occupies_slot()).count());
            let mut launches = Vec::new();
            for e in entries.iter_mut() {
                if free == 0 {
                    break;
                }
                if *e.download.status() != Status::Queued || busy.contains(&e.download.id) {
                    continue;
                }
                if let Some(launch) = e.start(&self.limit) {
                    busy.insert(launch.0);
                    launches.push(launch);
                    free -= 1;
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

    fn run(self: &Arc<Self>, (id, job, progress, cancel): Launch) {
        self.inflight.fetch_add(1, AcqRel);
        let this = self.clone();
        self.rt.spawn(async move {
            let result = engine::run(&this.client, &job, progress, cancel).await;
            let mut finished = None;
            this.update(id, |e| {
                e.cancel = None;
                let _ = match &result {
                    Ok(Outcome::Completed) => {
                        mark_from_internet(&e.download.target);
                        let _ = e.download.start(); // a pause may have raced the last byte
                        finished = Some(e.name.clone());
                        e.download.complete()
                    }
                    Ok(Outcome::Paused) => Ok(()),
                    Err(err) => e.download.fail(describe(err)),
                };
            });
            lock(&this.busy).remove(&id);
            this.inflight.fetch_sub(1, AcqRel);
            if let Some(name) = finished
                && this.with_settings(|s| s.notify)
            {
                notify::completed(&name);
            }
            this.schedule();
        });
    }

    fn insert(&self, url: Url, audio: Option<Url>, name: &str, headers: Vec<(String, String)>) -> DownloadId {
        let settings = self.settings();
        let mut entries = lock(&self.entries);
        let target = unique_path(&settings.target_dir(name), name, &entries);
        let mut download = Download::new(url, target, settings.connections);
        download.audio = audio;
        let id = download.id;
        entries.push(Entry::new(download, headers, 0, 0));
        drop(entries);
        self.changed();
        id
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
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(this) = weak.upgrade() else { return };
                this.reap_idle_recordings();
                if this.tick() {
                    this.repaint();
                }
                if this.dirty.load(Acquire) {
                    let _ = tokio::task::spawn_blocking(move || this.persist()).await;
                }
            }
        });
    }

    /// Updates speeds and the history; `true` while the UI has something moving to show (a running
    /// download, or the chart still scrolling back to zero).
    fn tick(&self) -> bool {
        let (mut active, mut total) = (false, 0.0);
        for e in lock(&self.entries).iter_mut() {
            let (done, _, _) = e.progress.snapshot();
            let running = *e.download.status() == Status::Running;
            let instant = done.saturating_sub(e.last) as f64 / TICK.as_secs_f64();
            e.speed = if running { e.speed * 0.6 + instant * 0.4 } else { 0.0 };
            e.last = done;
            active |= running;
            total += e.speed;
        }
        let mut history = lock(&self.history);
        history.pop_front();
        history.push_back(total as f32);
        active || history.iter().any(|&s| s > 0.0)
    }
}

fn load_entries() -> Vec<Entry> {
    let stored: Vec<Stored> = fs::read(crate::settings::config_file(STORE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    stored
        .into_iter()
        .map(|mut s| {
            // Interrupted by exit: downloads are resumable and wait for the user; a recording
            // cannot continue without its browser tab.
            if *s.download.status() == Status::Running {
                let _ = if s.download.is_recording() {
                    s.download.fail("enregistrement interrompu (RDM fermé)")
                } else {
                    s.download.pause()
                };
            }
            let mut entry = Entry::new(s.download, s.headers, s.downloaded, s.total);
            entry.sha256 = s.sha256;
            entry.scan = s.scan.map_or(Scan::None, Scan::Done);
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

/// Chunks of an interrupted recording (`<name>.rec<N>.<track>`), e.g. left by a previous session.
async fn remove_recording_leftovers(target: &Path) {
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else { return };
    let prefix = format!("{}.rec", name.to_string_lossy());
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut buf)? {
            0 => break,
            n => hasher.update(&buf[..n]),
        }
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Mark-of-the-Web, as browsers do: SmartScreen / Office Protected View then warn before running
/// downloaded executables or macros. Zone only — the source URL is not recorded (privacy).
fn mark_from_internet(path: &Path) {
    if cfg!(windows) {
        let _ = fs::write(with_suffix(path, ":Zone.Identifier"), "[ZoneTransfer]\r\nZoneId=3\r\n");
    }
}

/// Engine error → short French sentence for the user (no URLs: they may carry tokens).
fn describe(err: &engine::EngineError) -> String {
    use engine::EngineError as E;
    match err {
        E::Http(e) if e.is_redirect() => "redirection refusée (boucle ou vers le réseau local)".into(),
        E::Http(e) => match e.status().map(|s| s.as_u16()) {
            Some(401 | 403) => "accès refusé par le serveur (lien expiré ou protégé)".into(),
            Some(404) => "fichier introuvable (404)".into(),
            Some(410) => "lien expiré (410)".into(),
            Some(429) => "le serveur limite les connexions (429)".into(),
            Some(s @ 500..=599) => format!("erreur du serveur ({s})"),
            Some(s) => format!("le serveur a répondu {s}"),
            None if e.is_timeout() => "délai dépassé, connexion trop lente".into(),
            None if e.is_connect() => "connexion impossible au serveur".into(),
            None => "connexion interrompue".into(),
        },
        E::Io(e) => format!("erreur disque : {e}"),
        E::RangeIgnored => "le serveur a renvoyé une plage incohérente".into(),
        E::Empty => "le serveur n'a renvoyé aucune donnée (lien expiré ou protégé)".into(),
        E::Truncated => "connexion coupée avant la fin".into(),
        E::LocalNetwork => "bloqué : un contenu Internet visait votre réseau local".into(),
        E::Playlist(m) => format!("flux vidéo : {m}"),
        E::Mux(e) => format!("fusion audio/vidéo impossible : {e}"),
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

fn unique_path(dir: &Path, name: &str, taken: &[Entry]) -> PathBuf {
    let (stem, ext) = name.rsplit_once('.').map_or((name, None), |(s, e)| (s, Some(e)));
    (0u32..)
        .map(|i| match (i, ext) {
            (0, _) => dir.join(name),
            (_, Some(ext)) => dir.join(format!("{stem} ({i}).{ext}")),
            (_, None) => dir.join(format!("{stem} ({i})")),
        })
        .find(|p| !p.exists() && !taken.iter().any(|e| &e.download.target == p))
        .expect("unbounded range always yields a free name")
}

fn with_suffix(target: &Path, suffix: &str) -> PathBuf {
    let mut p = target.as_os_str().to_owned();
    p.push(suffix);
    p.into()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_known_content() {
        let path = std::env::temp_dir().join(format!("rdm-sha-{}", std::process::id()));
        fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn cleanup_covers_every_temporary_file() {
        let all: Vec<_> = cleanup_suffixes().collect();
        for s in [".rdm", ".video.part", ".video.part.ok", ".audio.part.rdm", ".mux.tmp"] {
            assert!(all.contains(&s), "{s}");
        }
    }
}
