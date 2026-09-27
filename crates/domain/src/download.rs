use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DownloadId(Uuid);

impl DownloadId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DownloadId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Queued,
    Running,
    Paused,
    Completed,
    Failed(String),
}

/// How the bytes are obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Source {
    /// Fetched by the engine from `url` (resumable).
    #[default]
    Http,
    /// Streamed in by the browser while its player plays `url` (cannot be resumed by RDM).
    Recording,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DownloadError {
    #[error("invalid transition from {from:?} to {to}")]
    InvalidTransition { from: Status, to: &'static str },
}

/// Aggregate root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    pub id: DownloadId,
    pub url: Url,
    pub target: PathBuf,
    pub connections: u8,
    /// Audio stream muxed into `target` together with the video-only `url`.
    #[serde(default)]
    pub audio: Option<Url>,
    #[serde(default)]
    pub source: Source,
    /// Named queue it waits in (0 = the main queue).
    #[serde(default)]
    pub queue: u32,
    /// Its own speed cap in KiB/s, on top of the global one (0 = none).
    #[serde(default)]
    pub speed_limit_kib: u32,
    /// Expected checksum (`md5:…`, `sha1:…`, `sha256:…`, `sha512:…`), verified once complete.
    #[serde(default)]
    pub checksum: Option<String>,
    /// Accept an invalid TLS certificate for this download only (the user's explicit choice).
    #[serde(default)]
    pub insecure: bool,
    status: Status,
}

impl Download {
    pub fn new(url: Url, target: PathBuf, connections: u8) -> Self {
        Self {
            id: DownloadId::new(),
            url,
            target,
            connections: connections.clamp(1, crate::MAX_CONNECTIONS),
            audio: None,
            source: Source::Http,
            queue: 0,
            speed_limit_kib: 0,
            checksum: None,
            insecure: false,
            status: Status::Queued,
        }
    }

    /// A recording in progress: running from the start, fed by the browser.
    pub fn recording(page: Url, target: PathBuf) -> Self {
        Self { source: Source::Recording, status: Status::Running, ..Self::new(page, target, 1) }
    }

    pub fn is_recording(&self) -> bool {
        self.source == Source::Recording
    }

    pub const fn status(&self) -> &Status {
        &self.status
    }

    pub fn start(&mut self) -> Result<(), DownloadError> {
        self.transition(
            matches!(self.status, Status::Queued | Status::Paused | Status::Failed(_)),
            Status::Running,
            "Running",
        )
    }

    /// Back into the queue: the scheduler starts it when a slot frees up. Recordings cannot be
    /// re-queued: only the browser can feed them.
    pub fn enqueue(&mut self) -> Result<(), DownloadError> {
        let allowed = !self.is_recording() && matches!(self.status, Status::Paused | Status::Failed(_));
        self.transition(allowed, Status::Queued, "Queued")
    }

    /// Running → back in the queue after a transient failure (network down, busy server): the
    /// scheduler retries it later. Recordings cannot: only the browser can feed them.
    pub fn retry_later(&mut self) -> Result<(), DownloadError> {
        let allowed = !self.is_recording() && self.status == Status::Running;
        self.transition(allowed, Status::Queued, "Queued")
    }

    pub fn category(&self) -> crate::Category {
        crate::Category::of(&self.target.to_string_lossy())
    }

    pub fn pause(&mut self) -> Result<(), DownloadError> {
        self.transition(matches!(self.status, Status::Running | Status::Queued), Status::Paused, "Paused")
    }

    pub fn complete(&mut self) -> Result<(), DownloadError> {
        self.transition(self.status == Status::Running, Status::Completed, "Completed")
    }

    pub fn fail(&mut self, reason: impl Into<String>) -> Result<(), DownloadError> {
        let next = Status::Failed(reason.into());
        self.transition(self.status == Status::Running, next, "Failed")
    }

    fn transition(&mut self, allowed: bool, next: Status, to: &'static str) -> Result<(), DownloadError> {
        if !allowed {
            return Err(DownloadError::InvalidTransition { from: self.status.clone(), to });
        }
        self.status = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Download {
        Download::new("https://x.io/a.zip".parse().unwrap(), "a.zip".into(), 32)
    }

    #[test]
    fn lifecycle() {
        let mut d = sample();
        d.start().unwrap();
        d.pause().unwrap();
        d.start().unwrap();
        d.complete().unwrap();
        assert!(d.start().is_err());
        assert!(d.enqueue().is_err());
    }

    #[test]
    fn recordings_run_at_once_and_cannot_be_requeued() {
        let mut r = Download::recording("https://www.youtube.com/watch?v=x".parse().unwrap(), "v.mp4".into());
        assert!(r.is_recording());
        assert_eq!(r.status(), &Status::Running);
        r.fail("interrompu").unwrap();
        assert!(r.enqueue().is_err());
    }

    #[test]
    fn older_saved_downloads_still_load() {
        let json = r#"{"id":"67e55044-10b1-426f-9247-bb680e5fe0c8","url":"https://x.io/a.zip","target":"a.zip","size":null,"connections":8,"status":"Paused"}"#;
        let d: Download = serde_json::from_str(json).unwrap();
        assert_eq!(d.status(), &Status::Paused);
        assert!(d.audio.is_none());
    }

    #[test]
    fn transient_failures_go_back_to_the_queue() {
        let mut d = sample();
        assert!(d.retry_later().is_err(), "only a running download");
        d.start().unwrap();
        d.retry_later().unwrap();
        assert_eq!(d.status(), &Status::Queued);
        let mut r = Download::recording("https://www.youtube.com/watch?v=x".parse().unwrap(), "v.mp4".into());
        assert!(r.retry_later().is_err());
    }

    #[test]
    fn failed_can_be_requeued() {
        let mut d = sample();
        d.start().unwrap();
        d.fail("boom").unwrap();
        d.enqueue().unwrap();
        assert_eq!(d.status(), &Status::Queued);
    }
}
