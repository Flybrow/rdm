//! Updates from the project's GitHub releases: one request to the GitHub API (at start, then daily,
//! or on demand), nothing else. Installing is silent and done from RDM itself, whatever the
//! system, and RDM is never left closed with nothing happening:
//!
//! 1. the package matching how this RDM was installed is downloaded from GitHub only (with
//!    retries), then checked: size and SHA-256 published by GitHub, the file's own format, and an
//!    **Ed25519 signature** made by the release workflow with a key only it holds — a tampered
//!    or substituted package (even through a compromised GitHub account) is refused;
//! 2. it is installed:
//!    - **Windows** (`.msi`, per user: no administrator prompt): a copy of `rdm.exe` is started as
//!      the installation assistant (the installed file must stay free to be replaced) and RDM
//!      quits; the assistant waits until RDM is really gone, runs `msiexec` without any window,
//!      then makes sure RDM runs again — the new version, or the old one after saying why the
//!      installation failed;
//!    - **Linux, RDM in the user's folders** (`install.sh`, the tarball): the binary is replaced in
//!      place, and RDM restarts;
//!    - **Linux, `.deb` / `.rpm`**: the package manager installs it after the system's password
//!      prompt (polkit), and RDM restarts.
//!
//! No PowerShell: RDM 0.1 and 0.2.0 started it without a console, which PowerShell 5.1 silently
//! refuses to run in. They look for an installer named `…x64.msi`; installers are named
//! `…-x64-setup.msi` since 0.2.1, which they do not recognise: they offer the release page
//! instead. RDM 0.2.1 recognises it and installs it with its own assistant (no signature check:
//! it predates them); signatures are required from 0.3.0 on.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use serde::Deserialize;

use crate::tr;

/// `owner/repo`, from `repository` in Cargo.toml: the single source for where releases live.
pub fn repo() -> Option<&'static str> {
    let url = env!("CARGO_PKG_REPOSITORY");
    let path = url.strip_prefix("https://github.com/")?.trim_end_matches('/');
    (path.split('/').count() == 2 && !path.contains("OWNER")).then_some(path)
}

/// Public half of the key the release workflow signs packages with (Ed25519, raw 32 bytes).
const PUBLIC_KEY: [u8; 32] = hex32("a972e3ab906c230c0e1c96b750e0c7d2abeae96c5979f9c48085853506b9b135");

const fn hex32(s: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("bad hex"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() == 64);
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(b[2 * i]) << 4 | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

/// What the UI shows about updates.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(f32),
    /// The package is ready: RDM is closing to let it install.
    Installing,
    /// Downloading or installing failed: the update stays on offer.
    InstallFailed(Release, String),
    Failed(String),
}

impl State {
    /// The release on offer, if any.
    pub fn release(&self) -> Option<&Release> {
        match self {
            Self::Available(r) | Self::InstallFailed(r, _) => Some(r),
            _ => None,
        }
    }

    /// Something is in progress (a spinner or a percentage is shown).
    pub fn busy(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading(_) | Self::Installing)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub page: String,
    pub notes: String,
    /// The package that updates this RDM, if the release has it.
    pub package: Option<Package>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub url: String,
    /// Size announced by GitHub (0 = unknown).
    pub size: u64,
    /// SHA-256 computed by GitHub when the asset was uploaded (lower-case hex), if published.
    pub sha256: Option<String>,
    /// The detached Ed25519 signature (`<name>.sig`), if the release has one.
    pub signature: Option<String>,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    /// `sha256:<hex>`.
    #[serde(default)]
    digest: Option<String>,
}

/// `v1.2.3` / `1.2.3` → (1, 2, 3).
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let v = version.trim().trim_start_matches(['v', 'V']);
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>());
    Some((parts.next()?.ok()?, parts.next().unwrap_or(Ok(0)).ok()?, parts.next().unwrap_or(Ok(0)).ok()?))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse(candidate), parse(current)), (Some(a), Some(b)) if a > b)
}

// ── How this RDM was installed ─────────────────────────────────────────────

/// How this copy of RDM was installed, hence which package updates it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "the Linux methods are detected on Linux only"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Windows installer (per user, `%LOCALAPPDATA%\Programs\RDM`).
    Msi,
    /// Linux binary in a folder the user can write (`~/.local/bin`, the tarball).
    Binary,
    /// Linux, installed by a package manager.
    Deb,
    Rpm,
    /// A copy RDM cannot update itself (a build folder, a USB stick…): the page is opened.
    Manual,
}

static EXE: OnceLock<PathBuf> = OnceLock::new();

/// Remembers the executable RDM started from: after an in-place update the running file is gone
/// (Linux then reports `…/rdm (deleted)`), and the restart must use the path, not the process.
pub fn remember_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = EXE.set(std::fs::canonicalize(&exe).unwrap_or(exe));
    }
}

fn startup_exe() -> Option<&'static Path> {
    EXE.get().map(PathBuf::as_path)
}

pub fn method() -> Method {
    static METHOD: OnceLock<Method> = OnceLock::new();
    *METHOD.get_or_init(detect)
}

/// RDM installs this release itself (silently); otherwise its page is opened.
pub fn installs_itself(release: &Release) -> bool {
    release.package.is_some() && method() != Method::Manual
}

#[cfg(windows)]
fn detect() -> Method {
    let (Some(installed), Some(me)) = (installed_exe(), startup_exe()) else { return Method::Manual };
    let norm = |p: &Path| std::fs::canonicalize(p).map(|p| p.to_string_lossy().to_lowercase());
    if matches!((norm(&installed), norm(me)), (Ok(a), Ok(b)) if a == b) { Method::Msi } else { Method::Manual }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn detect() -> Method {
    let Some(exe) = startup_exe() else { return Method::Manual };
    let succeeds = |program: &str, args: &[&std::ffi::OsStr]| {
        std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if exe.starts_with("/usr/") || exe.starts_with("/opt/") {
        let has_pkexec = which("pkexec").is_some();
        if has_pkexec && succeeds("dpkg-query", &["-S".as_ref(), exe.as_os_str()]) {
            return Method::Deb;
        }
        if has_pkexec && succeeds("rpm", &["-qf".as_ref(), exe.as_os_str()]) {
            return Method::Rpm;
        }
        return Method::Manual;
    }
    if writable(exe) { Method::Binary } else { Method::Manual }
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
fn detect() -> Method {
    Method::Manual
}

#[cfg(target_os = "linux")]
fn writable(exe: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let access = |p: &Path| {
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
        // SAFETY: a valid NUL-terminated path; `access` only reads it.
        unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
    };
    access(exe) && exe.parent().is_some_and(access)
}

#[cfg(target_os = "linux")]
fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([PathBuf::from("/usr/bin"), PathBuf::from("/usr/sbin"), PathBuf::from("/bin")])
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// Whether a release asset is the package for `method`.
fn wanted(method: Method, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    match method {
        Method::Msi => name.ends_with(".msi") && name.contains("x64"),
        Method::Binary => name == "rdm-linux-x64.tar.gz",
        Method::Deb => name.ends_with("_amd64.deb"),
        Method::Rpm => name.ends_with(".x86_64.rpm"),
        Method::Manual => false,
    }
}

/// The package for `method`, once both it and its signature are online (the release workflow
/// uploads them one after the other: in between, the release is looked at again later).
fn package(assets: &[Asset], method: Method) -> Option<Package> {
    let asset = assets.iter().find(|a| wanted(method, &a.name))?;
    let sha256 = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase);
    let sig_name = format!("{}.sig", asset.name);
    let signature = assets.iter().find(|a| a.name == sig_name).map(|a| a.browser_download_url.clone())?;
    Some(Package { name: asset.name.clone(), url: asset.browser_download_url.clone(), size: asset.size, sha256, signature: Some(signature) })
}

// ── Checking ───────────────────────────────────────────────────────────────

async fn latest(client: &reqwest::Client) -> Result<Option<ApiRelease>, String> {
    let repo = repo().ok_or_else(|| tr!("dépôt GitHub non configuré", "GitHub repository not configured").to_owned())?;
    let unreachable = || tr!("GitHub est injoignable", "GitHub cannot be reached").to_owned();
    let res = client
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| unreachable())?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None); // no release published yet
    }
    if !res.status().is_success() {
        let status = res.status().as_u16();
        return Err(crate::trf!("GitHub a répondu {status}", "GitHub answered {status}"));
    }
    let body = res.bytes().await.map_err(|_| unreachable())?;
    let r: ApiRelease = serde_json::from_slice(&body).map_err(|_| tr!("réponse inattendue de GitHub", "unexpected answer from GitHub").to_owned())?;
    Ok((!r.draft && !r.prerelease).then_some(r))
}

/// The latest published release if it is newer than this build.
pub async fn check(client: &reqwest::Client) -> Result<Option<Release>, String> {
    let Some(r) = latest(client).await? else { return Ok(None) };
    if !is_newer(&r.tag_name, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    Ok(Some(Release {
        version: r.tag_name.trim_start_matches(['v', 'V']).to_owned(),
        page: release_page(r.html_url),
        notes: r.body.unwrap_or_default(),
        package: package(&r.assets, method()),
    }))
}

/// The release's page, opened by the system as is: a GitHub web page, nothing else (the system
/// would run a program's path just as well).
fn release_page(html_url: String) -> String {
    if html_url.starts_with("https://github.com/") && !html_url.contains(char::is_whitespace) {
        html_url
    } else {
        format!("https://github.com/{}/releases/latest", repo().unwrap_or_default())
    }
}

/// Where release assets may be downloaded from (HTTPS, GitHub only).
const TRUSTED: [&str; 3] = ["https://github.com/", "https://objects.githubusercontent.com/", "https://release-assets.githubusercontent.com/"];
const FIREFOX_XPI: &str = "rdm-firefox.xpi";
const MAX_XPI: u64 = 20 << 20;
const MAX_PACKAGE: u64 = 200 << 20;
/// A download that receives nothing for this long is retried.
const STALL: Duration = Duration::from_secs(45);
const DOWNLOAD_ATTEMPTS: u32 = 4;

fn trusted(url: &str) -> bool {
    TRUSTED.iter().any(|p| url.starts_with(p))
}

/// The signed Firefox package of the latest release, written to `dest`: release Firefox installs
/// it for good (an unsigned package only temporarily). `Ok(false)` when there is none — no
/// release, a release older than this RDM, or a package Mozilla has not signed.
pub async fn signed_firefox_xpi(client: &reqwest::Client, dest: &Path) -> Result<bool, String> {
    let Some(r) = latest(client).await? else { return Ok(false) };
    if is_newer(env!("CARGO_PKG_VERSION"), &r.tag_name) {
        return Ok(false);
    }
    let Some(asset) = r.assets.iter().find(|a| a.name == FIREFOX_XPI) else { return Ok(false) };
    if !trusted(&asset.browser_download_url) {
        return Ok(false);
    }
    let failed = || tr!("téléchargement de l'extension impossible", "cannot download the extension").to_owned();
    let res = client
        .get(&asset.browser_download_url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| failed())?;
    if res.content_length().is_some_and(|n| n > MAX_XPI) {
        return Ok(false);
    }
    let bytes = res.bytes().await.map_err(|_| failed())?;
    // Mozilla's signature files: without them release Firefox refuses the package.
    let signed = |marker: &[u8]| bytes.windows(marker.len()).any(|w| w == marker);
    if bytes.len() as u64 > MAX_XPI || !(signed(b"META-INF/mozilla.rsa") || signed(b"META-INF/cose.sig")) {
        return Ok(false);
    }
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| e.to_string())?;
    }
    let tmp = crate::settings::with_suffix(dest, ".tmp");
    tokio::fs::write(&tmp, &bytes).await.map_err(|e| e.to_string())?;
    tokio::fs::rename(&tmp, dest).await.map_err(|e| e.to_string())?;
    Ok(true)
}

// ── Downloading and checking ──────────────────────────────────────────────

/// Downloads `package` of `version` and checks it (size, SHA-256, format, signature);
/// `progress` gets the fraction done.
pub async fn download(client: &reqwest::Client, version: &str, package: &Package, progress: impl Fn(f32)) -> Result<PathBuf, String> {
    if !trusted(&package.url) {
        return Err(tr!("adresse de téléchargement inattendue", "unexpected download address").into());
    }
    let Some(sig_url) = package.signature.as_deref().filter(|u| trusted(u)) else {
        return Err(tr!("mise à jour non signée : installation refusée", "unsigned update: installation refused").into());
    };
    let signature = fetch_small(client, sig_url).await?;
    let safe: String = version.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')).collect();
    let extension = package.name.rsplit_once('.').map_or("bin", |(_, e)| e);
    let extension = if package.name.ends_with(".tar.gz") { "tar.gz" } else { extension };
    let dir = work_dir().map_err(|e| e.to_string())?;
    let path = dir.join(format!("rdm-update-{safe}.{extension}"));
    let mut last = String::new();
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
        }
        let result = fetch(client, version, package, &path, &signature, &progress).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(crate::settings::with_suffix(&path, ".tmp")).await;
        }
        match result {
            Ok(()) => return Ok(path),
            Err(Fatal(reason)) => return Err(reason),
            Err(Retry(reason)) => last = reason,
        }
    }
    Err(last)
}

/// A small file (a signature), with retries.
async fn fetch_small(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
        }
        let res = client.get(url).timeout(Duration::from_secs(30)).send().await.and_then(reqwest::Response::error_for_status);
        if let Ok(res) = res
            && let Ok(bytes) = res.bytes().await
            && bytes.len() <= 1024
        {
            return Ok(bytes.to_vec());
        }
    }
    Err(tr!("signature de la mise à jour introuvable", "cannot fetch the update's signature").into())
}

enum Failure {
    Retry(String),
    Fatal(String),
}
use Failure::{Fatal, Retry};

async fn fetch(client: &reqwest::Client, version: &str, package: &Package, path: &Path, signature: &[u8], progress: &impl Fn(f32)) -> Result<(), Failure> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let res = client.get(&package.url).send().await.map_err(|_| Retry(tr!("GitHub est injoignable", "GitHub cannot be reached").into()))?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let reason = crate::trf!("téléchargement refusé par GitHub ({status})", "download refused by GitHub ({status})");
        return Err(if res.status().is_server_error() || status == 429 { Retry(reason) } else { Fatal(reason) });
    }
    let too_big = || Fatal(tr!("paquet anormalement gros", "abnormally large package").into());
    let total = if package.size > 0 { package.size } else { res.content_length().unwrap_or(0) };
    if total > MAX_PACKAGE {
        return Err(too_big());
    }
    let tmp = crate::settings::with_suffix(path, ".tmp");
    let io = |e: std::io::Error| Fatal(crate::trf!("écriture impossible : {e}", "cannot write: {e}"));
    let mut file = tokio::fs::File::create(&tmp).await.map_err(io)?;
    // Streamed to disk: only the first bytes (the format) and the running hash stay in memory.
    let (mut stream, mut head, mut received, mut hash) = (res.bytes_stream(), Vec::with_capacity(16), 0u64, Sha256::new());
    loop {
        let chunk = match tokio::time::timeout(STALL, stream.next()).await {
            Err(_) => return Err(Retry(tr!("téléchargement bloqué (connexion)", "download stalled (connection)").into())),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(Retry(tr!("téléchargement interrompu", "download interrupted").into())),
            Ok(Some(Ok(chunk))) => chunk,
        };
        received += chunk.len() as u64;
        if received > MAX_PACKAGE {
            return Err(too_big());
        }
        if head.len() < 16 {
            head.extend_from_slice(&chunk[..chunk.len().min(16 - head.len())]);
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(io)?;
        if total > 0 {
            progress((received as f32 / total as f32).min(1.0));
        }
    }
    file.sync_all().await.map_err(io)?;
    drop(file);

    if total > 0 && received != total {
        return Err(Retry(tr!("paquet incomplet", "incomplete package").into()));
    }
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if package.sha256.as_ref().is_some_and(|expected| *expected != digest) {
        return Err(Retry(tr!("paquet corrompu (empreinte SHA-256 différente)", "corrupted package (SHA-256 mismatch)").into()));
    }
    if !format_matches(&package.name, &head) {
        return Err(Retry(tr!("le fichier reçu n'est pas le paquet attendu", "the file received is not the expected package").into()));
    }
    if !signature_valid(signed_statement(version, &package.name, &digest).as_bytes(), signature) {
        return Err(Fatal(tr!("signature de la mise à jour invalide : installation refusée", "invalid update signature: installation refused").into()));
    }
    tokio::fs::rename(&tmp, path).await.map_err(io)
}

/// The package's own format, from its first bytes.
fn format_matches(name: &str, data: &[u8]) -> bool {
    let name = name.to_ascii_lowercase();
    let magic: &[u8] = if name.ends_with(".msi") {
        &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] // OLE compound file
    } else if name.ends_with(".tar.gz") {
        &[0x1F, 0x8B]
    } else if name.ends_with(".deb") {
        b"!<arch>\n"
    } else if name.ends_with(".rpm") {
        &[0xED, 0xAB, 0xEE, 0xDB]
    } else {
        return false;
    };
    data.starts_with(magic)
}

/// What the release workflow signs for each package (`.github/workflows/release.yml`): the version
/// it belongs to, its name and its SHA-256. Binding the version defeats replays: an older signed
/// package (with a since-fixed flaw) cannot be served again as a newer release.
fn signed_statement(version: &str, name: &str, sha256: &str) -> String {
    format!("rdm-update\nversion={version}\nname={name}\nsha256={sha256}\n")
}

/// Ed25519 signature of `message` by the release key.
fn signature_valid(message: &[u8], signature: &[u8]) -> bool {
    use ed25519_dalek::{Signature, VerifyingKey};
    let (Ok(key), Ok(sig)) = (VerifyingKey::from_bytes(&PUBLIC_KEY), <[u8; 64]>::try_from(signature)) else { return false };
    key.verify_strict(message, &Signature::from_bytes(&sig)).is_ok()
}

// ── Installing ─────────────────────────────────────────────────────────────

/// The command-line flag of the installation assistant: `rdm --apply-update <msi> <pid> <exe>`.
pub const HELPER_FLAG: &str = "--apply-update";

/// Where the Windows installer puts RDM (per user).
pub fn installed_exe() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"Programs\RDM\rdm.exe"))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Windows: starts the installation assistant for `msi` (see the module docs). The caller quits
/// right after.
pub fn start_installation(msi: &Path) -> std::io::Result<()> {
    let target = installed_exe().ok_or(std::io::ErrorKind::Unsupported)?;
    let helper = std::env::temp_dir().join(format!("rdm-updater-{}.exe", std::process::id()));
    std::fs::copy(startup_exe().map_or_else(std::env::current_exe, |p| Ok(p.to_path_buf()))?, &helper)?;
    let mut command = std::process::Command::new(&helper);
    command.arg(HELPER_FLAG).arg(msi).arg(std::process::id().to_string()).arg(&target);
    imp::spawn_outliving(&mut command)
}

/// Linux: installs the downloaded package (binary replaced in place, or the package manager behind
/// the system's password prompt). RDM restarts afterwards.
pub async fn install_linux(package: PathBuf) -> Result<(), String> {
    tokio::task::spawn_blocking(move || linux::install(method(), &package)).await.map_err(|e| e.to_string())?
}

/// After quitting for an update: starts the (new) RDM from where it was installed.
pub fn relaunch() {
    if let Some(exe) = startup_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// In the assistant: `true` when `args` asked for it (it then did its job).
pub fn run_assistant(args: &[String]) -> bool {
    let [flag, msi, pid, exe] = args else { return false };
    if flag != HELPER_FLAG {
        return false;
    }
    imp::apply(Path::new(msi), pid.parse().unwrap_or(0), Path::new(exe));
    true
}

/// Where update packages are downloaded: the user's own temporary folder on Windows; on Linux a
/// private folder (`~/.cache/rdm`, 0700) rather than the shared `/tmp`, where another account
/// could prepare a file or a link under the expected name.
fn work_dir() -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = directories::BaseDirs::new().ok_or(std::io::ErrorKind::NotFound)?.cache_dir().join("rdm");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }
    #[cfg(not(unix))]
    {
        Ok(std::env::temp_dir())
    }
}

/// At start: leftovers of a previous update (package, assistant copy, old logs). Files still in
/// use (an assistant finishing its job) are left for next time.
pub fn clean_leftovers() {
    let Ok(dir) = work_dir().and_then(std::fs::read_dir) else { return };
    let week = Duration::from_secs(7 * 24 * 3600);
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = || entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > week));
        let update = name.starts_with("rdm-update-");
        let leftover = name.starts_with("rdm-updater-") && name.ends_with(".exe")
            || update && [".msi", ".tmp", ".tar.gz", ".deb", ".rpm"].iter().any(|e| name.ends_with(e))
            || update && name.ends_with(".log") && old();
        if leftover {
            let _ = std::fs::remove_file(entry.path());
        } else if update && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path()); // an unpacked tarball (Linux)
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};

    use super::{Method, startup_exe, which};
    use crate::{tr, trf};

    pub fn install(method: Method, package: &Path) -> Result<(), String> {
        match method {
            Method::Binary => replace_binary(package),
            Method::Deb => {
                let tool = if which("apt-get").is_some() { vec!["apt-get", "install", "-y", "--allow-downgrades"] } else { vec!["dpkg", "-i"] };
                elevated(&tool, package)
            }
            Method::Rpm => {
                let tool = if which("dnf").is_some() {
                    vec!["dnf", "install", "-y"]
                } else if which("zypper").is_some() {
                    vec!["zypper", "--non-interactive", "install", "--allow-unsigned-rpm"]
                } else {
                    vec!["rpm", "-U", "--replacepkgs"]
                };
                elevated(&tool, package)
            }
            Method::Msi | Method::Manual => Err(tr!("mise à jour impossible ici", "cannot update this copy").into()),
        }
    }

    /// The package manager, as root after the system's password prompt (polkit).
    fn elevated(tool: &[&str], package: &Path) -> Result<(), String> {
        let status = Command::new("pkexec").args(tool).arg(package).status().map_err(|e| e.to_string())?;
        match status.code() {
            Some(0) => Ok(()),
            Some(126 | 127) => Err(tr!("mot de passe refusé ou demande fermée", "password refused or prompt closed").into()),
            Some(c) => Err(trf!("le gestionnaire de paquets a échoué (code {c})", "the package manager failed (code {c})")),
            None => Err(tr!("installation interrompue", "installation interrupted").into()),
        }
    }

    /// Unpacks the tarball and swaps the binary in place (a rename: the running process keeps its
    /// file until it exits, and a crash half-way leaves the old binary intact).
    fn replace_binary(archive: &Path) -> Result<(), String> {
        let exe = startup_exe().ok_or_else(|| tr!("emplacement de RDM inconnu", "RDM's location is unknown").to_owned())?;
        let work = super::work_dir().map_err(|e| e.to_string())?.join(format!("rdm-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let result = (|| {
            let ok = Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(&work).status().is_ok_and(|s| s.success());
            let new = work.join("rdm-linux-x64").join("rdm");
            let elf = std::fs::read(&new).is_ok_and(|b| b.starts_with(b"\x7fELF"));
            if !ok || !elf {
                return Err(tr!("archive de mise à jour illisible", "unreadable update archive").to_owned());
            }
            let staged = exe.with_file_name(".rdm-update");
            std::fs::copy(&new, &staged).map_err(|e| e.to_string())?;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            std::fs::rename(&staged, exe).map_err(|e| e.to_string())
        })();
        let _ = std::fs::remove_dir_all(&work);
        result
    }
}

#[cfg(not(target_os = "linux"))]
mod linux {
    use std::path::Path;

    use super::Method;

    pub fn install(_: Method, _: &Path) -> Result<(), String> {
        Err(crate::tr!("mise à jour impossible ici", "cannot update this copy").into())
    }
}

#[cfg(windows)]
mod imp {
    use std::{
        net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
        os::windows::process::CommandExt,
        path::Path,
        process::Command,
        thread::sleep,
        time::{Duration, Instant},
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject},
        UI::WindowsAndMessaging::{IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO, MessageBoxW},
    };

    use crate::{settings::BRIDGE_PORT, tr, trf};

    /// Outside a job that would end it together with RDM (when the job allows it).
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    /// How long RDM gets to close cleanly (downloads saved) before the assistant ends it.
    const CLOSE_GRACE: Duration = Duration::from_secs(30);
    /// Another installation running (Windows Update…): the installer is retried for this long.
    const BUSY_RETRIES: u32 = 30;

    pub fn spawn_outliving(command: &mut Command) -> std::io::Result<()> {
        match command.creation_flags(CREATE_BREAKAWAY_FROM_JOB).spawn() {
            Ok(_) => Ok(()),
            Err(_) => command.creation_flags(0).spawn().map(drop),
        }
    }

    pub fn apply(msi: &Path, pid: u32, exe: &Path) {
        wait_for_exit(pid);
        // The single-instance lock (the bridge port) is the last thing RDM lets go.
        wait_until(Duration::from_secs(15), || TcpListener::bind((Ipv4Addr::LOCALHOST, BRIDGE_PORT)).is_ok());

        let log = msi.with_extension("log");
        // Silent: no window at all; RDM's own card said "installing", and RDM comes back.
        let code = msiexec(&["/i".as_ref(), msi.as_os_str(), "/qn".as_ref(), "/norestart".as_ref(), "/l*v".as_ref(), log.as_os_str()]);
        // 3010 / 1641: installed, a restart completes it.
        if !matches!(code, Some(0 | 3010 | 1641)) {
            let reason = match code {
                Some(1602) => tr!("installation annulée", "installation cancelled").to_owned(),
                Some(1603) => tr!("erreur de Windows Installer (1603)", "Windows Installer error (1603)").to_owned(),
                Some(1618) => tr!("une autre installation est en cours (1618)", "another installation is in progress (1618)").to_owned(),
                Some(c) => trf!("code {c}", "code {c}"),
                None => tr!("Windows Installer n'a pas pu être lancé", "Windows Installer could not be started").to_owned(),
            };
            let log = log.display();
            let text = trf!(
                "La mise à jour de RDM n'a pas pu s'installer : {reason}.\n\nOuvrir l'installateur pour réessayer ? (Sinon, RDM redémarre dans sa version actuelle.)\n\nJournal : {log}",
                "RDM's update could not be installed: {reason}.\n\nOpen the installer to try again? (Otherwise RDM restarts in its current version.)\n\nLog: {log}"
            );
            if ask(&text) {
                msiexec(&["/i".as_ref(), msi.as_os_str()]);
            }
        }
        // The installer starts RDM itself when it succeeds; if it did not (or it failed), here.
        if !wait_until(Duration::from_secs(12), rdm_running) {
            let _ = Command::new(exe).spawn();
        }
    }

    /// Waits for RDM (`pid`) to exit; ends it if it does not within `CLOSE_GRACE`.
    fn wait_for_exit(pid: u32) {
        // SAFETY: plain Win32 calls on a handle we own and close.
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return; // already gone
            }
            if WaitForSingleObject(process, CLOSE_GRACE.as_millis() as u32) == WAIT_TIMEOUT {
                TerminateProcess(process, 1);
                WaitForSingleObject(process, 10_000);
            }
            CloseHandle(process);
        }
    }

    /// `msiexec` with `args`, waited for; retried while another installation is running. `None`
    /// when it could not be started.
    fn msiexec(args: &[&std::ffi::OsStr]) -> Option<i32> {
        let exe = std::env::var_os("SystemRoot").map_or_else(|| "msiexec.exe".into(), |root| Path::new(&root).join(r"System32\msiexec.exe"));
        for _ in 0..BUSY_RETRIES {
            let code = Command::new(&exe).args(args).status().ok()?.code();
            if code != Some(1618) {
                return code;
            }
            sleep(Duration::from_secs(10));
        }
        Some(1618)
    }

    fn rdm_running() -> bool {
        TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, BRIDGE_PORT)), Duration::from_millis(300)).is_ok()
    }

    fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        loop {
            if done() {
                return true;
            }
            if start.elapsed() >= limit {
                return false;
            }
            sleep(Duration::from_millis(250));
        }
    }

    fn ask(text: &str) -> bool {
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(text), wide(tr!("RDM — mise à jour", "RDM — update")));
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call; no owner window.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND) == IDYES }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::{path::Path, process::Command};

    pub fn spawn_outliving(_: &mut Command) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    pub fn apply(_: &Path, _: u32, _: &Path) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(is_newer("v0.2.0-beta", "0.1.0"));
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            browser_download_url: format!("https://github.com/o/r/releases/download/v1/{name}"),
            size: 42,
            digest: Some("sha256:ABCDEF".into()),
        }
    }

    #[test]
    fn picks_the_package_of_each_installation_method() {
        let mut assets = vec![
            asset("RDM-1.0.0-x64-setup.msi"),
            asset("RDM-1.0.0-x64-setup.msi.sig"),
            asset("rdm-linux-x64.tar.gz"),
            asset("rdm_1.0.0-1_amd64.deb"),
            asset("rdm_1.0.0-1_amd64.deb.sig"),
            asset("rdm-1.0.0-1.x86_64.rpm"),
            asset("rdm-1.0.0-1.x86_64.rpm.sig"),
            asset("rdm-firefox.xpi"),
        ];
        let msi = package(&assets, Method::Msi).unwrap();
        assert!(msi.url.ends_with("RDM-1.0.0-x64-setup.msi"));
        assert_eq!((msi.size, msi.sha256.as_deref()), (42, Some("abcdef")));
        assert!(msi.signature.unwrap().ends_with(".msi.sig"));
        assert!(package(&assets, Method::Binary).is_none(), "its signature is not online yet");
        assets.push(asset("rdm-linux-x64.tar.gz.sig"));
        assert_eq!(package(&assets, Method::Binary).unwrap().name, "rdm-linux-x64.tar.gz");
        assert_eq!(package(&assets, Method::Deb).unwrap().name, "rdm_1.0.0-1_amd64.deb");
        assert_eq!(package(&assets, Method::Rpm).unwrap().name, "rdm-1.0.0-1.x86_64.rpm");
        assert!(package(&assets, Method::Manual).is_none());
    }

    #[test]
    fn older_versions_do_not_recognise_the_new_installer_name() {
        // RDM 0.1 / 0.2.0 look for a name ending with "x64.msi" and have a broken installation
        // step: they must fall back to opening the release page.
        let name = format!("RDM-{}-x64-setup.msi", env!("CARGO_PKG_VERSION"));
        assert!(!name.to_ascii_lowercase().ends_with("x64.msi"));
    }

    #[test]
    fn signatures_are_checked_against_the_release_key() {
        // Made with the release key exactly as the release workflow does (`openssl pkeyutl -sign
        // -rawin` over the statement).
        const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let hex = "58bc8644f48626024e184357d866862636c6c1e0a87bbfe45437383bae392645\
                   a728ed1a840668d9e9147528bead52304664bd6fd2199b86f8acb05717d23c04";
        let good: Vec<u8> = (0..64).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
        let statement = |version: &str, name: &str, sha: &str| signed_statement(version, name, sha).into_bytes();
        assert!(signature_valid(&statement("0.3.0", "RDM-0.3.0-x64-setup.msi", EMPTY), &good));
        assert!(!signature_valid(&statement("9.9.9", "RDM-0.3.0-x64-setup.msi", EMPTY), &good), "an old package replayed as newer");
        assert!(!signature_valid(&statement("0.3.0", "rdm_0.3.0-1_amd64.deb", EMPTY), &good), "another package");
        assert!(!signature_valid(&statement("0.3.0", "RDM-0.3.0-x64-setup.msi", &EMPTY.replace('e', "f")), &good), "other content");
        let mut forged = good.clone();
        forged[10] ^= 1;
        assert!(!signature_valid(&statement("0.3.0", "RDM-0.3.0-x64-setup.msi", EMPTY), &forged), "a forged signature");
        assert!(!signature_valid(&statement("0.3.0", "RDM-0.3.0-x64-setup.msi", EMPTY), &good[..12]), "a malformed one");
    }

    #[test]
    fn formats_are_recognised() {
        assert!(format_matches("a.msi", &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0]));
        assert!(format_matches("a.tar.gz", &[0x1F, 0x8B, 8]));
        assert!(format_matches("a.deb", b"!<arch>\ndebian"));
        assert!(!format_matches("a.msi", b"<html>"));
        assert!(!format_matches("a.exe", b"MZ"));
    }

    /// The latest published release, end to end: every package downloads and passes the checks
    /// (GitHub's SHA-256, format, Ed25519 signature). `cargo test -p rdm -- --ignored published`
    #[tokio::test]
    #[ignore = "network: downloads the published release"]
    async fn published_release_packages_verify() {
        let client = reqwest::Client::builder().user_agent("rdm-test").build().unwrap();
        let release = latest(&client).await.unwrap().expect("a published release");
        let version = release.tag_name.trim_start_matches(['v', 'V']).to_owned();
        for method in [Method::Msi, Method::Binary, Method::Deb, Method::Rpm] {
            let package = package(&release.assets, method).unwrap_or_else(|| panic!("{method:?}: no signed package"));
            let file = download(&client, &version, &package, |_| {}).await.unwrap_or_else(|e| panic!("{method:?}: {e}"));
            println!("{method:?}: {} verified", package.name);
            let _ = std::fs::remove_file(file);
        }
        // Replayed as another version, the same packages are refused.
        let msi = package(&release.assets, Method::Msi).unwrap();
        assert!(download(&client, "99.0.0", &msi, |_| {}).await.is_err());
    }

    #[test]
    fn only_github_pages_are_opened() {
        let page = "https://github.com/o/r/releases/tag/v9.0.0";
        assert_eq!(release_page(page.into()), page);
        for odd in [r"C:\Windows\System32\calc.exe", "file:///etc/passwd", "https://github.com.evil.io/x", "https://github.com/x y"] {
            assert!(release_page(odd.into()).ends_with("/releases/latest"), "{odd}");
        }
    }

    #[test]
    fn assistant_arguments() {
        assert!(!run_assistant(&["--minimized".into()]));
        assert!(!run_assistant(&["--other".into(), "a".into(), "1".into(), "b".into()]));
    }
}
