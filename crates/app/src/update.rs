//! Updates from the project's GitHub releases: one request to the GitHub API (at start, then daily,
//! or on demand), nothing else. On Windows the new `.msi` is downloaded and installed in place
//! (per-user, no admin prompt), then RDM restarts; elsewhere the release page is opened.

use std::{path::PathBuf, time::Duration};

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
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub page: String,
    pub notes: String,
    /// Windows installer asset, if the release has one.
    pub msi: Option<String>,
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

/// The latest published release if it is newer than this build.
pub async fn check(client: &reqwest::Client) -> Result<Option<Release>, String> {
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
    if r.draft || r.prerelease || !is_newer(&r.tag_name, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let msi = r.assets.iter().find(|a| a.name.to_ascii_lowercase().ends_with("x64.msi")).map(|a| a.browser_download_url.clone());
    Ok(Some(Release {
        version: r.tag_name.trim_start_matches(['v', 'V']).to_owned(),
        page: r.html_url,
        notes: r.body.unwrap_or_default(),
        msi,
    }))
}

/// Where release assets may be downloaded from (HTTPS, GitHub only).
const TRUSTED: [&str; 3] = ["https://github.com/", "https://objects.githubusercontent.com/", "https://release-assets.githubusercontent.com/"];
const FIREFOX_XPI: &str = "rdm-firefox.xpi";
const MAX_XPI: u64 = 20 << 20;

/// The signed Firefox package of the latest release, written to `dest`: release Firefox installs
/// it for good (an unsigned package only temporarily). `Ok(false)` when there is none — no
/// release, a release older than this RDM, or a package Mozilla has not signed.
pub async fn signed_firefox_xpi(client: &reqwest::Client, dest: &std::path::Path) -> Result<bool, String> {
    let repo = repo().ok_or("dépôt GitHub non configuré")?;
    let res = client
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| "GitHub est injoignable".to_owned())?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    let body = res.error_for_status().map_err(|e| format!("GitHub a répondu {}", e.status().map_or(0, |s| s.as_u16())))?;
    let r: ApiRelease = serde_json::from_slice(&body.bytes().await.map_err(|_| "GitHub est injoignable".to_owned())?)
        .map_err(|_| "réponse inattendue de GitHub".to_owned())?;
    if r.draft || r.prerelease || is_newer(env!("CARGO_PKG_VERSION"), &r.tag_name) {
        return Ok(false);
    }
    let Some(asset) = r.assets.iter().find(|a| a.name == FIREFOX_XPI) else { return Ok(false) };
    if !TRUSTED.iter().any(|p| asset.browser_download_url.starts_with(p)) {
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

/// Downloads the installer (only from GitHub, over HTTPS); `progress` gets the fraction done.
pub async fn download(client: &reqwest::Client, url: &str, progress: impl Fn(f32)) -> Result<PathBuf, String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    if !TRUSTED.iter().any(|p| url.starts_with(p)) {
        return Err("adresse de téléchargement inattendue".into());
    }
    let res = client.get(url).send().await.and_then(reqwest::Response::error_for_status).map_err(|_| "téléchargement impossible".to_owned())?;
    let total = res.content_length().unwrap_or(0);
    let path = std::env::temp_dir().join(format!("rdm-update-{}.msi", std::process::id()));
    let mut file = tokio::fs::File::create(&path).await.map_err(|e| e.to_string())?;
    let (mut stream, mut done) = (res.bytes_stream(), 0u64);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "téléchargement interrompu".to_owned())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        if total > 0 {
            progress(done as f32 / total as f32);
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    if done < 64 * 1024 {
        return Err("installateur incomplet".into());
    }
    Ok(path)
}

/// Windows: installs the `.msi` once RDM has exited (msiexec, per-user: no admin prompt), then
/// starts the new version. The caller quits right after.
pub fn install_after_exit(msi: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED: u32 = 0x0000_0008 | 0x0800_0000; // DETACHED_PROCESS | CREATE_NO_WINDOW
        let exe = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"Programs\RDM\rdm.exe"));
        let exe = exe.map(|e| e.display().to_string()).unwrap_or_default();
        let script = format!(
            "Start-Sleep 3; Start-Process msiexec -Wait -ArgumentList '/i','\"{}\"','/passive','/norestart'; Start-Process '{}'",
            msi.display(),
            exe.replace('\'', "''")
        );
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
            .creation_flags(DETACHED)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = msi;
        Err(std::io::ErrorKind::Unsupported.into())
    }
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
}
