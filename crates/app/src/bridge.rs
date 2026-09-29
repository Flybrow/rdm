//! Loopback HTTP bridge the browser extension talks to.
//!
//! Threat model: any web page, any other browser extension, and any other account of the computer
//! can send requests to 127.0.0.1. Defences:
//! - `Origin`: browsers always set it on cross-origin requests; only *our* extension ID passes
//!   (pinned by the manifest `key`); Firefox extensions have a random per-install origin: paired
//!   by the native connector (`POST /pair`), or else accepted once the user approved it (Firefox
//!   sends `Origin` on POST only: the extension asks with `POST /ping` when it starts);
//! - no `Origin` (a local program — the native connector, `rdm --quit`, a second launch): only
//!   with the token RDM writes in the user's private files (see `local`); without it, only
//!   `GET /ping` and `GET /config` (the Firefox extension's GETs carry no `Origin`);
//! - `Host`: must be the loopback address, which defeats DNS rebinding;
//! - JSON bodies (force a CORS preflight we never answer), 64 KiB cap, http(s) URLs only.

use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{
        HeaderMap, StatusCode,
        header::{HOST, ORIGIN},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use url::Url;

use crate::{
    local,
    manager::{self, AddRequest, Manager, Track},
    settings::BRIDGE_PORT,
};

/// ID of the RDM extension, derived from the public `key` in `extension/manifest.json`.
pub const EXTENSION_ORIGIN: &str = "chrome-extension://cgailhenfaoohkakpdacohcnmppepjjl";
const MAX_BODY: usize = 64 * 1024;
const MAX_CHUNK: usize = 32 << 20;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn bind() -> std::io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, BRIDGE_PORT)).await
}

pub async fn serve(manager: Arc<Manager>, listener: TcpListener) {
    let app = Router::new()
        // POST: Firefox sends `Origin` on POST only, so this is how its extension pairs.
        .route("/ping", get(ping_read).post(ping))
        .route("/config", get(config))
        .route("/add", post(add))
        .route("/probe", post(probe))
        .route("/check", post(check))
        .route("/show", post(show))
        .route("/quit", post(quit))
        .route("/pair", post(pair))
        .route("/youtube/state", post(youtube_state))
        .route("/youtube/install", post(youtube_install))
        .route("/youtube/extract", post(youtube_extract))
        .route("/record/start", post(record_start))
        .route("/record/{token}/progress", post(record_progress))
        .route("/record/{token}/finish", post(record_finish))
        .route("/record/{token}/cancel", post(record_cancel))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        // Media chunks from the browser's player: the only route with large bodies.
        .route("/record/{token}/append", post(record_append).layer(DefaultBodyLimit::max(MAX_CHUNK)))
        .with_state(manager);
    let _ = axum::serve(listener, app).await;
}

fn guard(manager: &Manager, headers: &HeaderMap) -> Result<(), StatusCode> {
    guard_access(manager, headers, Access::Act)
}

/// What a request carrying no `Origin` may do without the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// Only learn what any program of the computer may know (RDM is here, the formats it takes):
    /// the Firefox extension's `GET`s carry no `Origin`.
    Read,
    /// Anything else: the browsers' own origins, or a local program holding the token.
    Act,
}

fn guard_access(manager: &Manager, headers: &HeaderMap, access: Access) -> Result<(), StatusCode> {
    let token = local::served_token().unwrap_or_default();
    guard_with(headers, token, access, |origin| manager.firefox_allowed(origin))?;
    // Which browser the extension runs in (the extension window shows the connected ones); sent
    // by the extension itself or its native connector. A web page cannot set this header; an
    // anonymous read (any program of the computer) is not taken at its word either.
    let proven = headers.contains_key(ORIGIN) || guard_with(headers, token, Access::Act, |_| false).is_ok();
    if proven && let Some(browser) = headers.get(BROWSER).and_then(|v| v.to_str().ok()) {
        manager.browser_seen(browser);
    }
    Ok(())
}

/// Sent by the extension: `firefox`, `waterfox`, `chrome`, `brave`, `opera`, `edge` or `chromium`.
const BROWSER: &str = "x-rdm-browser";

/// `firefox`: whether a (`moz-extension://…`) origin was approved by the user. Such an origin not
/// approved (yet) gets 401 — the extension then says "approve me in RDM", not "RDM is not
/// running" — anything else untrusted gets 403.
fn guard_with(headers: &HeaderMap, token: &str, access: Access, firefox: impl FnOnce(&str) -> bool) -> Result<(), StatusCode> {
    let host_ok = headers.get(HOST).and_then(|h| h.to_str().ok()).is_some_and(|h| {
        let host = h.rsplit_once(':').map_or(h, |(host, _)| host);
        matches!(host, "127.0.0.1" | "localhost")
    });
    if !host_ok {
        return Err(StatusCode::FORBIDDEN);
    }
    match headers.get(ORIGIN).map(|o| o.to_str()) {
        // A local program: only the user's own (it could read the token in the user's files).
        None if access == Access::Read => Ok(()),
        None if !token.is_empty() && local::token_matches(token, headers.get(local::TOKEN_HEADER).map(|v| v.as_bytes())) => Ok(()),
        None => Err(StatusCode::FORBIDDEN),
        Some(Ok(o)) if o == EXTENSION_ORIGIN => Ok(()),
        Some(Ok(o)) if o.starts_with("moz-extension://") => {
            if firefox(o) { Ok(()) } else { Err(StatusCode::UNAUTHORIZED) }
        }
        Some(_) => Err(StatusCode::FORBIDDEN),
    }
}

fn web(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

async fn ping(State(manager): State<Arc<Manager>>, headers: HeaderMap) -> Result<&'static str, StatusCode> {
    guard(&manager, &headers)?;
    Ok(concat!("rdm ", env!("CARGO_PKG_VERSION")))
}

/// `GET /ping`: is RDM here (the Firefox extension's `GET`s carry no `Origin`).
async fn ping_read(State(manager): State<Arc<Manager>>, headers: HeaderMap) -> Result<&'static str, StatusCode> {
    guard_access(&manager, &headers, Access::Read)?;
    Ok(concat!("rdm ", env!("CARGO_PKG_VERSION")))
}

#[derive(Serialize)]
struct Config {
    /// Extensions the browser should hand over (space-separated), from the user's settings.
    captured: String,
}

async fn config(State(manager): State<Arc<Manager>>, headers: HeaderMap) -> Result<Json<Config>, StatusCode> {
    guard_access(&manager, &headers, Access::Read)?;
    Ok(Json(Config { captured: manager.settings().captured }))
}

async fn add(State(manager): State<Arc<Manager>>, headers: HeaderMap, Json(req): Json<AddRequest>) -> StatusCode {
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    if !web(&req.url) || req.audio_url.as_ref().is_some_and(|u| !web(u)) {
        return StatusCode::BAD_REQUEST;
    }
    manager.add_from_browser(req);
    StatusCode::ACCEPTED
}

/// Qualities of an HLS stream, for the extension's quality menu (fetched with the page's cookies).
async fn probe(
    State(manager): State<Arc<Manager>>,
    headers: HeaderMap,
    Json(req): Json<AddRequest>,
) -> Result<Json<engine::HlsInfo>, StatusCode> {
    guard(&manager, &headers)?;
    if !web(&req.url) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let request_headers = manager::header_map(&req);
    let client = manager.client(&req.url).await;
    let info = engine::hls_info(&client, &req.url, &request_headers);
    match tokio::time::timeout(PROBE_TIMEOUT, info).await {
        Ok(Ok(info)) => Ok(Json(info)),
        Ok(Err(_)) => Err(StatusCode::BAD_GATEWAY),
        Err(_) => Err(StatusCode::GATEWAY_TIMEOUT),
    }
}

#[derive(Serialize)]
struct Check {
    ok: bool,
    /// HTTP status when the server refused (e.g. 403: link bound to another client or token).
    status: Option<u16>,
    size: Option<u64>,
}

/// Does this media link really download from here, with these headers? Lets the extension offer
/// only working YouTube formats instead of links that would fail with 403 later.
async fn check(
    State(manager): State<Arc<Manager>>,
    headers: HeaderMap,
    Json(req): Json<AddRequest>,
) -> Result<Json<Check>, StatusCode> {
    guard(&manager, &headers)?;
    if !web(&req.url) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut request_headers = manager::header_map(&req);
    let client = manager.client(&req.url).await;
    let verdict = async {
        let p = engine::probe(&client, &req.url, &request_headers).await?;
        // The server took another identity than the one asked for: so does the last-byte check.
        if let Some(agent) = &p.agent {
            request_headers.insert(engine::header::USER_AGENT, agent.clone());
        }
        // Some servers (YouTube without its anti-bot token) serve the beginning and refuse the rest:
        // the last byte must be reachable too.
        if let Some(size) = p.size.filter(|&s| s > 1 && p.ranges) {
            let last = client
                .get(req.url.clone())
                .headers(request_headers.clone())
                .header(axum::http::header::RANGE.as_str(), format!("bytes={}-{}", size - 1, size - 1))
                .send()
                .await?;
            let status = last.status().as_u16();
            if status != 206 {
                return Ok(Check { ok: false, status: Some(status), size: p.size });
            }
        }
        Ok::<_, engine::EngineError>(Check { ok: p.size != Some(0), status: None, size: p.size })
    };
    let check = match tokio::time::timeout(CHECK_TIMEOUT, verdict).await {
        Ok(Ok(check)) => check,
        Ok(Err(engine::EngineError::Http(e))) => Check { ok: false, status: e.status().map(|s| s.as_u16()), size: None },
        _ => Check { ok: false, status: None, size: None },
    };
    Ok(Json(check))
}

// ── YouTube module (see `ytdlp`): links YouTube refuses to the extension's own clients ─────

async fn youtube_state(State(manager): State<Arc<Manager>>, headers: HeaderMap) -> Result<Json<crate::ytdlp::State>, StatusCode> {
    guard(&manager, &headers)?;
    Ok(Json(crate::ytdlp::state()))
}

/// Starts the module's installation (a few minutes; the extension asks `/youtube/state` later).
async fn youtube_install(State(manager): State<Arc<Manager>>, headers: HeaderMap) -> Result<Json<crate::ytdlp::State>, StatusCode> {
    guard(&manager, &headers)?;
    crate::ytdlp::install(&tokio::runtime::Handle::current());
    Ok(Json(crate::ytdlp::state()))
}

#[derive(Deserialize)]
struct Video {
    id: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Extracted {
    Formats(crate::ytdlp::Formats),
    Refused { error: String },
}

async fn youtube_extract(State(manager): State<Arc<Manager>>, headers: HeaderMap, Json(v): Json<Video>) -> Result<Json<Extracted>, StatusCode> {
    guard(&manager, &headers)?;
    if !crate::ytdlp::valid_id(&v.id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if crate::ytdlp::state() != crate::ytdlp::State::Ready {
        return Err(StatusCode::CONFLICT);
    }
    Ok(Json(match crate::ytdlp::extract(&v.id).await {
        Ok(formats) => Extracted::Formats(formats),
        Err(error) => Extracted::Refused { error },
    }))
}

// ── Browser recordings (YouTube): the page's own player feeds the data ──────────────────

#[derive(Deserialize)]
struct RecordStart {
    page: Url,
    filename: String,
}

#[derive(Serialize)]
struct RecordToken {
    token: String,
}

async fn record_start(
    State(manager): State<Arc<Manager>>,
    headers: HeaderMap,
    Json(req): Json<RecordStart>,
) -> Result<Json<RecordToken>, StatusCode> {
    guard(&manager, &headers)?;
    if !web(&req.page) || req.filename.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Json(RecordToken { token: manager.start_recording(req.page, &req.filename) }))
}

#[derive(Deserialize)]
struct Chunk {
    ms: u32,
    track: Track,
}

async fn record_append(
    State(manager): State<Arc<Manager>>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Query(chunk): Query<Chunk>,
    body: Bytes,
) -> StatusCode {
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    match manager.record_append(&token, chunk.ms, chunk.track, &body).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INSUFFICIENT_STORAGE,
    }
}

#[derive(Deserialize)]
struct Fraction {
    fraction: f64,
}

async fn record_progress(
    State(manager): State<Arc<Manager>>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Json(p): Json<Fraction>,
) -> StatusCode {
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    if !p.fraction.is_finite() {
        return StatusCode::BAD_REQUEST;
    }
    if manager.record_progress(&token, p.fraction.clamp(0.0, 1.0)) { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }
}

async fn record_finish(State(manager): State<Arc<Manager>>, headers: HeaderMap, Path(token): Path<String>) -> StatusCode {
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    if manager.finish_recording(&token) { StatusCode::ACCEPTED } else { StatusCode::NOT_FOUND }
}

async fn record_cancel(State(manager): State<Arc<Manager>>, headers: HeaderMap, Path(token): Path<String>) -> StatusCode {
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    if manager.cancel_recording(&token, crate::tr!("enregistrement annulé", "recording cancelled")) { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }
}

#[derive(Deserialize)]
struct Pair {
    origin: String,
}

/// The native connector pairs its Firefox extension (see `native`). Local programs only, like
/// `/quit`: a browser always sends `Origin` with a POST.
async fn pair(State(manager): State<Arc<Manager>>, headers: HeaderMap, Json(p): Json<Pair>) -> StatusCode {
    if headers.contains_key(ORIGIN) {
        return StatusCode::FORBIDDEN;
    }
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    if manager.pair_firefox(&p.origin) { StatusCode::NO_CONTENT } else { StatusCode::BAD_REQUEST }
}

/// `rdm --quit` (the installer, before replacing files): quit as from the tray's "Quitter". Local
/// programs only: browsers always send `Origin` with a POST, so neither a web page nor an
/// extension can close RDM.
async fn quit(State(manager): State<Arc<Manager>>, headers: HeaderMap) -> StatusCode {
    if headers.contains_key(ORIGIN) {
        return StatusCode::FORBIDDEN;
    }
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    manager.request_quit();
    StatusCode::ACCEPTED
}

async fn show(State(manager): State<Arc<Manager>>, headers: HeaderMap) -> StatusCode {
    // The Firefox extension's toolbar button (not approved yet: before the guard refuses it): an
    // explicit request, so a put-off approval question returns. Not for a web page's request.
    if headers.get(ORIGIN).is_some_and(|o| o.as_bytes().starts_with(b"moz-extension://")) {
        manager.firefox_wake();
    }
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    manager.show();
    StatusCode::NO_CONTENT
}

/// For talking to the running RDM: never through a proxy (a system proxy would get 127.0.0.1).
fn local_client() -> Option<engine::Client> {
    engine::client_with(&engine::ClientOptions { route: engine::Route::Direct, ..engine::ClientOptions::default() }).ok()
}

/// A request to the running RDM, as a local program (see `local`): only when the program on the
/// bridge's port runs as this user, and with the token that proves this program does too.
fn local_post(path: &str) -> Option<reqwest::RequestBuilder> {
    if !local::bridge_is_ours() {
        return None;
    }
    let request = local_client()?.post(format!("http://127.0.0.1:{BRIDGE_PORT}{path}"));
    Some(match local::read_token() {
        Some(token) => request.header(local::TOKEN_HEADER, token),
        None => request, // an older RDM: it asks for none
    })
}

/// `rdm --quit`: asks a running RDM to quit and waits (bounded) until it is gone. Starts nothing.
pub async fn quit_running() {
    let Some(request) = local_post("/quit") else { return };
    let asked = request
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    if !asked {
        return;
    }
    // The port is let go when the process ends (downloads saved first).
    for _ in 0..80 {
        if bind().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Second launch: hands the URL (or just "show yourself") to the running instance; `true` when it
/// took the request.
pub async fn forward(url: Option<&Url>) -> bool {
    let request = match url {
        Some(url) => local_post("/add").map(|r| r.header("content-type", "application/json").body(serde_json::json!({ "url": url }).to_string())),
        None => local_post("/show"),
    };
    let Some(request) = request else { return false };
    request.timeout(Duration::from_secs(5)).send().await.is_ok_and(|r| r.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORBIDDEN: Result<(), StatusCode> = Err(StatusCode::FORBIDDEN);

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs.iter().map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap())).collect()
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn origin_and_host_rules() {
        let guard = |h: &HeaderMap| guard_with(h, TOKEN, Access::Act, |_| panic!("not a Firefox origin"));
        assert_eq!(guard(&headers(&[("host", "127.0.0.1:9614"), ("origin", EXTENSION_ORIGIN)])), Ok(()));
        assert_eq!(guard(&headers(&[("host", "localhost:9614"), (local::TOKEN_HEADER, TOKEN)])), Ok(()));
        assert_eq!(guard(&headers(&[("host", "127.0.0.1:9614"), ("origin", "https://evil.example")])), FORBIDDEN);
        assert_eq!(guard(&headers(&[("host", "127.0.0.1:9614"), ("origin", "chrome-extension://another")])), FORBIDDEN);
        assert_eq!(guard(&headers(&[("host", "evil.example:9614"), ("origin", EXTENSION_ORIGIN)])), FORBIDDEN);
        assert_eq!(guard(&headers(&[("host", "127.0.0.1.evil.example"), (local::TOKEN_HEADER, TOKEN)])), FORBIDDEN);
        assert_eq!(guard(&headers(&[])), FORBIDDEN);
    }

    /// A local program without `Origin` must prove it is the user's (the token from the user's
    /// files): another account of the computer, or a stripped `Origin`, gets nothing done.
    #[test]
    fn local_programs_need_the_token() {
        let h = |pairs: &[(&'static str, &str)]| headers(&[&[("host", "127.0.0.1:9614")], pairs].concat());
        let act = |h: &HeaderMap| guard_with(h, TOKEN, Access::Act, |_| panic!("no origin"));
        assert_eq!(act(&h(&[(local::TOKEN_HEADER, TOKEN)])), Ok(()));
        assert_eq!(act(&h(&[])), FORBIDDEN, "no token");
        assert_eq!(act(&h(&[(local::TOKEN_HEADER, &TOKEN.replace('0', "1"))])), FORBIDDEN, "a wrong token");
        assert_eq!(guard_with(&h(&[]), "", Access::Act, |_| false), FORBIDDEN, "no token issued: nobody");
        // Reading whether RDM is here and what it captures stays open (Firefox's GETs).
        assert_eq!(guard_with(&h(&[]), TOKEN, Access::Read, |_| false), Ok(()));
        // An origin is judged as an origin, token or not.
        let web = h(&[("origin", "https://evil.example"), (local::TOKEN_HEADER, TOKEN)]);
        assert_eq!(guard_with(&web, TOKEN, Access::Read, |_| false), FORBIDDEN);
    }

    #[test]
    fn firefox_origins_need_approval() {
        const FF: &str = "moz-extension://0f8e7a1c-3b2d-4e5f-9a8b-7c6d5e4f3a2b";
        let h = headers(&[("host", "127.0.0.1:9614"), ("origin", FF)]);
        assert_eq!(guard_with(&h, TOKEN, Access::Act, |o| o == FF), Ok(()));
        // Waiting for the user's approval: 401, told apart from a foreign caller.
        assert_eq!(guard_with(&h, TOKEN, Access::Act, |_| false), Err(StatusCode::UNAUTHORIZED));
        // A foreign host is refused before anything is put up for approval.
        let h = headers(&[("host", "evil.example"), ("origin", FF)]);
        assert_eq!(guard_with(&h, TOKEN, Access::Act, |_| panic!("must not ask")), FORBIDDEN);
    }
}
