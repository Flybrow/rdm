//! Loopback HTTP bridge the browser extension talks to.
//!
//! Threat model: any web page — and any other browser extension — can send requests to 127.0.0.1.
//! Defences:
//! - `Origin`: browsers always set it on cross-origin requests; only *our* extension ID passes
//!   (pinned by the manifest `key`), plus local non-browser tools (no `Origin`, already trusted);
//!   Firefox extensions have a random per-install origin: accepted once the user approved it
//!   (Firefox sends `Origin` on POST only: the extension pairs with `POST /ping` when it starts);
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
        .route("/ping", get(ping).post(ping))
        .route("/config", get(config))
        .route("/add", post(add))
        .route("/probe", post(probe))
        .route("/check", post(check))
        .route("/show", post(show))
        .route("/quit", post(quit))
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
    guard_with(headers, |origin| manager.firefox_allowed(origin))?;
    // Which browser the extension runs in (the extension window shows the connected ones).
    if headers.contains_key(ORIGIN)
        && let Some(browser) = headers.get(BROWSER).and_then(|v| v.to_str().ok())
    {
        manager.browser_seen(browser);
    }
    Ok(())
}

/// Sent by the extension: `firefox`, `waterfox`, `chrome`, `brave`, `opera`, `edge` or `chromium`.
const BROWSER: &str = "x-rdm-browser";

/// `firefox`: whether a (`moz-extension://…`) origin was approved by the user. Such an origin not
/// approved (yet) gets 401 — the extension then says "approve me in RDM", not "RDM is not
/// running" — anything else untrusted gets 403.
fn guard_with(headers: &HeaderMap, firefox: impl FnOnce(&str) -> bool) -> Result<(), StatusCode> {
    let host_ok = headers.get(HOST).and_then(|h| h.to_str().ok()).is_some_and(|h| {
        let host = h.rsplit_once(':').map_or(h, |(host, _)| host);
        matches!(host, "127.0.0.1" | "localhost")
    });
    if !host_ok {
        return Err(StatusCode::FORBIDDEN);
    }
    match headers.get(ORIGIN).map(|o| o.to_str()) {
        None => Ok(()),
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

#[derive(Serialize)]
struct Config {
    /// Extensions the browser should hand over (space-separated), from the user's settings.
    captured: String,
}

async fn config(State(manager): State<Arc<Manager>>, headers: HeaderMap) -> Result<Json<Config>, StatusCode> {
    guard(&manager, &headers)?;
    Ok(Json(Config { captured: manager.settings().captured }))
}

async fn add(State(manager): State<Arc<Manager>>, headers: HeaderMap, Json(req): Json<AddRequest>) -> StatusCode {
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    if !web(&req.url) || req.audio_url.as_ref().is_some_and(|u| !web(u)) {
        return StatusCode::BAD_REQUEST;
    }
    manager.add(req);
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
    let info = engine::hls_info(manager.client(), &req.url, &request_headers);
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
    let request_headers = manager::header_map(&req);
    let verdict = async {
        let p = engine::probe(manager.client(), &req.url, &request_headers).await?;
        // Some servers (YouTube without its anti-bot token) serve the beginning and refuse the rest:
        // the last byte must be reachable too.
        if let Some(size) = p.size.filter(|&s| s > 1 && p.ranges) {
            let last = manager
                .client()
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
    if manager.cancel_recording(&token, "enregistrement annulé") { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }
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
    // The extension's toolbar button: an explicit request, so a put-off approval question returns.
    manager.firefox_wake();
    if let Err(status) = guard(&manager, &headers) {
        return status;
    }
    manager.show();
    StatusCode::NO_CONTENT
}

/// Second launch: hand the URL (or just "show yourself") to the running instance.
/// `rdm --quit`: asks a running RDM to quit and waits (bounded) until it is gone. Starts nothing.
pub async fn quit_running() {
    let Ok(client) = engine::client() else { return };
    let asked = client
        .post(format!("http://127.0.0.1:{BRIDGE_PORT}/quit"))
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

/// `true` when a running RDM took the request.
pub async fn forward(url: Option<&Url>) -> bool {
    let Ok(client) = engine::client() else { return false };
    let base = format!("http://127.0.0.1:{BRIDGE_PORT}");
    let request = match url {
        Some(url) => client
            .post(format!("{base}/add"))
            .header("content-type", "application/json")
            .body(serde_json::json!({ "url": url }).to_string()),
        None => client.post(format!("{base}/show")),
    };
    request.timeout(Duration::from_secs(5)).send().await.is_ok_and(|r| r.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORBIDDEN: Result<(), StatusCode> = Err(StatusCode::FORBIDDEN);

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs.iter().map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap())).collect()
    }

    #[test]
    fn origin_and_host_rules() {
        let guard = |h: &HeaderMap| guard_with(h, |_| panic!("not a Firefox origin"));
        assert_eq!(guard(&headers(&[("host", "127.0.0.1:9614"), ("origin", EXTENSION_ORIGIN)])), Ok(()));
        assert_eq!(guard(&headers(&[("host", "localhost:9614")])), Ok(()));
        assert_eq!(guard(&headers(&[("host", "127.0.0.1:9614"), ("origin", "https://evil.example")])), FORBIDDEN);
        assert_eq!(guard(&headers(&[("host", "127.0.0.1:9614"), ("origin", "chrome-extension://another")])), FORBIDDEN);
        assert_eq!(guard(&headers(&[("host", "evil.example:9614"), ("origin", EXTENSION_ORIGIN)])), FORBIDDEN);
        assert_eq!(guard(&headers(&[("host", "127.0.0.1.evil.example")])), FORBIDDEN);
        assert_eq!(guard(&headers(&[])), FORBIDDEN);
    }

    #[test]
    fn firefox_origins_need_approval() {
        const FF: &str = "moz-extension://0f8e7a1c-3b2d-4e5f-9a8b-7c6d5e4f3a2b";
        let h = headers(&[("host", "127.0.0.1:9614"), ("origin", FF)]);
        assert_eq!(guard_with(&h, |o| o == FF), Ok(()));
        // Waiting for the user's approval: 401, told apart from a foreign caller.
        assert_eq!(guard_with(&h, |_| false), Err(StatusCode::UNAUTHORIZED));
        // A foreign host is refused before anything is put up for approval.
        let h = headers(&[("host", "evil.example"), ("origin", FF)]);
        assert_eq!(guard_with(&h, |_| panic!("must not ask")), FORBIDDEN);
    }
}
