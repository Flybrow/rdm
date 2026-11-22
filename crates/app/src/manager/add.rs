//! Adding downloads: from the browser (maybe confirmed first, in a window of its own: see
//! `windows`), a pasted or copied link; the real file name asked from the server in the background.

use super::*;

/// A new link shows in the list at once; asking its server for the real file name takes at most this.
pub(super) const NAME_TIMEOUT: Duration = Duration::from_secs(10);
/// The same link sent again within this window is the same request (double click, page retrying).
const DUPLICATE_WINDOW: Duration = Duration::from_secs(5);
/// Downloads waiting for the user's answer about an existing file at most (the rest get a new name).
const MAX_TO_CONFIRM: usize = 50;

/// What the browser (or the UI) hands us.
#[derive(Debug, Deserialize)]
pub struct AddRequest {
    pub url: Url,
    pub audio_url: Option<Url>,
    pub filename: Option<String>,
    pub referrer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    /// The user's answer for this download when its file already exists (else the settings').
    #[serde(skip)]
    pub existing: Option<ExistingFile>,
    /// What the user chose for this download only, in its window (folder, connections…).
    #[serde(skip)]
    pub options: Option<AddOptions>,
    /// Waiting for the user's answer: a file of its name was already there when it was queued
    /// (looked up once, not at every frame of the window).
    #[serde(skip)]
    exists: bool,
}

/// Settings of one download, chosen in its window; the others follow the settings.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Its folder, instead of its category's.
    pub dir: Option<PathBuf>,
    pub connections: Option<u8>,
    /// Its own speed cap in KiB/s (0 = none).
    pub speed_limit_kib: u32,
    pub queue: u32,
    pub checksum: Option<domain::Checksum>,
    /// Added paused, started when the user says so ("Download later").
    pub paused: bool,
}

impl AddRequest {
    pub fn from_url(url: Url) -> Self {
        Self {
            url,
            audio_url: None,
            filename: None,
            referrer: None,
            cookies: None,
            user_agent: None,
            existing: None,
            options: None,
            exists: false,
        }
    }

    /// The file name the page gave, made safe.
    pub(super) fn given_name(&self) -> Option<String> {
        self.filename.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(engine::sanitize_file_name)
    }

    /// The request behind a download of the list, from its link and headers.
    fn from_parts(url: Url, audio_url: Option<Url>, headers: &[(String, String)]) -> Self {
        let get = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        Self { audio_url, referrer: get("referer"), cookies: get("cookie"), user_agent: get("user-agent"), ..Self::from_url(url) }
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        [("referer", &self.referrer), ("cookie", &self.cookies), ("user-agent", &self.user_agent)]
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_owned(), v.clone().filter(|v| !v.is_empty())?)))
            .collect()
    }
}

/// A pasted or copied link whose file is already there, waiting in the main window for the user's
/// answer (`ExistingFile::Ask`).
pub struct ToConfirm {
    pub url: Url,
    /// Its name, when known.
    pub name: Option<String>,
    /// A file of that name is already there, and the user wants to be asked (`ExistingFile::Ask`).
    pub ask_existing: bool,
    pub waiting: usize,
}

impl Manager {
    /// A download from the browser extension: added at once, or first shown in a window of its own
    /// for the user's go-ahead, as the settings say (see `windows`). `false` when there is no room
    /// left to ask (a page flooding the extension): the browser keeps that download.
    pub fn add_from_browser(self: &Arc<Self>, req: AddRequest) -> bool {
        // A copy: the disk is looked at below, never under the settings' lock.
        let settings = self.settings();
        let ask = settings.existing == ExistingFile::Ask;
        let exists = req.given_name().is_some_and(|n| settings.target_dir(&n).join(&n).is_file());
        // Without confirmation, still asked when the file is already there (`ExistingFile::Ask`).
        if settings.confirm_browser || (ask && exists) {
            return self.ask_in_window(req).is_ok();
        }
        if let Some(id) = self.add_new(req)
            && settings.progress_window
        {
            self.open_window(DownloadWindow::Track(id));
        }
        true
    }

    /// Shown in the (raised) window until the user answers (see `to_confirm`). `Err`: no room left
    /// to ask (`MAX_TO_CONFIRM`), the request comes back. `req.exists` is filled in by the caller.
    fn wait_for_answer(&self, req: AddRequest) -> Result<(), Box<AddRequest>> {
        {
            let mut waiting = lock(&self.to_confirm);
            if waiting.iter().any(|w| w.url == req.url) {
                return Ok(()); // the very same link, just sent twice
            }
            if waiting.len() >= MAX_TO_CONFIRM {
                return Err(Box::new(req));
            }
            waiting.push_back(req);
        }
        self.show();
        self.repaint();
        Ok(())
    }

    /// The first download waiting for the user's answer (read at every frame: no disk access).
    pub fn to_confirm(&self) -> Option<ToConfirm> {
        let waiting = lock(&self.to_confirm);
        let req = waiting.front()?;
        let ask = self.with_settings(|s| s.existing == ExistingFile::Ask);
        Some(ToConfirm { url: req.url.clone(), name: req.given_name(), ask_existing: ask && req.exists, waiting: waiting.len() })
    }

    /// The user answered for the first waiting download: `download` it or not, with `existing`
    /// as the answer for a file already there.
    pub fn answer_confirm(self: &Arc<Self>, download: bool, existing: Option<ExistingFile>) {
        let Some(mut req) = lock(&self.to_confirm).pop_front() else { return };
        if download {
            req.existing = existing;
            self.add(req);
        }
        self.repaint();
    }

    /// Shows the download in the list at once; without a name from the page, the server is asked
    /// for the real one in the background (bounded), and the download starts right after.
    pub fn add(self: &Arc<Self>, req: AddRequest) {
        self.add_new(req);
    }

    /// `add`, telling which download of the list it became (`None`: the same link added a moment
    /// ago, or a file to skip).
    pub(super) fn add_new(self: &Arc<Self>, req: AddRequest) -> Option<DownloadId> {
        let headers = req.headers();
        let given = req.given_name();
        let provisional = given.clone().unwrap_or_else(|| engine::suggest_file_name(&req.url, None));
        let id = self.insert(&req, &provisional, headers.clone(), given.is_none())?;
        if given.is_some() {
            self.schedule();
            return Some(id);
        }
        let this = self.clone();
        self.rt.spawn(async move {
            let name = tokio::time::timeout(NAME_TIMEOUT, this.suggest_name(&req.url, &to_header_map(&headers))).await.ok().flatten();
            let _ = tokio::task::spawn_blocking(move || {
                this.resolved(id, name);
                this.schedule();
            })
            .await;
        });
        Some(id)
    }

    /// Name from the server; HLS gets the extension of what will actually be written.
    async fn suggest_name(&self, url: &Url, headers: &HeaderMap) -> Option<String> {
        self.inspect(url, headers).await.map(|i| i.name)
    }

    /// What the server says about a link: the file's name (HLS: with the extension of what will
    /// actually be written), its size, whether it resumes.
    pub(super) async fn inspect(&self, url: &Url, headers: &HeaderMap) -> Option<Inspected> {
        let client = self.client(url).await.ok()?;
        // The site's saved login too: without it a protected server only answers 401.
        let mut headers = headers.clone();
        self.add_login(url, &mut headers);
        let headers = &headers;
        let probe = engine::probe_once(&client, url, headers).await.ok()?;
        if !probe.hls {
            return Some(Inspected { name: probe.file_name, size: probe.size, resumable: Some(probe.ranges) });
        }
        let fmp4 = engine::hls_info(&client, url, headers).await.is_ok_and(|i| i.fmp4);
        let stem = probe.file_name.rsplit_once('.').map_or(probe.file_name.as_str(), |(s, _)| s);
        Some(Inspected { name: format!("{stem}.{}", if fmp4 { "mp4" } else { "ts" }), size: None, resumable: None })
    }

    /// The server's name for a new download (if it gave one): its target follows, unless it
    /// already started meanwhile. Either way it may start now — or, when a file of that name
    /// exists and the settings say to skip it, it goes away.
    fn resolved(self: &Arc<Self>, id: DownloadId, name: Option<String>) {
        let settings = self.settings();
        // The disk is looked at with only the naming lock held (see `naming`); the list is read,
        // then written, in short steps around it.
        let naming = lock(&self.naming);
        let Some((current, status, mut taken)) =
            self.view(|es| es.iter().find(|e| e.download.id == id).map(|e| (e.name.clone(), e.download.status().clone(), planned(es))))
        else {
            return;
        };
        taken.extend(naming.iter().cloned());
        // Its name known at last (a pasted or copied link): a file of that name already there is
        // the user's to decide, before anything is written (`ExistingFile::Ask`).
        let final_name = name.clone().unwrap_or_else(|| current.clone());
        if settings.existing == ExistingFile::Ask && status == Status::Queued && settings.target_dir(&final_name).join(&final_name).is_file() {
            let removed = self.take_entry(id, |s| *s == Status::Queued);
            drop(naming); // `add` below chooses a name too
            let Some(e) = removed else { return };
            let mut req = AddRequest::from_parts(e.download.url.clone(), e.download.audio.clone(), &e.headers);
            req.filename = Some(final_name);
            req.exists = true;
            // Too many questions already: not lost, downloaded under a new name.
            if let Err(req) = self.wait_for_answer(req) {
                self.add(AddRequest { existing: Some(ExistingFile::Rename), ..*req });
            }
            self.changed();
            return;
        }
        let renamable = |s: &Status| matches!(s, Status::Queued | Status::Paused);
        let mut target = None;
        if let Some(name) = name.filter(|n| *n != current)
            && renamable(&status)
        {
            match target_for(&settings, &name, &taken, Some(id)) {
                Some(path) => {
                    let replaces = path.is_file();
                    target = Some((path, replaces));
                }
                None => {
                    self.take_entry(id, renamable);
                    drop(naming);
                    self.notice(false, &trf!("Déjà téléchargé, ignoré : {name}", "Already downloaded, skipped: {name}", name = name));
                    self.changed();
                    return;
                }
            }
        }
        let mut entries = lock(&self.entries);
        let Some(e) = entries.iter_mut().find(|e| e.download.id == id) else { return };
        if let Some((path, replaces)) = target
            && renamable(e.download.status())
        {
            e.replaces = replaces;
            e.download.target = path;
            e.named();
        }
        e.resolving = false;
        drop(entries);
        self.changed();
    }

    /// Removes download `id` from the list if its status still allows it.
    fn take_entry(&self, id: DownloadId, allowed: impl Fn(&Status) -> bool) -> Option<Entry> {
        let mut entries = lock(&self.entries);
        let i = entries.iter().position(|e| e.download.id == id && allowed(e.download.status()))?;
        Some(entries.remove(i))
    }

    /// `None` when the same link was added a moment ago (a double click, a page asking twice).
    fn insert(&self, req: &AddRequest, name: &str, headers: Vec<(String, String)>, resolving: bool) -> Option<DownloadId> {
        let (url, audio, existing, options) = (req.url.clone(), req.audio_url.clone(), req.existing, req.options.as_ref());
        let mut settings = self.settings();
        if let Some(existing) = existing {
            settings.existing = existing;
        }
        // A folder chosen for this download: its category's folder, for this download only.
        if let Some(dir) = options.and_then(|o| o.dir.clone()) {
            settings.category_dirs.insert(Category::of(name), dir);
        }
        // One name chosen at a time, the disk looked at outside the list's lock (see `naming`).
        let naming = lock(&self.naming);
        let mut taken = {
            let entries = lock(&self.entries);
            let twice = entries.iter().rev().take_while(|e| e.added.is_some_and(|t| t.elapsed() < DUPLICATE_WINDOW)).any(|e| {
                e.download.url == url && e.download.audio == audio && !matches!(e.download.status(), Status::Failed(_))
            });
            if twice {
                return None;
            }
            planned(&entries)
        };
        taken.extend(naming.iter().cloned());
        // A name already known (the page gave it): an existing file may mean "skip".
        let target = if resolving {
            unique_path(&settings.target_dir(name), name, &taken, None)
        } else {
            let Some(target) = target_for(&settings, name, &taken, None) else {
                drop(naming);
                self.notice(false, &trf!("Déjà téléchargé, ignoré : {name}", "Already downloaded, skipped: {name}", name = name));
                return None;
            };
            target
        };
        // Only "overwrite" keeps the name of an existing file.
        let replaces = target.is_file();
        let connections = options.and_then(|o| o.connections).unwrap_or(settings.connections);
        let mut download = Download::new(url, target, connections);
        download.audio = audio;
        if let Some(o) = options {
            download.queue = o.queue;
            download.speed_limit_kib = o.speed_limit_kib;
            download.checksum = o.checksum.clone();
            if o.paused {
                let _ = download.pause();
            }
        }
        let id = download.id;
        let mut entry = Entry::new(download, headers, 0, 0);
        entry.resolving = resolving;
        entry.replaces = replaces;
        entry.added = Some(Instant::now());
        lock(&self.entries).push(entry);
        drop(naming);
        self.changed();
        Some(id)
    }
}

/// The headers `req` asks for (cookies, referrer, user agent), for requests about its link.
pub fn header_map(req: &AddRequest) -> HeaderMap {
    to_header_map(&req.headers())
}
