//! Browser recordings: a running entry fed chunk by chunk by the extension.

use super::*;

/// Track of a browser recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Track {
    Video,
    Audio,
}

impl Track {
    const fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// A recording fed by the browser's own player (extension/capture.js): the bytes arrive per
/// media source (the page may create several: ads, quality restarts) and per track.
pub(super) struct Recording {
    pub(super) id: DownloadId,
    pub(super) target: PathBuf,
    pub(super) parts: HashMap<(u32, Track), u64>,
    pub(super) last_data: Instant,
}

/// Media sources one recording may create (the programme, ads, quality restarts): far more than a
/// player needs.
const MAX_RECORDING_SOURCES: usize = 64;

impl Recording {
    pub(super) fn part(&self, ms: u32, track: Track) -> PathBuf {
        with_suffix(&self.target, &format!(".rec{ms}.{}", track.name()))
    }

    /// The main programme: the media source with both tracks and the most data (not an ad).
    fn best_source(&self) -> Option<u32> {
        let sources: HashSet<u32> = self.parts.keys().map(|(ms, _)| *ms).collect();
        sources
            .into_iter()
            .filter(|ms| self.parts.contains_key(&(*ms, Track::Video)) && self.parts.contains_key(&(*ms, Track::Audio)))
            .max_by_key(|ms| self.parts[&(*ms, Track::Video)] + self.parts[&(*ms, Track::Audio)])
    }
}

impl Manager {
    /// Opens a recording: a running entry the browser will feed. Returns its secret token.
    pub fn start_recording(&self, page: Url, name: &str) -> String {
        let name = engine::sanitize_file_name(name);
        let settings = self.settings();
        let mut entries = lock(&self.entries);
        let target = unique_path(&settings.target_dir(&name), &name, &entries, None);
        let download = Download::recording(page, target.clone());
        let id = download.id;
        let mut entry = Entry::new(download, Vec::new(), 0, 0);
        entry.added = Some(Instant::now());
        entries.push(entry);
        drop(entries);
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let recording = Recording { id, target, parts: HashMap::new(), last_data: Instant::now() };
        lock(&self.recordings).insert(token.clone(), recording);
        self.changed();
        token
    }

    /// Appends a chunk of one track; `false` if the token is unknown (finished, cancelled, forged).
    pub async fn record_append(&self, token: &str, ms: u32, track: Track, data: &[u8]) -> std::io::Result<bool> {
        let (path, id) = {
            let mut recordings = lock(&self.recordings);
            let Some(r) = recordings.get_mut(token) else { return Ok(false) };
            // A page creating media sources without end (a file each) is not a player: refused.
            let new_source = !r.parts.keys().any(|(m, _)| *m == ms);
            if new_source && r.parts.keys().map(|(m, _)| *m).collect::<HashSet<_>>().len() >= MAX_RECORDING_SOURCES {
                return Ok(false);
            }
            r.last_data = Instant::now();
            *r.parts.entry((ms, track)).or_default() += data.len() as u64;
            (r.part(ms, track), r.id)
        };
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, data).await?;
        self.view(|es| {
            if let Some(e) = es.iter().find(|e| e.download.id == id) {
                e.progress.downloaded.fetch_add(data.len() as u64, Relaxed);
            }
        });
        Ok(true)
    }

    /// Playback position (0..1): turns the bytes received so far into an estimated total.
    pub fn record_progress(&self, token: &str, fraction: f64) -> bool {
        let Some(id) = lock(&self.recordings).get(token).map(|r| r.id) else { return false };
        self.view(|es| {
            if let Some(e) = es.iter().find(|e| e.download.id == id) {
                let done = e.progress.downloaded.load(Relaxed);
                if fraction > 0.01 {
                    e.progress.total.store((done as f64 / fraction.min(1.0)) as u64, Relaxed);
                }
            }
        });
        true
    }

    /// End of playback: mux the main programme's tracks into the target, then clean up.
    pub fn finish_recording(self: &Arc<Self>, token: &str) -> bool {
        let Some(r) = lock(&self.recordings).remove(token) else { return false };
        let this = self.clone();
        self.inflight.fetch_add(1, AcqRel); // quitting waits (bounded) for the merge
        lock(&self.busy).insert(r.id); // so does removing it: its files are being written
        self.rt.spawn_blocking(move || {
            let result = match r.best_source() {
                Some(ms) => engine::mux::merge(&r.part(ms, Track::Video), &r.part(ms, Track::Audio), &r.target)
                    .map_err(|e| trf!("fusion audio/vidéo impossible : {e}", "cannot merge audio and video: {e}", e = e)),
                None => Err(tr!("aucune donnée vidéo et audio reçue", "no video or audio data received").to_owned()),
            };
            for (ms, track) in r.parts.keys() {
                let _ = fs::remove_file(r.part(*ms, *track));
            }
            let mut finished = None;
            this.update(r.id, |e| {
                let _ = match result {
                    Ok(()) => {
                        mark_from_internet(&r.target);
                        let size = fs::metadata(&r.target).map_or(0, |m| m.len());
                        e.progress.downloaded.store(size, Relaxed);
                        e.progress.total.store(size, Relaxed);
                        finished = r.target.file_name().map(|n| n.to_string_lossy().into_owned());
                        e.download.complete()
                    }
                    Err(reason) => e.download.fail(reason),
                };
            });
            lock(&this.busy).remove(&r.id);
            this.inflight.fetch_sub(1, AcqRel);
            if let Some(name) = finished
                && this.with_settings(|s| s.notify)
            {
                notify::completed(&name);
            }
        });
        true
    }

    /// Stops a recording (cancelled in the page, idle, or removed): drops its partial data.
    pub fn cancel_recording(&self, token: &str, reason: &str) -> bool {
        let Some(r) = lock(&self.recordings).remove(token) else { return false };
        for (ms, track) in r.parts.keys() {
            let _ = fs::remove_file(r.part(*ms, *track));
        }
        self.update(r.id, |e| {
            let _ = e.download.fail(reason);
        });
        true
    }

    pub(super) fn token_of(&self, id: DownloadId) -> Option<String> {
        lock(&self.recordings).iter().find(|(_, r)| r.id == id).map(|(t, _)| t.clone())
    }

    /// Recordings whose tab went silent (closed, navigated away) fail instead of hanging forever.
    pub(super) fn reap_idle_recordings(&self) {
        let idle: Vec<String> = lock(&self.recordings)
            .iter()
            .filter(|(_, r)| r.last_data.elapsed() > RECORDING_IDLE)
            .map(|(t, _)| t.clone())
            .collect();
        for token in idle {
            self.cancel_recording(&token, tr!("enregistrement interrompu (onglet fermé ou lecture arrêtée)", "recording interrupted (tab closed or playback stopped)"));
        }
    }
}
