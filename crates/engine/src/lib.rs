//! Segmented HTTP download engine (infrastructure layer).

mod hls;
mod merged;
mod pace;
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
pub use probe::{Probe, probe, probe_once, sanitize_file_name, suggest_file_name};
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
    let result = match &job.audio {
        Some(audio) => merged::run(client, job, audio, progress, cancel).await,
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
    /// Connecting failed: the cached address of that server is forgotten, so the next attempt
    /// resolves its name again (the network changed, a VPN went up, the CDN moved).
    pub fn forget_address(&self) {
        if let Self::Http(e) = self
            && e.is_connect()
            && let Some(url) = e.url()
        {
            net::forget(url);
        }
    }

    /// 429 / 503: the server limits concurrent connections.
    pub fn is_throttled(&self) -> bool {
        matches!(self, Self::Http(e) if e.status().is_some_and(|s| s.as_u16() == 429 || s.as_u16() == 503))
    }

    /// The server's TLS certificate is not trusted (self-signed, expired, another name).
    pub fn is_certificate(&self) -> bool {
        let Self::Http(e) = self else { return false };
        let mut source: Option<&dyn std::error::Error> = Some(e);
        while let Some(err) = source {
            if err.to_string().to_ascii_lowercase().contains("certificate") {
                return true;
            }
            source = err.source();
        }
        false
    }

    /// Retrying cannot help (404, 403, blocked redirect, untrusted certificate, disk error…).
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::Http(e) => {
                e.is_redirect()
                    || e.is_builder()
                    || e.status().is_some_and(|s| s.is_client_error() && !matches!(s.as_u16(), 408 | 425 | 429))
                    || self.is_certificate()
                    || net::is_lan_blocked(e)
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
    /// This download's own speed limiter (unlimited unless the user set one).
    pub own_limit: Arc<RateLimit>,
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
            own_limit: Arc::default(),
        }
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

/// How the engine reaches servers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum Route {
    /// Direct connections, even if the environment defines a proxy.
    Direct,
    /// The system's proxy (Windows and macOS settings, `HTTP(S)_PROXY` / `ALL_PROXY` variables).
    #[default]
    System,
    /// This proxy: `http://`, `https://`, `socks5://`, `socks5h://` (names resolved by the proxy),
    /// `socks4://`, `socks4a://`; with a login when `user` is not empty.
    Proxy { url: String, user: String, password: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ClientOptions {
    pub route: Route,
    /// Accept invalid TLS certificates (a download the user explicitly exempted).
    pub insecure: bool,
    /// A download that starts on the Internet: no connection into the local network, whatever the
    /// names it meets resolve to ([`net::PublicDns`]). Not through a proxy, which resolves the
    /// names itself (and may well be on the local network).
    pub public_only: bool,
}

/// The default client: system proxy, certificates checked.
pub fn client() -> reqwest::Result<Client> {
    client_with(&ClientOptions::default())
}

/// Tuned for bulk parallel transfers:
/// - HTTP/1.1 only: HTTP/2 would multiplex every segment onto one TCP stream and kill parallelism;
/// - no compression: `Content-Encoding` on ranged responses breaks offsets, and media is already compressed;
/// - kept-alive pool sized for the max connection count, so stolen segments reuse warm TLS sessions;
/// - names resolved once per minute and shared ([`net::CachedDns`]): 32 connections to one server
///   cost one DNS lookup, not 32 (a slow resolver no longer delays every connection);
/// - redirects may not lead from the Internet into the local network (SSRF), and with
///   `public_only` no name may resolve into it either.
///
/// Privacy: no telemetry, no update ping; a browser-like User-Agent so the client is not fingerprinted as RDM.
pub fn client_with(options: &ClientOptions) -> reqwest::Result<Client> {
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
    let mut builder = Client::builder()
        .http1_only()
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(usize::from(domain::MAX_CONNECTIONS))
        .pool_idle_timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(30))
        .dns_resolver(net::CachedDns::shared())
        .redirect(redirects)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36");
    if options.public_only && !matches!(options.route, Route::Proxy { .. }) {
        builder = builder.dns_resolver(Arc::new(net::PublicDns));
    }
    builder = route(builder, &options.route)?;
    if options.insecure {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build()
}

/// `builder` set up to reach servers by `route` (for RDM's other clients too: updates, VirusTotal).
pub fn route(builder: reqwest::ClientBuilder, route: &Route) -> reqwest::Result<reqwest::ClientBuilder> {
    Ok(match route {
        Route::Direct => builder.no_proxy(),
        Route::System => builder,
        Route::Proxy { url, user, password } => {
            let mut proxy = reqwest::Proxy::all(url.as_str())?;
            // SOCKS4 has no password authentication (reqwest panics if asked for one).
            let socks4 = url.get(..6).is_some_and(|s| s.eq_ignore_ascii_case("socks4"));
            if !user.is_empty() && !socks4 {
                proxy = proxy.basic_auth(user, password);
            }
            builder.no_proxy().proxy(proxy)
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_proxy_kind_builds_with_or_without_a_login() {
        for url in ["http://p:3128", "https://p:3128", "socks5://p:1080", "socks5h://p:1080", "socks4://p:1080", "SOCKS4A://p:1080"] {
            for user in ["", "me"] {
                let route = Route::Proxy { url: url.into(), user: user.into(), password: "pw".into() };
                assert!(client_with(&ClientOptions { route, ..ClientOptions::default() }).is_ok(), "{url} {user}");
            }
        }
    }
}
