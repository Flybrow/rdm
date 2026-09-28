//! Separate video + audio tracks (YouTube, HLS alternate audio): fetched in parallel, then remuxed.

use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering::Relaxed},
    time::Duration,
};

use reqwest::Client;
use tokio::fs;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{EngineError, Job, Outcome, Progress, mux, net, transfer, with_suffix};

/// Suffixes of every temporary file a split download may leave next to its target.
pub const PART_SUFFIXES: [&str; 7] = [
    ".video.part",
    ".video.part.rdm",
    ".video.part.ok",
    ".audio.part",
    ".audio.part.rdm",
    ".audio.part.ok",
    mux::TMP_SUFFIX,
];

pub(crate) async fn run(
    client: &Client,
    job: &Job,
    audio: &Url,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    let split = Split::new(&job.target, &cancel);
    let video_job = Job { target: split.video.clone(), audio: None, ..job.clone() };
    let audio_job = Job {
        url: audio.clone(),
        target: split.audio.clone(),
        audio: None,
        connections: (job.connections / 4).max(1),
        // The credentials given for the video stay on its site.
        headers: net::headers_for(&job.headers, &job.url, audio).into_owned(),
        ..job.clone()
    };
    let v = transfer::run(client, &video_job, split.progress[0].clone(), split.stop.clone());
    let a = transfer::run(client, &audio_job, split.progress[1].clone(), split.stop.clone());
    split.finish(&progress, &job.target, v, a).await
}

/// Two part files next to `target`, their own progress, and a shared stop token.
pub(crate) struct Split {
    pub video: PathBuf,
    pub audio: PathBuf,
    pub progress: [Arc<Progress>; 2],
    pub stop: CancellationToken,
}

impl Split {
    pub fn new(target: &Path, cancel: &CancellationToken) -> Self {
        Self {
            video: with_suffix(target, ".video.part"),
            audio: with_suffix(target, ".audio.part"),
            progress: Default::default(),
            stop: cancel.child_token(),
        }
    }

    /// Drives both part downloads (progress summed into `progress`), then muxes them into `target`.
    /// A part already completed by a previous attempt (`.ok` marker) is not fetched again.
    pub async fn finish<V, A>(&self, progress: &Progress, target: &Path, video: V, audio: A) -> Result<Outcome, EngineError>
    where
        V: Future<Output = Result<Outcome, EngineError>>,
        A: Future<Output = Result<Outcome, EngineError>>,
    {
        let both = async {
            tokio::join!(
                self.guard(&self.video, &self.progress[0], video),
                self.guard(&self.audio, &self.progress[1], audio)
            )
        };
        tokio::pin!(both);
        let (video, audio) = loop {
            tokio::select! {
                r = &mut both => break r,
                () = tokio::time::sleep(Duration::from_millis(250)) => self.sum_into(progress),
            }
        };
        self.sum_into(progress);

        if (video?, audio?) != (Outcome::Completed, Outcome::Completed) {
            return Ok(Outcome::Paused);
        }
        let (v, a, out) = (self.video.clone(), self.audio.clone(), target.to_path_buf());
        tokio::task::spawn_blocking(move || mux::merge(&v, &a, &out))
            .await
            .map_err(|e| EngineError::Io(e.into()))??;
        for suffix in PART_SUFFIXES {
            let _ = fs::remove_file(with_suffix(target, suffix)).await;
        }
        Ok(Outcome::Completed)
    }

    /// Skips a part finished earlier; stops the sibling on failure; marks the part once complete.
    async fn guard(
        &self,
        part: &Path,
        progress: &Progress,
        fetch: impl Future<Output = Result<Outcome, EngineError>>,
    ) -> Result<Outcome, EngineError> {
        let marker = with_suffix(part, ".ok");
        if let (Ok(_), Ok(meta)) = (fs::metadata(&marker).await, fs::metadata(part).await) {
            progress.downloaded.store(meta.len(), Relaxed);
            progress.total.store(meta.len(), Relaxed);
            return Ok(Outcome::Completed);
        }
        let result = fetch.await;
        match &result {
            Ok(Outcome::Completed) => fs::write(&marker, b"").await?,
            Ok(Outcome::Paused) => {}
            Err(_) => self.stop.cancel(),
        }
        result
    }

    fn sum_into(&self, progress: &Progress) {
        let sum = |f: fn(&Progress) -> u64| self.progress.iter().map(|p| f(p)).sum::<u64>();
        progress.downloaded.store(sum(|p| p.downloaded.load(Relaxed)), Relaxed);
        progress.total.store(sum(|p| p.total.load(Relaxed)), Relaxed);
        progress.active.store(self.progress.iter().map(|p| p.active.load(Relaxed)).sum(), Relaxed);
    }
}
