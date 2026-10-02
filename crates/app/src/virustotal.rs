//! VirusTotal (API v3) with the user's own free API key. A finished file is first looked up by its
//! SHA-256 — nothing leaves the computer when VirusTotal already knows it — and otherwise uploaded,
//! then RDM waits for the verdict. Everything happens in RDM: the site is never opened.
//!
//! Free API limits: 4 requests per minute; files up to 650 MB (above 32 MB through a one-shot
//! upload URL).

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, StatusCode, multipart};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::io::ReaderStream;

use crate::{tr, trf};

/// VirusTotal refuses larger files: RDM offers no analysis for them.
pub const MAX_UPLOAD: u64 = 650_000_000;
const DIRECT_UPLOAD_MAX: u64 = 32_000_000;
const API: &str = "https://www.virustotal.com/api/v3";
/// Free API: 4 requests per minute, so a verdict is polled every 20 s.
const POLL_EVERY: Duration = Duration::from_secs(20);
const QUOTA_WAIT: Duration = Duration::from_secs(61);
const QUOTA_RETRIES: u32 = 5;
const GIVE_UP_AFTER: Duration = Duration::from_secs(20 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Where users create their free key (the one time the site is needed).
pub const KEY_PAGE: &str = "https://www.virustotal.com/gui/my-apikey";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub malicious: u32,
    pub suspicious: u32,
    pub harmless: u32,
    pub undetected: u32,
    /// Engines that could not analyse this kind of file, timed out or failed.
    pub unavailable: u32,
    /// Engines flagging the file, malicious first.
    pub detections: Vec<Detection>,
    pub sha256: String,
}

impl Report {
    /// Engines that gave a verdict.
    pub fn engines(&self) -> u32 {
        self.malicious + self.suspicious + self.harmless + self.undetected
    }

    pub fn flagged(&self) -> u32 {
        self.malicious + self.suspicious
    }

    pub fn link(&self) -> String {
        format!("https://www.virustotal.com/gui/file/{}", self.sha256)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    pub engine: String,
    pub label: String,
    pub malicious: bool,
}

/// Where a scan stands, for the UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stage {
    /// Behind another scan (the free API allows one at a time).
    Queued,
    Hashing,
    LookingUp,
    /// Fraction sent.
    Uploading(f32),
    Analyzing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadKey,
    Quota,
    TooLarge,
    Status(u16),
    Network,
    Io,
    Timeout,
    Unexpected,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadKey => f.write_str(tr!("clé API VirusTotal refusée : vérifiez-la dans les paramètres", "VirusTotal API key refused: check it in the settings")),
            Self::Quota => f.write_str(tr!("quota de l'API VirusTotal atteint, réessayez plus tard", "VirusTotal API quota reached, try again later")),
            Self::TooLarge => f.write_str(tr!("fichier trop volumineux pour VirusTotal (650 Mo au maximum)", "file too large for VirusTotal (650 MB at most)")),
            Self::Status(s) => f.write_str(&trf!("VirusTotal a répondu {s}", "VirusTotal answered {s}", s = s)),
            Self::Network => f.write_str(tr!("VirusTotal est injoignable (connexion)", "VirusTotal cannot be reached (connection)")),
            Self::Io => f.write_str(tr!("fichier illisible", "unreadable file")),
            Self::Timeout => f.write_str(tr!("VirusTotal n'a pas terminé l'analyse à temps", "VirusTotal did not finish the analysis in time")),
            Self::Unexpected => f.write_str(tr!("réponse inattendue de VirusTotal", "unexpected answer from VirusTotal")),
        }
    }
}

/// A separate client from the download engine's (also used for GitHub): uploads can take long (no
/// read timeout), and HTTP/2 is fine here. Same route as downloads (the proxy of the settings).
pub fn client(route: &engine::Route) -> reqwest::Result<Client> {
    // The API key (`x-apikey`) is not a header the HTTP client knows to drop on a redirect to
    // another site: a redirect out of VirusTotal is not followed. GitHub's downloads (same
    // client) do move to its storage servers.
    let redirects = reqwest::redirect::Policy::custom(|attempt| {
        let from = attempt.previous().last();
        if from.is_some_and(|from| !redirect_keeps_key(from, attempt.url())) {
            attempt.stop()
        } else if attempt.previous().len() > 10 {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    });
    let builder = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(redirects)
        .user_agent(concat!("RDM/", env!("CARGO_PKG_VERSION")));
    engine::route(builder, route)?.build()
}

/// A redirect from `from` to `to` may be followed: anything but leaving VirusTotal.
fn redirect_keeps_key(from: &url::Url, to: &url::Url) -> bool {
    !is_virustotal(from.as_str()) || is_virustotal(to.as_str())
}

pub type OnStage = Arc<dyn Fn(Stage) + Send + Sync>;

/// Verdict for the file at `path`, whose SHA-256 is `sha256`.
pub async fn scan(client: &Client, key: &str, path: &Path, sha256: &str, stage: OnStage) -> Result<Report, Error> {
    stage(Stage::LookingUp);
    match get(client, key, &format!("{API}/files/{sha256}")).await {
        Ok(json) => {
            let attributes = &json["data"]["attributes"];
            // Known but never analysed (only its hash was ever seen): upload it after all.
            if let Some(report) = report(&attributes["last_analysis_stats"], &attributes["last_analysis_results"], sha256)
                && report.engines() > 0
            {
                return Ok(report);
            }
        }
        Err(Error::Status(404)) => {}
        Err(e) => return Err(e),
    }

    let size = tokio::fs::metadata(path).await.map_err(|_| Error::Io)?.len();
    if size == 0 || size > MAX_UPLOAD {
        return Err(Error::TooLarge);
    }
    stage(Stage::Uploading(0.0));
    let analysis = upload(client, key, path, size, &stage).await?;

    stage(Stage::Analyzing);
    let started = Instant::now();
    loop {
        tokio::time::sleep(POLL_EVERY).await;
        let json = get(client, key, &format!("{API}/analyses/{analysis}")).await?;
        let attributes = &json["data"]["attributes"];
        if attributes["status"] == "completed" {
            return report(&attributes["stats"], &attributes["results"], sha256).ok_or(Error::Unexpected);
        }
        if started.elapsed() > GIVE_UP_AFTER {
            return Err(Error::Timeout);
        }
    }
}

async fn get(client: &Client, key: &str, url: &str) -> Result<Value, Error> {
    send(|| async move { Ok(client.get(url).header("x-apikey", key).timeout(REQUEST_TIMEOUT)) }).await
}

/// Sends the request `make` builds, waiting out the per-minute quota (429) a few times.
async fn send<F, Fut>(make: F) -> Result<Value, Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<RequestBuilder, Error>>,
{
    for _ in 0..QUOTA_RETRIES {
        let res = make().await?.send().await.map_err(|_| Error::Network)?;
        match res.status() {
            s if s.is_success() => {
                let body = res.bytes().await.map_err(|_| Error::Network)?;
                return serde_json::from_slice(&body).map_err(|_| Error::Unexpected);
            }
            StatusCode::TOO_MANY_REQUESTS => tokio::time::sleep(QUOTA_WAIT).await,
            StatusCode::UNAUTHORIZED => return Err(Error::BadKey),
            StatusCode::PAYLOAD_TOO_LARGE => return Err(Error::TooLarge),
            s => return Err(Error::Status(s.as_u16())),
        }
    }
    Err(Error::Quota)
}

/// Uploads the file (streamed from disk, progress reported); returns the analysis id.
async fn upload(client: &Client, key: &str, path: &Path, size: u64, stage: &OnStage) -> Result<String, Error> {
    let url = if size <= DIRECT_UPLOAD_MAX {
        format!("{API}/files")
    } else {
        let json = get(client, key, &format!("{API}/files/upload_url")).await?;
        // The API key goes with the file: only to VirusTotal itself, over HTTPS.
        json["data"].as_str().filter(|u| is_virustotal(u)).ok_or(Error::Unexpected)?.to_owned()
    };
    let url = url.as_str();
    // Rebuilt on each attempt: a streamed body cannot be sent twice.
    let json = send(|| async move {
        let form = multipart::Form::new().part("file", file_part(path, size, stage.clone()).await?);
        Ok(client.post(url).header("x-apikey", key).multipart(form).timeout(UPLOAD_TIMEOUT))
    })
    .await?;
    json["data"]["id"].as_str().map(str::to_owned).ok_or(Error::Unexpected)
}

fn is_virustotal(url: &str) -> bool {
    url.parse::<url::Url>().is_ok_and(|u| {
        u.scheme() == "https" && u.host_str().is_some_and(|h| h == "virustotal.com" || h.ends_with(".virustotal.com"))
    })
}

/// The file as a streamed form part. Its name is not sent (VirusTotal shows submitted names to
/// everyone): only the extension, which some engines use.
async fn file_part(path: &Path, size: u64, stage: OnStage) -> Result<multipart::Part, Error> {
    let file = tokio::fs::File::open(path).await.map_err(|_| Error::Io)?;
    let name = match path.extension() {
        Some(ext) => format!("file.{}", ext.to_string_lossy()),
        None => "file".to_owned(),
    };
    let (mut sent, mut shown) = (0u64, 0u64);
    let stream = ReaderStream::with_capacity(file, 256 << 10).inspect(move |chunk| {
        if let Ok(chunk) = chunk {
            sent += chunk.len() as u64;
            let percent = sent * 100 / size.max(1);
            if percent != shown {
                shown = percent;
                stage(Stage::Uploading(sent as f32 / size as f32));
            }
        }
    });
    Ok(multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), size).file_name(name))
}

/// `stats` / `results` of a file's last analysis, or of an analysis object.
fn report(stats: &Value, results: &Value, sha256: &str) -> Option<Report> {
    if !stats.is_object() {
        return None;
    }
    let n = |key: &str| u32::try_from(stats[key].as_u64().unwrap_or(0)).unwrap_or(u32::MAX);
    let mut detections: Vec<Detection> = results
        .as_object()
        .map(|engines| {
            engines
                .iter()
                .filter_map(|(engine, r)| {
                    let category = r["category"].as_str()?;
                    let malicious = category == "malicious";
                    (malicious || category == "suspicious").then(|| Detection {
                        engine: r["engine_name"].as_str().unwrap_or(engine).to_owned(),
                        label: r["result"].as_str().unwrap_or(category).to_owned(),
                        malicious,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    detections.sort_by(|a, b| b.malicious.cmp(&a.malicious).then_with(|| a.engine.to_lowercase().cmp(&b.engine.to_lowercase())));
    Some(Report {
        malicious: n("malicious"),
        suspicious: n("suspicious"),
        harmless: n("harmless"),
        undetected: n("undetected"),
        unavailable: n("type-unsupported") + n("timeout") + n("confirmed-timeout") + n("failure"),
        detections,
        sha256: sha256.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "275a021bbfb6489e54d471899f7db9d1663fc695ec2fe2a2c4538aabf651fd0f";

    #[test]
    fn reads_a_known_files_last_analysis() {
        let json: Value = serde_json::from_str(
            r#"{"data":{"attributes":{
                "last_analysis_stats":{"harmless":0,"type-unsupported":8,"suspicious":1,"confirmed-timeout":0,
                                       "timeout":1,"failure":0,"malicious":2,"undetected":60},
                "last_analysis_results":{
                    "Zeta":{"category":"undetected","engine_name":"Zeta","result":null},
                    "beta":{"category":"suspicious","engine_name":"Beta","result":"Heur.Generic"},
                    "Alpha":{"category":"malicious","engine_name":"Alpha","result":"EICAR-Test-File"},
                    "Gamma":{"category":"malicious","engine_name":"Gamma","result":null}}}}}"#,
        )
        .unwrap();
        let attributes = &json["data"]["attributes"];
        let r = report(&attributes["last_analysis_stats"], &attributes["last_analysis_results"], SHA).unwrap();
        assert_eq!((r.malicious, r.suspicious, r.undetected, r.unavailable), (2, 1, 60, 9));
        assert_eq!((r.engines(), r.flagged()), (63, 3));
        let names: Vec<_> = r.detections.iter().map(|d| (d.engine.as_str(), d.label.as_str(), d.malicious)).collect();
        assert_eq!(names, [("Alpha", "EICAR-Test-File", true), ("Gamma", "malicious", true), ("Beta", "Heur.Generic", false)]);
        assert!(r.link().ends_with(SHA));
    }

    #[test]
    fn reads_a_completed_analysis() {
        let json: Value = serde_json::from_str(
            r#"{"data":{"attributes":{"status":"completed",
                "stats":{"malicious":0,"suspicious":0,"undetected":70,"harmless":0},
                "results":{"A":{"category":"undetected","engine_name":"A","result":null}}}}}"#,
        )
        .unwrap();
        let a = &json["data"]["attributes"];
        let r = report(&a["stats"], &a["results"], SHA).unwrap();
        assert_eq!((r.engines(), r.flagged()), (70, 0));
        assert!(r.detections.is_empty());
    }

    #[test]
    fn the_key_only_goes_to_virustotal() {
        assert!(is_virustotal("https://www.virustotal.com/_ah/upload/AMmfu6b/"));
        for elsewhere in ["http://www.virustotal.com/_ah/upload/x", "https://virustotal.com.evil.io/x", "https://evil.io/www.virustotal.com", "nonsense"] {
            assert!(!is_virustotal(elsewhere), "{elsewhere}");
        }
        let u = |s: &str| s.parse::<url::Url>().unwrap();
        assert!(redirect_keeps_key(&u("https://www.virustotal.com/api/v3/x"), &u("https://www.virustotal.com/api/v3/y")));
        assert!(!redirect_keeps_key(&u("https://www.virustotal.com/api/v3/x"), &u("https://evil.io/")), "the key would follow");
        assert!(!redirect_keeps_key(&u("https://www.virustotal.com/x"), &u("http://www.virustotal.com/x")), "not in clear");
        let github = u("https://github.com/o/r/releases/download/v1/a.msi");
        assert!(redirect_keeps_key(&github, &u("https://release-assets.githubusercontent.com/a")), "GitHub's downloads still work");
    }

    #[test]
    fn a_file_without_analysis_has_no_report() {
        assert!(report(&Value::Null, &Value::Null, SHA).is_none());
    }

    #[test]
    fn errors_read_as_sentences() {
        let _one_at_a_time = crate::i18n::TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::i18n::set(crate::i18n::Language::French);
        assert!(Error::TooLarge.to_string().contains("650 Mo"));
        assert_eq!(Error::Status(503).to_string(), "VirusTotal a répondu 503");
        crate::i18n::set(crate::i18n::Language::English);
        assert_eq!(Error::Status(503).to_string(), "VirusTotal answered 503");
    }
}
