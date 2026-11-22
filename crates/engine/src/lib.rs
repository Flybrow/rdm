//! Segmented HTTP download engine (infrastructure layer).

mod client;
mod error;
mod hls;
mod merged;
pub mod mux;
pub mod net;
mod pace;
mod probe;
mod rate;
mod slots;
mod transfer;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use url::Url;

pub use client::{ClientOptions, Route, client, client_with, route};
pub use error::EngineError;
pub use hls::{HlsInfo, PlaylistError, Variant};
pub use merged::PART_SUFFIXES;
pub use probe::{Probe, probe, probe_once, sanitize_file_name, suggest_file_name};
pub use rate::RateLimit;
pub use reqwest::{
    Client,
    header::{self, HeaderMap, HeaderValue},
};
pub use tokio_util::sync::CancellationToken;

/// Resume-state file written next to an unfinished download.
pub const STATE_SUFFIX: &str = ".rdm";

/// Downloads `job` to completion, or until `cancel` fires (resumable state is then kept next to the file).
pub async fn run(client: &Client, job: &Job, progress: Arc<Progress>, cancel: CancellationToken) -> Result<Outcome, EngineError> {
    // Boxed: two transfers side by side make a large future, which every download's task would
    // otherwise carry.
    let result = match &job.audio {
        Some(audio) => Box::pin(merged::run(client, job, audio, progress, cancel)).await,
        None => transfer::run(client, job, progress, cancel).await,
    };
    match result {
        // Said plainly: a name of the Internet content resolved into the local network.
        Err(EngineError::Http(e)) if net::is_lan_blocked(&e) => Err(EngineError::LocalNetwork),
        Err(e) => {
            e.forget_address();
            Err(e)
        }
        ok => ok,
    }
}

/// Qualities offered by an HLS playlist (for a quality picker) and its container.
pub async fn hls_info(client: &Client, url: &Url, headers: &HeaderMap) -> Result<HlsInfo, EngineError> {
    hls::info(client, url, headers).await
}

/// `Content-Range: bytes <start>-<end>/<total>` of a partial answer: where it starts, and the
/// inclusive end, and the file's size when the server tells it (`*`: unknown).
pub(crate) fn content_range(headers: &HeaderMap) -> Option<(u64, u64, Option<u64>)> {
    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?.trim();
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end) = (start.trim().parse::<u64>().ok()?, end.trim().parse::<u64>().ok()?);
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse::<u64>().ok()?),
    };
    (start <= end && end < u64::MAX && total.is_none_or(|size| end < size)).then_some((start, end, total))
}

/// How long a server asks to wait before the next request (`Retry-After`: seconds, or a date).
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    match value.parse::<u64>() {
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => httpdate::parse_http_date(value).ok()?.duration_since(std::time::SystemTime::now()).ok(),
    }
}

#[derive(Clone)]
pub struct Job {
    pub url: Url,
    pub target: PathBuf,
    pub connections: u8,
    /// Browser context (Cookie, Referer, User-Agent) so protected media links keep working.
    pub headers: HeaderMap,
    /// Separate audio stream to mux with `url` (video-only) into `target`.
    pub audio: Option<Url>,
    /// Global speed limiter, shared across jobs.
    pub limit: Arc<RateLimit>,
    /// This download's own speed limiter (unlimited unless the user set one).
    pub own_limit: Arc<RateLimit>,
}

impl Job {
    pub fn new(url: Url, target: PathBuf) -> Self {
        Self { url, target, connections: domain::DEFAULT_CONNECTIONS, headers: HeaderMap::new(), audio: None, limit: Arc::default(), own_limit: Arc::default() }
    }

    /// The tighter of the global and the download's own limit (bytes per second, 0 = none).
    pub fn effective_limit(limit: &RateLimit, own: &RateLimit) -> u64 {
        match (limit.get(), own.get()) {
            (0, o) => o,
            (g, 0) => g,
            (g, o) => g.min(o),
        }
    }
}

#[derive(Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    pub active: AtomicUsize,
    /// The file's pieces while a segmented transfer runs, a few times a second, in file order with
    /// finished neighbours joined (about one per connection); empty otherwise. For the progress bar.
    pieces: Mutex<Vec<domain::Segment>>,
    /// Whether the server serves parts of the file (a pause resumes): 0 not known yet, 1 yes, 2 no.
    resumable: AtomicU8,
}

impl Progress {
    pub fn pieces(&self) -> Vec<domain::Segment> {
        self.pieces.lock().map(|p| p.clone()).unwrap_or_default()
    }

    fn set_pieces(&self, pieces: Vec<domain::Segment>) {
        if let Ok(mut p) = self.pieces.lock() {
            *p = pieces;
        }
    }

    /// Whether a pause would resume where it stopped (`None` until the server answered).
    pub fn resumable(&self) -> Option<bool> {
        match self.resumable.load(Ordering::Relaxed) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }

    pub(crate) fn set_resumable(&self, ranges: bool) {
        self.resumable.store(if ranges { 1 } else { 2 }, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64, usize) {
        (self.downloaded.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed), self.active.load(Ordering::Relaxed))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Paused,
}

pub(crate) fn state_path(target: &Path) -> PathBuf {
    with_suffix(target, STATE_SUFFIX)
}

/// Writes `bytes` to `path` atomically (temp file + rename, data synced first): a crash mid-write
/// never leaves a torn file.
pub(crate) async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let tmp = with_suffix(path, ".tmp");
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(tmp, path).await
}

/// `path` + `suffix`, without treating anything as an extension (`a.mp4` → `a.mp4.rdm`).
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    p.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_retry_after() {
        let with = |v: &str| HeaderMap::from_iter([(header::RETRY_AFTER, HeaderValue::from_str(v).unwrap())]);
        assert_eq!(retry_after(&with("10")), Some(Duration::from_secs(10)));
        assert_eq!(retry_after(&with(" 3 ")), Some(Duration::from_secs(3)));
        let later = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(120));
        assert!(retry_after(&with(&later)).is_some_and(|d| d > Duration::from_secs(100)));
        assert_eq!(retry_after(&with("Wed, 21 Oct 2015 07:28:00 GMT")), None, "a date in the past: no wait");
        assert_eq!(retry_after(&with("soon")), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn parses_content_range() {
        let range = |v: &str| content_range(&HeaderMap::from_iter([(header::CONTENT_RANGE, v.parse().unwrap())]));
        assert_eq!(range("bytes 1024-2047/4096"), Some((1024, 2047, Some(4096))));
        assert_eq!(range("bytes 0-0/*"), Some((0, 0, None)), "size unknown");
        assert_eq!(range("bytes */4096"), None, "no range at all");
        assert_eq!(range("items 0-1/2"), None);
        for invalid in ["bytes 10-9/100", "bytes 0-100/100", "bytes 0-0/0", "bytes 0-x/100", "bytes 0-0/garbage", "bytes 0-18446744073709551615/*"] {
            assert_eq!(range(invalid), None, "{invalid}");
        }
        assert_eq!(content_range(&HeaderMap::new()), None);
    }

    #[test]
    fn the_tighter_of_the_two_speed_limits_applies() {
        let limit = |bytes_per_second| {
            let limit = RateLimit::default();
            limit.set(bytes_per_second);
            limit
        };

        assert_eq!(Job::effective_limit(&limit(0), &limit(0)), 0, "no limit at all");
        assert_eq!(Job::effective_limit(&limit(4096), &limit(0)), 4096);
        assert_eq!(Job::effective_limit(&limit(0), &limit(2048)), 2048);
        assert_eq!(Job::effective_limit(&limit(4096), &limit(2048)), 2048);
        assert_eq!(Job::effective_limit(&limit(2048), &limit(4096)), 2048);
    }

    #[test]
    fn progress_reads_back_what_the_transfer_published() {
        let progress = Progress::default();
        progress.downloaded.store(300, Ordering::Relaxed);
        progress.total.store(1000, Ordering::Relaxed);
        progress.active.store(4, Ordering::Relaxed);
        let piece = domain::Segment::new(0, 999);

        progress.set_pieces(vec![piece]);

        assert_eq!(progress.snapshot(), (300, 1000, 4));
        assert_eq!(progress.pieces(), vec![piece]);
    }

    #[test]
    fn the_resume_state_sits_next_to_the_file() {
        assert_eq!(state_path(Path::new("dir/a.mp4")), PathBuf::from("dir/a.mp4.rdm"));
    }

    /// Regression: a download's task held the future of a split (video + audio) download inline,
    /// 128 KB whatever the download, because nested futures are laid out side by side.
    #[test]
    fn a_download_task_stays_small() {
        let client = client().unwrap();
        let job = Job::new("https://x.io/a".parse().unwrap(), "a".into());
        let task = run(&client, &job, Arc::default(), CancellationToken::new());
        assert!(size_of_val(&task) < 8 << 10, "{} bytes", size_of_val(&task));
    }
}
