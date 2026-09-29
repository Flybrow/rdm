//! YouTube module: yt-dlp, with Deno (YouTube's JavaScript challenges) and bgutil (the anti-bot
//! "proof of origin" token, generated locally by YouTube's own BotGuard code) — for the videos
//! whose links YouTube refuses to hand to the extension's clients.
//!
//! Installed on request only, in the user's own data folder (no administrator rights), from the
//! projects' GitHub releases: each file checked against the SHA-256 GitHub publishes for it
//! (bgutil's sources: against the digest pinned below). yt-dlp only reads the link list
//! (`-J`): RDM downloads the streams itself, like any other link.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// bgutil is pinned: its server code runs on this computer.
const BGUTIL_TAG: &str = "2.0.0";
const BGUTIL_PLUGIN: &str = "bgutil-ytdlp-pot-provider.zip";
const BGUTIL_SOURCE_SHA256: &str = "e95324ee24b1b0f1b4ad43d336343afe7cf1914acdf65d9cc1977f51d7b137c2";
const BGUTIL_SOURCE_URL: &str = "https://codeload.github.com/Brainicism/bgutil-ytdlp-pot-provider/zip/refs/tags/2.0.0";

#[cfg(windows)]
const YTDLP_ASSET: &str = "yt-dlp.exe";
#[cfg(not(windows))]
const YTDLP_ASSET: &str = "yt-dlp_linux";
#[cfg(windows)]
const DENO_ASSET: &str = "deno-x86_64-pc-windows-msvc.zip";
#[cfg(not(windows))]
const DENO_ASSET: &str = "deno-x86_64-unknown-linux-gnu.zip";
const EXE: &str = std::env::consts::EXE_SUFFIX;

const MAX_FILE: u64 = 400 << 20;
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(25);
/// Extractions at once, and how long one waits for its turn.
const MAX_EXTRACTIONS: usize = 2;
const WAIT_TURN: Duration = Duration::from_secs(3);
const SETUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Written last: the module is complete.
const READY: &str = "ready";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase", tag = "state", content = "error")]
pub enum State {
    Missing,
    Installing,
    Ready,
    /// The last installation failed (why, in English: shown as is in the extension).
    Failed(String),
}

static INSTALLING: Mutex<Option<State>> = Mutex::new(None);

fn tools_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "rdm", "rdm").map(|d| d.data_local_dir().join("youtube"))
}

struct Paths {
    ytdlp: PathBuf,
    deno: PathBuf,
    plugins: PathBuf,
    server: PathBuf,
}

fn paths(dir: &Path) -> Paths {
    Paths {
        ytdlp: dir.join(format!("yt-dlp{EXE}")),
        deno: dir.join(format!("deno{EXE}")),
        plugins: dir.join("plugins"),
        server: dir.join("bgutil").join("server"),
    }
}

pub fn state() -> State {
    let current = INSTALLING.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    match current {
        Some(state) => state,
        None if tools_dir().is_some_and(|d| d.join(READY).is_file()) => State::Ready,
        None => State::Missing,
    }
}

/// Starts the installation in the background (nothing when one runs or the module is there).
pub fn install(rt: &tokio::runtime::Handle) {
    {
        let mut current = INSTALLING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*current, Some(State::Installing)) {
            return;
        }
        if current.is_none() && tools_dir().is_some_and(|d| d.join(READY).is_file()) {
            return;
        }
        *current = Some(State::Installing);
    }
    rt.spawn(async {
        let result = match tokio::time::timeout(SETUP_TIMEOUT, setup()).await {
            Ok(result) => result,
            Err(_) => Err("installation took too long".to_owned()),
        };
        let mut current = INSTALLING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = match result {
            Ok(()) => None, // `ready` is on disk
            Err(e) => Some(State::Failed(e)),
        };
    });
}

// ── Installation ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Release {
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

async fn setup() -> Result<(), String> {
    let dir = tools_dir().ok_or("no user data folder")?;
    let p = paths(&dir);
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    let _ = tokio::fs::remove_file(dir.join(READY)).await;
    tokio::fs::create_dir_all(&p.plugins).await.map_err(|e| io("cannot create the folder", e))?;
    let client = engine::client().map_err(|e| e.to_string())?;

    let ytdlp = release_asset(&client, "yt-dlp/yt-dlp", "latest", YTDLP_ASSET).await?;
    let path = p.ytdlp.clone();
    tokio::task::spawn_blocking(move || write_exe(&path, |out| std::io::Write::write_all(out, &ytdlp))).await.map_err(|e| e.to_string())??;

    let deno = release_asset(&client, "denoland/deno", "latest", DENO_ASSET).await?;
    let path = p.deno.clone();
    tokio::task::spawn_blocking(move || unzip_exe(&deno, &format!("deno{EXE}"), &path)).await.map_err(|e| e.to_string())??;

    let plugin = release_asset(&client, "Brainicism/bgutil-ytdlp-pot-provider", &format!("tags/{BGUTIL_TAG}"), BGUTIL_PLUGIN).await?;
    tokio::fs::write(p.plugins.join(BGUTIL_PLUGIN), &plugin).await.map_err(|e| io("cannot write", e))?;

    let source = download(&client, BGUTIL_SOURCE_URL, Some(BGUTIL_SOURCE_SHA256)).await?;
    let bgutil = dir.join("bgutil");
    let _ = tokio::fs::remove_dir_all(&bgutil).await;
    let target = bgutil.clone();
    tokio::task::spawn_blocking(move || unzip_stripped(&source, &target)).await.map_err(|e| e.to_string())??;

    // The token generator's own libraries (npm), as bgutil's documentation installs them.
    let mut command = tokio::process::Command::new(&p.deno);
    command.args(["install", "--allow-scripts=npm:canvas", "--frozen"]).current_dir(&p.server);
    let out = run(command, SETUP_TIMEOUT).await?;
    if !out.status.success() {
        return Err(format!("bgutil setup failed: {}", tail(&out.stderr)));
    }
    tokio::fs::write(dir.join(READY), format!("bgutil {BGUTIL_TAG}\n")).await.map_err(|e| io("cannot write", e))
}

/// A release asset of `repo` (`latest` or `tags/<tag>`), checked against GitHub's SHA-256.
async fn release_asset(client: &reqwest::Client, repo: &str, which: &str, name: &str) -> Result<Vec<u8>, String> {
    let res = client
        .get(format!("https://api.github.com/repos/{repo}/releases/{which}"))
        .header("accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("GitHub ({repo}): {e}"))?;
    let body = res.bytes().await.map_err(|e| format!("GitHub ({repo}): {e}"))?;
    let release: Release = serde_json::from_slice(&body).map_err(|e| format!("GitHub ({repo}): {e}"))?;
    let asset = release.assets.into_iter().find(|a| a.name == name).ok_or_else(|| format!("{name} not found in {repo}"))?;
    let sha256 = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).ok_or_else(|| format!("{name}: no checksum published"))?;
    if !asset.browser_download_url.starts_with("https://github.com/") {
        return Err(format!("{name}: unexpected address"));
    }
    download(client, &asset.browser_download_url, Some(sha256)).await
}

async fn download(client: &reqwest::Client, url: &str, sha256: Option<&str>) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};
    let res = client
        .get(url)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("download failed: {e}"))?;
    if res.content_length().is_some_and(|n| n > MAX_FILE) {
        return Err("file abnormally large".into());
    }
    let bytes = res.bytes().await.map_err(|e| format!("download failed: {e}"))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("file abnormally large".into());
    }
    let digest = crate::manager::checksum::hex(&Sha256::digest(&bytes));
    if sha256.is_some_and(|expected| !expected.eq_ignore_ascii_case(&digest)) {
        return Err(format!("corrupted download (SHA-256 mismatch): {url}"));
    }
    Ok(bytes.to_vec())
}

/// Writes the program at `path` through a temporary file (`fill` writes its bytes), then puts it
/// in place: a half-written program is never left under its name.
fn write_exe(path: &Path, fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>) -> Result<(), String> {
    let tmp = crate::settings::with_suffix(path, ".tmp");
    let mut out = std::fs::File::create(&tmp).map_err(|e| format!("cannot write: {e}"))?;
    fill(&mut out).map_err(|e| format!("cannot write: {e}"))?;
    drop(out);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot write (is it running?): {e}"))
}

fn archive(data: &[u8]) -> Result<zip::ZipArchive<std::io::Cursor<&[u8]>>, String> {
    zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| format!("bad archive: {e}"))
}

/// The one file named `name` of a zip archive, written as the program `path`: unpacked straight to
/// disk (Deno's is over 100 MB), never whole in memory.
fn unzip_exe(data: &[u8], name: &str, path: &Path) -> Result<(), String> {
    let mut zip = archive(data)?;
    let mut entry = zip.by_name(name).map_err(|e| format!("{name}: {e}"))?;
    write_exe(path, |out| std::io::copy(&mut entry, out).map(drop))
}

/// A GitHub source archive into `dest`, without its top folder (`<repo>-<tag>/`). Paths that
/// would leave `dest` (`..`, absolute) are refused.
fn unzip_stripped(data: &[u8], dest: &Path) -> Result<(), String> {
    let mut zip = archive(data)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(inner) = entry.enclosed_name() else { return Err("unsafe path in archive".into()) };
        let inner: PathBuf = inner.components().skip(1).collect();
        if inner.as_os_str().is_empty() {
            continue;
        }
        let path = dest.join(inner);
        if entry.is_dir() {
            std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── Extraction ─────────────────────────────────────────────────────────────

/// A direct link, in the shape `extension/youtube.js` gives the menu.
#[derive(Debug, Serialize, PartialEq)]
pub struct Format {
    itag: String,
    url: String,
    container: String,
    video: bool,
    audio: bool,
    height: u64,
    fps: f64,
    label: String,
    size: u64,
    bitrate: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Formats {
    title: String,
    /// The User-Agent the links are bound to.
    ua: String,
    formats: Vec<Format>,
}

/// Whether `id` is a YouTube video ID (11 characters of `[A-Za-z0-9_-]`).
pub fn valid_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The links of video `id` (checked by [`valid_id`]), as yt-dlp resolves them.
pub async fn extract(id: &str) -> Result<Formats, String> {
    if !valid_id(id) {
        return Err("invalid video".into());
    }
    // yt-dlp and Deno take hundreds of megabytes each: a menu opened again and again must not
    // start them by the dozen.
    static RUNNING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_EXTRACTIONS);
    let _turn = match tokio::time::timeout(WAIT_TURN, RUNNING.acquire()).await {
        Ok(Ok(permit)) => permit,
        _ => return Err("the YouTube module is busy: try again in a moment".into()),
    };
    let dir = tools_dir().ok_or("no user data folder")?;
    let p = paths(&dir);
    let mut jsrt = p.deno.into_os_string();
    jsrt = [std::ffi::OsStr::new("deno:"), &jsrt].into_iter().collect();
    let mut server_home = p.server.into_os_string();
    server_home = [std::ffi::OsStr::new("youtubepot-bgutilscript:server_home="), &server_home].into_iter().collect();
    let mut command = tokio::process::Command::new(&p.ytdlp);
    command
        .args(["-J", "--no-playlist", "--no-warnings", "--ignore-config", "--no-js-runtimes", "--js-runtimes"])
        .arg(jsrt)
        .args(["--no-plugin-dirs", "--plugin-dirs"])
        .arg(&p.plugins)
        .arg("--extractor-args")
        .arg(server_home)
        .arg("--")
        .arg(format!("https://www.youtube.com/watch?v={id}"));
    let out = run(command, EXTRACT_TIMEOUT).await?;
    if !out.status.success() {
        return Err(tail(&out.stderr));
    }
    let info: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("unexpected yt-dlp output: {e}"))?;
    Ok(formats(&info))
}

/// The direct (`https`) links of yt-dlp's `-J` output, all bound to one User-Agent; no DRM
/// streams, no dynamic-range-compressed audio duplicates.
fn formats(info: &Value) -> Formats {
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let ua_of = |f: &Value| f.pointer("/http_headers/User-Agent").and_then(Value::as_str).unwrap_or("").to_owned();
    let all = info.get("formats").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let direct: Vec<&Value> = all
        .iter()
        .filter(|f| {
            str_of(f, "protocol") == "https"
                && str_of(f, "url").starts_with("https://")
                && !str_of(f, "format_id").ends_with("-drc")
                && f.get("has_drm").and_then(Value::as_bool) != Some(true)
        })
        .collect();
    let ua = direct.first().map(|f| ua_of(f)).unwrap_or_default();
    let formats = direct
        .into_iter()
        .filter(|f| ua_of(f) == ua)
        .map(|f| {
            let codec = |k: &str| f.get(k).and_then(Value::as_str).is_some_and(|c| c != "none");
            let height = f.get("height").and_then(Value::as_u64).unwrap_or(0);
            let ext = str_of(f, "ext");
            Format {
                itag: str_of(f, "format_id"),
                url: str_of(f, "url"),
                container: if ext == "m4a" { "mp4".into() } else { ext },
                video: codec("vcodec"),
                audio: codec("acodec"),
                height,
                fps: f.get("fps").and_then(Value::as_f64).unwrap_or(0.0),
                label: if height > 0 { format!("{height}p") } else { String::new() },
                size: f.get("filesize").or_else(|| f.get("filesize_approx")).and_then(Value::as_f64).unwrap_or(0.0) as u64,
                bitrate: (f.get("tbr").and_then(Value::as_f64).unwrap_or(0.0) * 1000.0) as u64,
            }
        })
        .collect();
    Formats { title: str_of(info, "title"), ua, formats }
}

// ── Processes ──────────────────────────────────────────────────────────────

async fn run(mut command: tokio::process::Command, limit: Duration) -> Result<std::process::Output, String> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let child = command.spawn().map_err(|e| format!("cannot start the YouTube module: {e}"))?;
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => Err("the YouTube module took too long".into()),
    }
}

/// The last meaningful line of an error output (yt-dlp: `ERROR: …`).
fn tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().find(|l| l.starts_with("ERROR")).or_else(|| text.lines().rev().find(|l| !l.trim().is_empty())).unwrap_or("failed");
    line.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_ids() {
        assert!(valid_id("jNQXAC9IVRw"));
        assert!(valid_id("a-b_c123456"));
        assert!(!valid_id("jNQXAC9IVR"));
        assert!(!valid_id("jNQXAC9IVR&"));
        assert!(!valid_id("../../etc/x"));
    }

    #[test]
    fn keeps_direct_links_only() {
        let ua = "Mozilla/5.0 X";
        let info = serde_json::json!({
            "title": "Me at the zoo",
            "formats": [
                { "format_id": "233", "protocol": "m3u8_native", "url": "https://manifest.googlevideo.com/x", "ext": "mp4" },
                { "format_id": "140-drc", "protocol": "https", "url": "https://r.googlevideo.com/a", "ext": "m4a", "http_headers": { "User-Agent": ua } },
                { "format_id": "140", "protocol": "https", "url": "https://r.googlevideo.com/b", "ext": "m4a", "vcodec": "none",
                  "acodec": "mp4a.40.2", "filesize": 309288, "tbr": 129.5, "http_headers": { "User-Agent": ua } },
                { "format_id": "134", "protocol": "https", "url": "https://r.googlevideo.com/c", "ext": "mp4", "vcodec": "avc1",
                  "acodec": "none", "height": 360, "fps": 30, "filesize_approx": 1000, "http_headers": { "User-Agent": ua } },
                { "format_id": "999", "protocol": "https", "url": "https://r.googlevideo.com/d", "ext": "mp4", "has_drm": true, "http_headers": { "User-Agent": ua } },
                { "format_id": "18", "protocol": "https", "url": "https://r.googlevideo.com/e", "ext": "mp4", "http_headers": { "User-Agent": "other" } },
            ]
        });
        let f = formats(&info);
        assert_eq!(f.title, "Me at the zoo");
        assert_eq!(f.ua, ua);
        assert_eq!(f.formats.iter().map(|f| f.itag.as_str()).collect::<Vec<_>>(), ["140", "134"]);
        let audio = &f.formats[0];
        assert!(audio.audio && !audio.video && audio.container == "mp4" && audio.size == 309288 && audio.bitrate == 129500);
        let video = &f.formats[1];
        assert!(video.video && !video.audio && video.height == 360 && video.label == "360p" && video.size == 1000);
    }

    #[test]
    fn a_program_is_unpacked_into_place() {
        let dir = std::env::temp_dir().join(format!("rdm-unzip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deno");
        let data = crate::extension::zip([("deno", b"\x7fELF program".as_slice()), ("README.md", b"docs".as_slice())].into_iter());
        unzip_exe(&data, "deno", &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\x7fELF program");
        assert!(!crate::settings::with_suffix(&path, ".tmp").exists(), "nothing left beside it");
        assert!(unzip_exe(&data, "missing", &path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_lines() {
        assert_eq!(tail(b"[youtube] x\nERROR: [youtube] x: Sign in to confirm you're not a bot\n"), "ERROR: [youtube] x: Sign in to confirm you're not a bot");
        assert_eq!(tail(b"boom\n\n"), "boom");
        assert_eq!(tail(b""), "failed");
    }
}

/// The real thing: installs the module in the user's folder and reads a video's links (network,
/// minutes): `cargo test -p rdm ytdlp::live -- --ignored --nocapture`.
#[cfg(test)]
mod live {
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "network: downloads the YouTube module"]
    async fn install_and_extract() {
        super::setup().await.expect("installation");
        assert_eq!(super::state(), super::State::Ready);
        let f = super::extract("jNQXAC9IVRw").await.expect("extraction");
        println!("{} — {} links, UA {}", f.title, f.formats.len(), f.ua);
        assert!(f.formats.iter().any(|x| x.audio && !x.video) && f.formats.iter().any(|x| x.video));
    }
}
