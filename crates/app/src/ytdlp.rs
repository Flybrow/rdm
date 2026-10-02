//! YouTube module: yt-dlp, with Deno (YouTube's JavaScript challenges) and bgutil (the anti-bot
//! "proof of origin" token, generated locally by YouTube's own BotGuard code) — for the videos
//! whose links YouTube refuses to hand to the extension's clients.
//!
//! Installed on request only, in the user's own data folder (no administrator rights), from the
//! projects' GitHub releases, and nothing runs that was not checked first:
//! - yt-dlp, its latest release (YouTube changes often): its checksum list must carry the
//!   signature of yt-dlp's own key (written below), and the program must match it — a GitHub
//!   account taken over cannot slip in another program;
//! - Deno, bgutil and the drawing library bgutil needs (`canvas`, a native library): exact
//!   versions, each archive checked against the SHA-256 written below. No package's own install
//!   script runs (`canvas`'s used to download its library unchecked).
//!
//! yt-dlp only reads the link list (`-J`): RDM downloads the streams itself, like any other link.

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
const BGUTIL_PLUGIN_SHA256: &str = "bce874dfa25896c2798e0f4f8147b7b22e785479eb1e459ab232bf2506c95016";
const BGUTIL_SOURCE_SHA256: &str = "e95324ee24b1b0f1b4ad43d336343afe7cf1914acdf65d9cc1977f51d7b137c2";
const BGUTIL_SOURCE_URL: &str = "https://codeload.github.com/Brainicism/bgutil-ytdlp-pot-provider/zip/refs/tags/2.0.0";

#[cfg(windows)]
const YTDLP_ASSET: &str = "yt-dlp.exe";
#[cfg(not(windows))]
const YTDLP_ASSET: &str = "yt-dlp_linux";
/// yt-dlp's checksum list, and its detached signature.
const YTDLP_SUMS: &str = "SHA2-256SUMS";

/// yt-dlp's release signing key (RSA 4096, fingerprint AC0C BBE6 848D 6A87 3464 AF4E 57CF 6593
/// 3B5A 7581): its repository's `public.key`, unchanged since 2023, the same on keys.openpgp.org.
pub(crate) const SIGNING_KEY: crate::openpgp::RsaKey = crate::openpgp::RsaKey { n: &YTDLP_MODULUS, e: &[1, 0, 1] };
const YTDLP_MODULUS: [u8; 512] = crate::openpgp::hex(concat!(
    "f4ac5f738c63c0b74b6196de42d5e6f371c0155fb35bd6ff9ea908e4f961197531135428375803399e002b947766fdf66acd480efa9018ac1eb418a7a0edf7d4",
    "0dff92d1714a11975e44b61e01aaaa2a14a26d56a50e3bce04ecedbb75bad3b1cb113400a80ef04cfb88b765aa9a92fad0d21ccabdbb2f1aa681d37798dbd9da",
    "22cef9b3f49ca47f0be3e013d1f8cadcb00dad76cccc7a3e66429d538e3aee97644166754c71750f5f143a9df4b701298d8b0ce712628d6c698d185a26561edc",
    "d5a19bb590ece39d2e458eaff204724b2260c7ee93711e6eecfcf9dababa66405b4316bba4b7eda58439e9294986aff23fdc81ea1ff5fef673e84b8ba2a90e47",
    "a5b941f012bee167e96a2120ef92e7bee38f7d920f298c976bfd00ed0a2333080eac3f3ca334fe54290a92427e8a35d9ee01cd8e670169474a2bb7c320439823",
    "adcbc9ce905a41904e28057df4945d61c308ef5a5cd84fdde8ea15680632d094b2b6e4709916b9663bab9011cbfd391db48cdcae0353d7aab67c66895d5643c9",
    "d7b88f49809411be1e4f4e49f2e4bb71ab3726aaf58ef538c705baecd7804f10ef5f717c4a2ff08505bcd70d2bef02002b76aaa53289dc43fca271395f2faab3",
    "27c10a162a96d7840c4dd027e92bae653a1d5c24e2227c284fb5a38ee14f28a76f7c7d463f86e7a9dcd39f32f10920c812d7f27cfb085fef4623b26493fcd98d",
));

/// Deno is pinned: it runs bgutil's code, and needs no update to follow YouTube.
const DENO_VERSION: &str = "2.9.7";
#[cfg(windows)]
const DENO_ASSET: (&str, &str) = ("deno-x86_64-pc-windows-msvc.zip", "a0c3101b4158d1dfb7d6a78a7bf0f3de80c96bb423c152beec8beb22786f2238");
#[cfg(not(windows))]
const DENO_ASSET: (&str, &str) = ("deno-x86_64-unknown-linux-gnu.zip", "c6527f24f4b16031d3ae4fa9f658d5f11534c8d84ce7dc8502420280919c3490");

/// The native drawing library bgutil uses (through its `canvas` package), as its own release
/// publishes it; unpacked over the package in place of its install script.
const CANVAS_VERSION: &str = "3.2.3";
#[cfg(windows)]
const CANVAS_ASSET: (&str, &str) = ("canvas-v3.2.3-napi-v7-win32-x64.tar.gz", "ba953cc8c38303ab94cc83461c7561506a5a3af37d6c678de4a47914d2d0bb48");
#[cfg(not(windows))]
const CANVAS_ASSET: (&str, &str) = ("canvas-v3.2.3-napi-v7-linux-x64.tar.gz", "886d1cc270d4caad1d1698ee97532e79c60e992e16fe5f37f2d3bb058f4292b0");
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
        let setup = async { setup(&tools_dir().ok_or("no user data folder")?).await };
        let result = match tokio::time::timeout(SETUP_TIMEOUT, setup).await {
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
}

/// Installs the module into `dir` (the user's data folder; a scratch folder in tests).
async fn setup(dir: &Path) -> Result<(), String> {
    let p = paths(dir);
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    let _ = tokio::fs::remove_file(dir.join(READY)).await;
    tokio::fs::create_dir_all(&p.plugins).await.map_err(|e| io("cannot create the folder", e))?;
    let client = engine::client().map_err(|e| e.to_string())?;

    let ytdlp = signed_ytdlp(&client).await?;
    let path = p.ytdlp.clone();
    tokio::task::spawn_blocking(move || write_exe(&path, |out| std::io::Write::write_all(out, &ytdlp))).await.map_err(|e| e.to_string())??;

    let (name, sha256) = DENO_ASSET;
    let deno = download(&client, &format!("https://github.com/denoland/deno/releases/download/v{DENO_VERSION}/{name}"), Some(sha256)).await?;
    let path = p.deno.clone();
    tokio::task::spawn_blocking(move || unzip_exe(&deno, &format!("deno{EXE}"), &path)).await.map_err(|e| e.to_string())??;

    let plugin_url = format!("https://github.com/Brainicism/bgutil-ytdlp-pot-provider/releases/download/{BGUTIL_TAG}/{BGUTIL_PLUGIN}");
    let plugin = download(&client, &plugin_url, Some(BGUTIL_PLUGIN_SHA256)).await?;
    tokio::fs::write(p.plugins.join(BGUTIL_PLUGIN), &plugin).await.map_err(|e| io("cannot write", e))?;

    let source = download(&client, BGUTIL_SOURCE_URL, Some(BGUTIL_SOURCE_SHA256)).await?;
    let bgutil = dir.join("bgutil");
    let _ = tokio::fs::remove_dir_all(&bgutil).await;
    let target = bgutil.clone();
    tokio::task::spawn_blocking(move || unzip_stripped(&source, &target)).await.map_err(|e| e.to_string())??;

    // The token generator's own libraries (npm), each checked against bgutil's lock file
    // (`--frozen`); no install script runs.
    let mut command = tokio::process::Command::new(&p.deno);
    command.args(["install", "--frozen"]).current_dir(&p.server);
    let out = run(command, SETUP_TIMEOUT).await?;
    if !out.status.success() {
        return Err(format!("bgutil setup failed: {}", tail(&out.stderr)));
    }
    // What `canvas`'s install script would have downloaded: its library, from its own release.
    let (name, sha256) = CANVAS_ASSET;
    let canvas_url = format!("https://github.com/Automattic/node-canvas/releases/download/v{CANVAS_VERSION}/{name}");
    let library = download(&client, &canvas_url, Some(sha256)).await?;
    unpack_tar_gz(dir, &library, &p.server.join("node_modules").join("canvas")).await?;
    let ready = format!("bgutil {BGUTIL_TAG}\ndeno {DENO_VERSION}\ncanvas {CANVAS_VERSION}\n");
    tokio::fs::write(dir.join(READY), ready).await.map_err(|e| io("cannot write", e))
}

/// yt-dlp's latest program, once its release's checksum list carries yt-dlp's signature
/// ([`SIGNING_KEY`]) and the program matches its line there.
async fn signed_ytdlp(client: &reqwest::Client) -> Result<Vec<u8>, String> {
    let release = latest_release(client, "yt-dlp/yt-dlp").await?;
    let url_of = |name: &str| {
        let asset = release.assets.iter().find(|a| a.name == name).ok_or_else(|| format!("{name} not found in yt-dlp's release"))?;
        if asset.browser_download_url.starts_with("https://github.com/") {
            Ok(asset.browser_download_url.clone())
        } else {
            Err(format!("{name}: unexpected address"))
        }
    };
    let sums = download(client, &url_of(YTDLP_SUMS)?, None).await?;
    let signature = download(client, &url_of(&format!("{YTDLP_SUMS}.sig"))?, None).await?;
    if !crate::openpgp::verify(&SIGNING_KEY, &sums, &signature) {
        return Err("yt-dlp: the checksum list is not signed by yt-dlp's key: refused".into());
    }
    let sha256 = sum_of(&String::from_utf8_lossy(&sums), YTDLP_ASSET).ok_or_else(|| format!("{YTDLP_ASSET}: not in yt-dlp's signed checksums"))?;
    download(client, &url_of(YTDLP_ASSET)?, Some(&sha256)).await
}

/// The SHA-256 a `sha256sum` list (`<hex>  <name>` lines) gives for `name`.
fn sum_of(list: &str, name: &str) -> Option<String> {
    list.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        (file.trim_start().trim_start_matches('*') == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// The latest release of `repo`, as GitHub's API lists it.
async fn latest_release(client: &reqwest::Client, repo: &str) -> Result<Release, String> {
    let res = client
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("GitHub ({repo}): {e}"))?;
    let body = res.bytes().await.map_err(|e| format!("GitHub ({repo}): {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("GitHub ({repo}): {e}"))
}

/// Unpacks a (checked) `.tar.gz` archive into `dest` with the system's `tar` (Windows 10 and
/// later ship one), through a file in `work`.
async fn unpack_tar_gz(work: &Path, archive: &[u8], dest: &Path) -> Result<(), String> {
    let file = work.join("unpack.tar.gz");
    tokio::fs::write(&file, archive).await.map_err(|e| format!("cannot write: {e}"))?;
    #[cfg(windows)]
    let tar = std::env::var_os("SystemRoot").map_or_else(|| "tar.exe".into(), |root| Path::new(&root).join(r"System32\tar.exe"));
    #[cfg(not(windows))]
    let tar = std::path::PathBuf::from("tar");
    let mut command = tokio::process::Command::new(tar);
    command.arg("-xzf").arg(&file).arg("-C").arg(dest);
    let out = run(command, Duration::from_secs(120)).await;
    let _ = tokio::fs::remove_file(&file).await;
    match out {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!("cannot unpack {}: {}", dest.display(), tail(&out.stderr))),
        Err(e) => Err(e),
    }
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
    extract_in(&tools_dir().ok_or("no user data folder")?, id).await
}

/// [`extract`] with the module installed in `dir`.
async fn extract_in(dir: &Path, id: &str) -> Result<Formats, String> {
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
    let p = paths(dir);
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

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

async fn run(mut command: tokio::process::Command, limit: Duration) -> Result<std::process::Output, String> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    // Its own process group: the programs it starts (Deno, bgutil) end with it (see `end_tree`).
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().map_err(|e| format!("cannot start the YouTube module: {e}"))?;
    let pid = child.id();
    let output = child.wait_with_output();
    tokio::pin!(output);
    tokio::select! {
        result = &mut output => result.map_err(|e| e.to_string()),
        () = tokio::time::sleep(limit) => {
            // Ended while the child still is (then by `kill_on_drop`): ending only yt-dlp would
            // leave its Deno running, hundreds of megabytes each.
            if let Some(pid) = pid {
                end_tree(pid).await;
            }
            Err("the YouTube module took too long".into())
        }
    }
}

/// Ends process `pid` and every process it started.
async fn end_tree(pid: u32) {
    #[cfg(windows)]
    {
        let taskkill = std::env::var_os("SystemRoot").map_or_else(|| "taskkill.exe".into(), |root| Path::new(&root).join(r"System32\taskkill.exe"));
        let pid = pid.to_string();
        let mut kill = tokio::process::Command::new(taskkill);
        kill.args(["/T", "/F", "/PID", &pid]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
        let _ = kill.status().await;
    }
    #[cfg(unix)]
    if let Ok(group) = i32::try_from(pid) {
        // SAFETY: a plain signal to the process group the child leads (`process_group(0)`).
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
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

    /// A module that takes too long is ended with what it started (yt-dlp's Deno): nothing is left
    /// running behind it.
    #[tokio::test]
    async fn a_timeout_ends_the_whole_process_tree() {
        let file = std::env::temp_dir().join(format!("rdm-tree-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&file);
        #[cfg(windows)]
        let command = {
            let mut c = tokio::process::Command::new("powershell");
            let script = format!(
                "$p = Start-Process -FilePath ping -ArgumentList '-n','120','127.0.0.1' -PassThru -WindowStyle Hidden; \
                 Set-Content -Path '{}' -Value $p.Id; Start-Sleep 120",
                file.display()
            );
            c.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
            c
        };
        #[cfg(unix)]
        let command = {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", &format!("sleep 120 & echo $! > '{}'; wait", file.display())]);
            c
        };
        assert!(run(command, Duration::from_secs(6)).await.is_err(), "timed out");
        let pid: u32 = std::fs::read_to_string(&file).expect("the child started").trim().parse().unwrap();
        let _ = std::fs::remove_file(&file);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            assert!(std::time::Instant::now() < deadline, "the grandchild {pid} still runs");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[cfg(windows)]
    fn alive(pid: u32) -> bool {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        };
        // SAFETY: plain queries on a handle closed right after.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let mut code = 0u32;
            let running = GetExitCodeProcess(process, &mut code) != 0 && code == STILL_ACTIVE as u32;
            CloseHandle(process);
            running
        }
    }

    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        // A process killed but not reaped yet (a zombie) is gone for this purpose.
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        !state.is_empty() && !state.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z')
    }

    /// The program's line in yt-dlp's signed list (the real one of 2026.08.19).
    #[test]
    fn reads_the_signed_checksum_list() {
        let list = include_str!("../testdata/yt-dlp-2026.08.19-SHA2-256SUMS");
        assert_eq!(sum_of(list, "yt-dlp.exe").as_deref(), Some("66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a"));
        assert_eq!(sum_of(list, "yt-dlp_linux").as_deref(), Some("58162f9bfdc27458ea47bfcb311cf47028f17d8154a8bf7d689861d46399230a"));
        assert_eq!(sum_of(list, "yt-dlp"), Some(sum_of(list, "yt-dlp").unwrap()), "exact names only");
        assert!(sum_of(list, "yt-dlp_linux.zip").is_some_and(|s| Some(s) != sum_of(list, "yt-dlp_linux")));
        assert_eq!(sum_of(list, "missing.exe"), None);
        assert_eq!(sum_of("nothex  yt-dlp.exe\n", "yt-dlp.exe"), None);
    }

    #[test]
    fn error_lines() {
        assert_eq!(tail(b"[youtube] x\nERROR: [youtube] x: Sign in to confirm you're not a bot\n"), "ERROR: [youtube] x: Sign in to confirm you're not a bot");
        assert_eq!(tail(b"boom\n\n"), "boom");
        assert_eq!(tail(b""), "failed");
    }
}

/// The real thing: installs the module (every check included) in a scratch folder and reads a
/// video's links (network, minutes): `cargo test -p rdm ytdlp::live -- --ignored --nocapture`.
#[cfg(test)]
mod live {
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "network: downloads the YouTube module"]
    async fn install_and_extract() {
        let dir = std::env::temp_dir().join(format!("rdm-youtube-{}", std::process::id()));
        super::setup(&dir).await.expect("installation");
        assert!(dir.join(super::READY).is_file());
        // bgutil's native drawing library loads (unpacked by RDM, not by its install script).
        let p = super::paths(&dir);
        let mut deno = tokio::process::Command::new(&p.deno);
        let script = "import { createCanvas } from 'canvas'; createCanvas(4, 4).getContext('2d'); console.log('canvas ok')";
        deno.args(["eval", script]).current_dir(&p.server);
        let out = super::run(deno, std::time::Duration::from_secs(120)).await.expect("deno runs");
        assert!(String::from_utf8_lossy(&out.stdout).contains("canvas ok"), "{}", String::from_utf8_lossy(&out.stderr));
        let f = super::extract_in(&dir, "jNQXAC9IVRw").await.expect("extraction");
        println!("{} — {} links, UA {}", f.title, f.formats.len(), f.ua);
        assert!(f.formats.iter().any(|x| x.audio && !x.video) && f.formats.iter().any(|x| x.video));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
