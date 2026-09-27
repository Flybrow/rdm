//! Segmented HTTP download engine (infrastructure layer).

mod hls;
mod merged;
pub mod mux;
pub mod net;
mod probe;
mod rate;
mod slots;
mod transfer;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use url::Url;

pub use hls::{HlsInfo, Variant};
pub use merged::PART_SUFFIXES;
pub use probe::{Probe, probe, sanitize_file_name, suggest_file_name};
pub use rate::RateLimit;
pub use reqwest::{
    Client,
    header::{self, HeaderMap, HeaderValue},
};
pub use tokio_util::sync::CancellationToken;

/// Resume-state file written next to an unfinished download.
pub const STATE_SUFFIX: &str = ".rdm";
const MAX_REDIRECTS: usize = 10;

/// Downloads `job` to completion, or until `cancel` fires (resumable state is then kept next to the file).
pub async fn run(
    client: &Client,
    job: &Job,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    match &job.audio {
        Some(audio) => merged::run(client, job, audio, progress, cancel).await,
        None => transfer::run(client, job, progress, cancel).await,
    }
}

/// Qualities offered by an HLS playlist (for a quality picker) and its container.
pub async fn hls_info(client: &Client, url: &Url, headers: &HeaderMap) -> Result<HlsInfo, EngineError> {
    hls::info(client, url, headers).await
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("server ignored range request")]
    RangeIgnored,
    #[error("server sent no data")]
    Empty,
    #[error("connection closed before the end")]
    Truncated,
    #[error("blocked: Internet content pointing into the local network")]
    LocalNetwork,
    #[error("playlist: {0}")]
    Playlist(&'static str),
    #[error("merge: {0}")]
    Mux(#[from] mux::MuxError),
}

impl EngineError {
    /// 429 / 503: the server limits concurrent connections.
    pub fn is_throttled(&self) -> bool {
        matches!(self, Self::Http(e) if e.status().is_some_and(|s| s.as_u16() == 429 || s.as_u16() == 503))
    }

    /// Retrying cannot help (404, 403, blocked redirect, disk error, bad playlist…).
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::Http(e) => {
                e.is_redirect()
                    || e.is_builder()
                    || e.status().is_some_and(|s| s.is_client_error() && !matches!(s.as_u16(), 408 | 425 | 429))
            }
            Self::Io(_) | Self::Empty | Self::LocalNetwork | Self::Playlist(_) | Self::Mux(_) => true,
            Self::RangeIgnored | Self::Truncated => false,
        }
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
}

impl Job {
    pub fn new(url: Url, target: PathBuf) -> Self {
        Self {
            url,
            target,
            connections: domain::DEFAULT_CONNECTIONS,
            headers: HeaderMap::new(),
            audio: None,
            limit: Arc::default(),
        }
    }
}

#[derive(Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    pub active: AtomicUsize,
}

impl Progress {
    pub fn snapshot(&self) -> (u64, u64, usize) {
        (
            self.downloaded.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
            self.active.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Paused,
}

/// Tuned for bulk parallel transfers:
/// - HTTP/1.1 only: HTTP/2 would multiplex every segment onto one TCP stream and kill parallelism;
/// - no compression: `Content-Encoding` on ranged responses breaks offsets, and media is already compressed;
/// - kept-alive pool sized for the max connection count, so stolen segments reuse warm TLS sessions;
/// - redirects may not lead from the Internet into the local network (SSRF).
///
/// Privacy: no telemetry, no update ping; a browser-like User-Agent so the client is not fingerprinted as RDM.
pub fn client() -> reqwest::Result<Client> {
    let redirects = reqwest::redirect::Policy::custom(|attempt| {
        let hops = attempt.previous().len();
        let escapes = attempt.previous().last().is_some_and(|from| !net::allowed_hop(from, attempt.url()));
        if hops > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if escapes {
            attempt.error("redirect into the local network blocked")
        } else {
            attempt.follow()
        }
    });
    Client::builder()
        .http1_only()
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(usize::from(domain::MAX_CONNECTIONS))
        .pool_idle_timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .redirect(redirects)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36")
        .build()
}

pub(crate) fn state_path(target: &Path) -> PathBuf {
    with_suffix(target, STATE_SUFFIX)
}

/// `path` + `suffix`, without treating anything as an extension (`a.mp4` → `a.mp4.rdm`).
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    p.into()
}
