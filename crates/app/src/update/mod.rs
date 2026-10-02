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
mod install;
mod package;

pub use install::{clean_leftovers, install_linux, installed_exe, relaunch, run_assistant, start_installation};
use install::work_dir;
pub use package::{Verified, download};


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
        return Err(crate::trf!("GitHub a répondu {status}", "GitHub answered {status}", status = status));
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
    fn only_github_pages_are_opened() {
        let page = "https://github.com/o/r/releases/tag/v9.0.0";
        assert_eq!(release_page(page.into()), page);
        for odd in [r"C:\Windows\System32\calc.exe", "file:///etc/passwd", "https://github.com.evil.io/x", "https://github.com/x y"] {
            assert!(release_page(odd.into()).ends_with("/releases/latest"), "{odd}");
        }
    }
}
