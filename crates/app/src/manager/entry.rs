//! One download of the list: its state as the window shows it, what starts it, and what is saved.

use super::*;

/// Never written to disk: session cookies stay in memory only.
const SECRET_HEADERS: [&str; 2] = ["cookie", "authorization"];

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
    pub(super) headers: Vec<(String, String)>,
    pub(super) cancel: Option<CancellationToken>,
    pub(super) last: u64,
    /// Automatic retries in a row without progress.
    pub(super) retries: u32,
    /// When it was added in this session (`None`: loaded from disk).
    pub(super) added: Option<Instant>,
    /// Its own speed limiter, shared with its running job: a change applies at once.
    pub(super) own_limit: Arc<RateLimit>,
    /// When it last started, and since when it has been too slow (automatic proxy).
    pub(super) started: Option<Instant>,
    pub(super) slow_since: Option<Instant>,
    /// Stopped to start again right away (new route, new link, certificate choice).
    pub(super) restart: bool,
    /// Its target was an existing file, to overwrite ("overwrite" in the settings): removing the
    /// download must not take that file along before the transfer wrote over it.
    pub(super) replaces: bool,
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
pub(super) struct Launch {
    pub(super) id: DownloadId,
    pub(super) job: engine::Job,
    pub(super) progress: Arc<Progress>,
    pub(super) cancel: CancellationToken,
    pub(super) via_proxy: bool,
    pub(super) insecure: bool,
}

impl Entry {
    pub(super) fn new(download: Download, headers: Vec<(String, String)>, downloaded: u64, total: u64) -> Self {
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
    pub(super) fn restart(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            self.restart = true;
            cancel.cancel();
        }
    }

    /// Derives what the list shows from the target (after creation, or a rename before start).
    pub(super) fn named(&mut self) {
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
    pub(super) fn occupies_slot(&self) -> bool {
        *self.download.status() == Status::Running && !self.download.is_recording()
    }

    /// A recording follows the browser's playback: it cannot be paused from here.
    pub(super) fn pause(&mut self) {
        if !self.download.is_recording() && self.download.pause().is_ok() {
            self.retry = None;
            if let Some(cancel) = self.cancel.take() {
                cancel.cancel();
            }
        }
    }

    /// The user asked: no waiting for an automatic retry, and a fresh retry budget.
    pub(super) fn resume(&mut self) {
        if self.download.enqueue().is_ok() || self.retry.is_some() {
            self.retry = None;
            self.retries = 0;
        }
    }

    /// Can the scheduler start it now?
    pub(super) fn startable(&self, now: Instant) -> bool {
        *self.download.status() == Status::Queued && !self.resolving && self.retry.as_ref().is_none_or(|r| r.at <= now)
    }

    /// Queued → running: the engine job, with a fresh cancel token.
    pub(super) fn start(&mut self, limit: &Arc<RateLimit>) -> Option<Launch> {
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
    pub(super) fn stored(&self) -> Stored {
        let (downloaded, total, _) = self.progress.snapshot();
        let headers = self.headers.iter().filter(|(k, _)| !SECRET_HEADERS.contains(&k.as_str())).cloned().collect();
        let scan = if let Scan::Done(report) = &self.scan { Some(report.clone()) } else { None };
        Stored { download: self.download.clone(), headers, downloaded, total, sha256: self.sha256.clone(), scan, replaces: self.replaces }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Stored {
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

/// The list saved by the previous session. Entries this version cannot read (written by a newer
/// RDM) are left out, not the whole list; an unreadable file is kept aside (`.bad`) rather than
/// overwritten by the next save.
pub(super) fn load_entries() -> Vec<Entry> {
    load_json::<Vec<serde_json::Value>>(STORE)
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

pub(super) fn to_header_map(pairs: &[(String, String)]) -> HeaderMap {
    pairs
        .iter()
        .filter_map(|(k, v)| Some((HeaderName::from_bytes(k.as_bytes()).ok()?, HeaderValue::from_str(v).ok()?)))
        .collect()
}
