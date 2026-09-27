//! Updates from the project's GitHub releases: one request to the GitHub API (at start, then daily,
//! or on demand), nothing else. Elsewhere than on Windows the release page is opened.
//!
//! Windows installs in place, built so that RDM is never left closed with nothing happening:
//! 1. the installer (`.msi`, per-user: no administrator prompt) is downloaded from GitHub only,
//!    with retries, then checked — size and SHA-256 published by GitHub, MSI file signature;
//! 2. a copy of `rdm.exe` is started as the installation assistant (a copy: the installed file
//!    must stay free for the installer to replace), and RDM quits;
//! 3. the assistant waits until RDM is really gone (and ends it if it hangs), runs `msiexec`
//!    (progress bar, log file), then makes sure RDM runs again — the new version, or the old one
//!    after saying why the installation failed.
//!
//! No PowerShell: RDM 0.1 and 0.2 started it without a console, which PowerShell 5.1 silently
//! refuses to run in — RDM closed and nothing was installed. Their installer asset names end
//! with `x64.msi`; installers are now named `…-x64-setup.msi`, which those versions do not
//! recognise: they offer the release page instead of closing for nothing.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

/// `owner/repo`, from `repository` in Cargo.toml: the single source for where releases live.
pub fn repo() -> Option<&'static str> {
    let url = env!("CARGO_PKG_REPOSITORY");
    let path = url.strip_prefix("https://github.com/")?.trim_end_matches('/');
    (path.split('/').count() == 2 && !path.contains("OWNER")).then_some(path)
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
    /// The installer is ready: RDM is closing to let it run.
    Installing,
    /// Downloading or starting the installer failed: the update stays on offer.
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
    /// Windows installer asset, if the release has one.
    pub msi: Option<Installer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installer {
    pub url: String,
    /// Size announced by GitHub (0 = unknown).
    pub size: u64,
    /// SHA-256 computed by GitHub when the asset was uploaded (lower-case hex), if published.
    pub sha256: Option<String>,
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

/// The Windows installer among a release's assets.
fn installer(assets: &[Asset]) -> Option<Installer> {
    let asset = assets.iter().find(|a| {
        let name = a.name.to_ascii_lowercase();
        name.ends_with(".msi") && name.contains("x64")
    })?;
    let sha256 = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase);
    Some(Installer { url: asset.browser_download_url.clone(), size: asset.size, sha256 })
}

async fn latest(client: &reqwest::Client) -> Result<Option<ApiRelease>, String> {
    let repo = repo().ok_or("dépôt GitHub non configuré")?;
    let res = client
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| "GitHub est injoignable".to_owned())?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None); // no release published yet
    }
    if !res.status().is_success() {
        return Err(format!("GitHub a répondu {}", res.status().as_u16()));
    }
    let body = res.bytes().await.map_err(|_| "GitHub est injoignable".to_owned())?;
    let r: ApiRelease = serde_json::from_slice(&body).map_err(|_| "réponse inattendue de GitHub".to_owned())?;
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
        page: r.html_url,
        notes: r.body.unwrap_or_default(),
        msi: installer(&r.assets),
    }))
}

/// Where release assets may be downloaded from (HTTPS, GitHub only).
const TRUSTED: [&str; 3] = ["https://github.com/", "https://objects.githubusercontent.com/", "https://release-assets.githubusercontent.com/"];
const FIREFOX_XPI: &str = "rdm-firefox.xpi";
const MAX_XPI: u64 = 20 << 20;
const MAX_MSI: u64 = 200 << 20;
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
    let res = client
        .get(&asset.browser_download_url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| "téléchargement de l'extension impossible".to_owned())?;
    if res.content_length().is_some_and(|n| n > MAX_XPI) {
        return Ok(false);
    }
    let bytes = res.bytes().await.map_err(|_| "téléchargement de l'extension interrompu".to_owned())?;
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

/// Downloads and checks the installer of `version`; `progress` gets the fraction done.
pub async fn download(client: &reqwest::Client, version: &str, installer: &Installer, progress: impl Fn(f32)) -> Result<PathBuf, String> {
    if !trusted(&installer.url) {
        return Err("adresse de téléchargement inattendue".into());
    }
    let safe: String = version.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')).collect();
    let path = std::env::temp_dir().join(format!("rdm-update-{safe}.msi"));
    let mut last = String::new();
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
        }
        let result = fetch(client, installer, &path, &progress).await;
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

enum Failure {
    Retry(String),
    Fatal(String),
}
use Failure::{Fatal, Retry};

async fn fetch(client: &reqwest::Client, installer: &Installer, path: &Path, progress: &impl Fn(f32)) -> Result<(), Failure> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let res = client.get(&installer.url).send().await.map_err(|_| Retry("GitHub est injoignable".into()))?;
    if !res.status().is_success() {
        let status = res.status();
        let reason = format!("téléchargement refusé par GitHub ({})", status.as_u16());
        return Err(if status.is_server_error() || status.as_u16() == 429 { Retry(reason) } else { Fatal(reason) });
    }
    let total = if installer.size > 0 { installer.size } else { res.content_length().unwrap_or(0) };
    if total > MAX_MSI {
        return Err(Fatal("installateur anormalement gros".into()));
    }
    let tmp = crate::settings::with_suffix(path, ".tmp");
    let io = |e: std::io::Error| Fatal(format!("écriture impossible : {e}"));
    let mut file = tokio::fs::File::create(&tmp).await.map_err(io)?;
    let (mut stream, mut done, mut hash) = (res.bytes_stream(), 0u64, Sha256::new());
    loop {
        let chunk = match tokio::time::timeout(STALL, stream.next()).await {
            Err(_) => return Err(Retry("téléchargement bloqué (connexion)".into())),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(Retry("téléchargement interrompu".into())),
            Ok(Some(Ok(chunk))) => chunk,
        };
        done += chunk.len() as u64;
        if done > MAX_MSI {
            return Err(Fatal("installateur anormalement gros".into()));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(io)?;
        if total > 0 {
            progress((done as f32 / total as f32).min(1.0));
        }
    }
    file.sync_all().await.map_err(io)?;
    drop(file);

    if total > 0 && done != total {
        return Err(Retry("installateur incomplet".into()));
    }
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if installer.sha256.as_ref().is_some_and(|expected| *expected != digest) {
        return Err(Retry("installateur corrompu (empreinte SHA-256 différente)".into()));
    }
    let mut head = [0u8; 8];
    let read = std::fs::File::open(&tmp).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head));
    // Every .msi is an OLE compound file.
    if read.is_err() || head != [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
        return Err(Retry("le fichier reçu n'est pas un installateur".into()));
    }
    tokio::fs::rename(&tmp, path).await.map_err(io)
}

/// The command-line flag of the installation assistant: `rdm --apply-update <msi> <pid> <exe>`.
pub const HELPER_FLAG: &str = "--apply-update";

/// Where the installer puts RDM (per user).
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

/// RDM can install updates itself: Windows, and this is the copy the installer manages (a copy
/// run from a build folder or a USB stick would not be the one replaced). Computed once.
pub fn can_self_install() -> bool {
    static CAN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CAN.get_or_init(|| {
        let (Some(installed), Ok(me)) = (installed_exe(), std::env::current_exe()) else { return false };
        let norm = |p: &Path| std::fs::canonicalize(p).map(|p| p.to_string_lossy().to_lowercase());
        matches!((norm(&installed), norm(&me)), (Ok(a), Ok(b)) if a == b)
    })
}

/// Starts the installation assistant for `msi` (see the module docs). The caller quits right after.
pub fn start_installation(msi: &Path) -> std::io::Result<()> {
    let target = installed_exe().ok_or(std::io::ErrorKind::Unsupported)?;
    let helper = std::env::temp_dir().join(format!("rdm-updater-{}.exe", std::process::id()));
    std::fs::copy(std::env::current_exe()?, &helper)?;
    let mut command = std::process::Command::new(&helper);
    command.arg(HELPER_FLAG).arg(msi).arg(std::process::id().to_string()).arg(&target);
    imp::spawn_outliving(&mut command)
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

/// At start: leftovers of a previous update (installer, assistant copy, old logs). Files still in
/// use (an assistant finishing its job) are left for next time.
pub fn clean_leftovers() {
    let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else { return };
    let week = Duration::from_secs(7 * 24 * 3600);
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = || entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > week));
        let leftover = name.starts_with("rdm-updater-") && name.ends_with(".exe")
            || name.starts_with("rdm-update-") && (name.ends_with(".msi") || name.ends_with(".tmp"))
            || name.starts_with("rdm-update-") && name.ends_with(".log") && old();
        if leftover {
            let _ = std::fs::remove_file(entry.path());
        }
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

    use crate::settings::BRIDGE_PORT;

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
        let code = msiexec(&["/i".as_ref(), msi.as_os_str(), "/passive".as_ref(), "/norestart".as_ref(), "/l*v".as_ref(), log.as_os_str()]);
        // 3010 / 1641: installed, a restart completes it.
        if !matches!(code, Some(0 | 3010 | 1641)) {
            let reason = match code {
                Some(1602) => "installation annulée".to_owned(),
                Some(1603) => "erreur de Windows Installer (1603)".to_owned(),
                Some(1618) => "une autre installation est en cours (1618)".to_owned(),
                Some(c) => format!("code {c}"),
                None => "Windows Installer n'a pas pu être lancé".to_owned(),
            };
            let text = format!(
                "La mise à jour de RDM n'a pas pu s'installer : {reason}.\n\nOuvrir l'installateur pour réessayer ? \
                 (Sinon, RDM redémarre dans sa version actuelle.)\n\nJournal : {}",
                log.display()
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
        let (text, title) = (wide(text), wide("RDM — mise à jour"));
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

    #[test]
    fn finds_the_installer_with_its_checksum() {
        let asset = |name: &str| Asset {
            name: name.into(),
            browser_download_url: format!("https://github.com/o/r/releases/download/v1/{name}"),
            size: 42,
            digest: Some("sha256:ABCDEF".into()),
        };
        let found = installer(&[asset("rdm-linux-x64.tar.gz"), asset("RDM-1.0.0-x64-setup.msi")]).unwrap();
        assert!(found.url.ends_with("RDM-1.0.0-x64-setup.msi"));
        assert_eq!((found.size, found.sha256.as_deref()), (42, Some("abcdef")));
        assert!(installer(&[asset("rdm_1.0.0-1_amd64.deb")]).is_none());
    }

    #[test]
    fn older_versions_do_not_recognise_the_new_installer_name() {
        // RDM 0.1 / 0.2 look for a name ending with "x64.msi" and have a broken installation
        // step: they must fall back to opening the release page.
        let name = format!("RDM-{}-x64-setup.msi", env!("CARGO_PKG_VERSION"));
        assert!(!name.to_ascii_lowercase().ends_with("x64.msi"));
    }

    #[tokio::test]
    #[ignore = "network: downloads the latest release's installer from GitHub"]
    async fn downloads_and_checks_the_published_installer() {
        let client = crate::virustotal::client().unwrap();
        let release = latest(&client).await.unwrap().unwrap();
        let found = installer(&release.assets).expect("the latest release has an installer");
        let path = download(&client, "selftest", &found, |_| {}).await.unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), found.size);
        let _ = std::fs::remove_file(path);
        // A file that does not match GitHub's checksum is refused.
        let tampered = Installer { sha256: Some("0".repeat(64)), ..found };
        let refused = download(&client, "selftest-bad", &tampered, |_| {}).await.unwrap_err();
        assert!(refused.contains("SHA-256"), "{refused}");
    }

    #[test]
    fn assistant_arguments() {
        assert!(!run_assistant(&["--minimized".into()]));
        assert!(!run_assistant(&["--other".into(), "a".into(), "1".into(), "b".into()]));
    }
}
