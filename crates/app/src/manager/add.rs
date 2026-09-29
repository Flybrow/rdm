//! Adding downloads: from the browser (maybe confirmed first), a pasted or copied link; the
//! real file name asked from the server in the background.

use super::*;

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
    /// Shown to the user only because its file already exists (not a browser download to confirm).
    #[serde(skip)]
    pub existing_only: bool,
}

impl AddRequest {
    pub fn from_url(url: Url) -> Self {
        Self { url, audio_url: None, filename: None, referrer: None, cookies: None, user_agent: None, existing: None, existing_only: false }
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

/// A download waiting for the user's answer, as the window shows it.
pub struct ToConfirm {
    pub url: Url,
    /// Its name, when known.
    pub name: Option<String>,
    /// A file of that name is already there, and the user wants to be asked (`ExistingFile::Ask`).
    pub ask_existing: bool,
    /// Every browser download is confirmed (`confirm_browser`); otherwise only the file is asked about.
    pub confirming: bool,
    pub waiting: usize,
}

impl Manager {
    /// A download from the browser extension: added at once, or first shown in the (raised)
    /// window for the user's go-ahead, as the settings say.
    pub fn add_from_browser(self: &Arc<Self>, req: AddRequest) {
        let (confirm, ask) = self.with_settings(|s| (s.confirm_browser, s.existing == ExistingFile::Ask));
        let exists = req
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(engine::sanitize_file_name)
            .is_some_and(|n| self.with_settings(|s| s.target_dir(&n)).join(&n).is_file());
        // Without confirmation, still asked when the file is already there (`ExistingFile::Ask`).
        if confirm || (ask && exists) {
            // No room left to ask: a page flooding the extension, the user has enough to answer.
            let _ = self.wait_for_answer(AddRequest { existing_only: !confirm, ..req });
        } else {
            self.add(req);
        }
    }

    /// Shown in the (raised) window until the user answers (see `to_confirm`). `Err`: no room left
    /// to ask (`MAX_TO_CONFIRM`), the request comes back.
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

    /// The first download waiting for the user's answer.
    pub fn to_confirm(&self) -> Option<ToConfirm> {
        let waiting = lock(&self.to_confirm);
        let req = waiting.front()?;
        let name = req.filename.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(engine::sanitize_file_name);
        let (confirming, ask) = self.with_settings(|s| (s.confirm_browser, s.existing == ExistingFile::Ask));
        let exists = name.as_deref().is_some_and(|n| self.with_settings(|s| s.target_dir(n)).join(n).is_file());
        Some(ToConfirm { url: req.url.clone(), name, ask_existing: ask && exists, confirming: confirming && !req.existing_only, waiting: waiting.len() })
    }

    /// The user answered for the first waiting download: `download` it or not, with `existing`
    /// as the answer for a file already there; `always`: stop asking (the ones still waiting then
    /// start too).
    pub fn answer_confirm(self: &Arc<Self>, download: bool, existing: Option<ExistingFile>, always: bool) {
        let Some(mut req) = lock(&self.to_confirm).pop_front() else { return };
        if download {
            req.existing = existing;
            self.add(req);
        }
        if always {
            let mut settings = self.settings();
            settings.confirm_browser = false;
            self.apply_settings(settings);
            self.save_settings();
            let rest: Vec<_> = lock(&self.to_confirm).drain(..).collect();
            rest.into_iter().for_each(|r| self.add(r));
        }
        self.repaint();
    }

    /// Shows the download in the list at once; without a name from the page, the server is asked
    /// for the real one in the background (bounded), and the download starts right after.
    pub fn add(self: &Arc<Self>, req: AddRequest) {
        let headers = req.headers();
        let given = req.filename.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(engine::sanitize_file_name);
        let provisional = given.clone().unwrap_or_else(|| engine::suggest_file_name(&req.url, None));
        let Some(id) = self.insert(req.url.clone(), req.audio_url, &provisional, headers.clone(), given.is_none(), req.existing) else {
            return; // the very same link, just sent twice
        };
        if given.is_some() {
            self.schedule();
            return;
        }
        let this = self.clone();
        self.rt.spawn(async move {
            let name = tokio::time::timeout(NAME_TIMEOUT, this.suggest_name(&req.url, &to_header_map(&headers))).await.ok().flatten();
            this.resolved(id, name);
            this.schedule();
        });
    }

    /// Name from the server; HLS gets the extension of what will actually be written.
    async fn suggest_name(&self, url: &Url, headers: &HeaderMap) -> Option<String> {
        let client = self.client(url).await;
        // The site's saved login too: without it a protected server only answers 401.
        let mut headers = headers.clone();
        self.add_login(url, &mut headers);
        let headers = &headers;
        let probe = engine::probe_once(&client, url, headers).await.ok()?;
        if !probe.hls {
            return Some(probe.file_name);
        }
        let fmp4 = engine::hls_info(&client, url, headers).await.is_ok_and(|i| i.fmp4);
        let stem = probe.file_name.rsplit_once('.').map_or(probe.file_name.as_str(), |(s, _)| s);
        Some(format!("{stem}.{}", if fmp4 { "mp4" } else { "ts" }))
    }

    /// The server's name for a new download (if it gave one): its target follows, unless it
    /// already started meanwhile. Either way it may start now — or, when a file of that name
    /// exists and the settings say to skip it, it goes away.
    fn resolved(self: &Arc<Self>, id: DownloadId, name: Option<String>) {
        let settings = self.settings();
        let mut entries = lock(&self.entries);
        let Some(i) = entries.iter().position(|e| e.download.id == id) else { return };
        // Its name known at last (a pasted or copied link): a file of that name already there is
        // the user's to decide, before anything is written (`ExistingFile::Ask`).
        let final_name = name.clone().unwrap_or_else(|| entries[i].name.clone());
        if settings.existing == ExistingFile::Ask
            && matches!(entries[i].download.status(), Status::Queued)
            && settings.target_dir(&final_name).join(&final_name).is_file()
        {
            let e = entries.remove(i);
            drop(entries);
            let mut req = AddRequest::from_parts(e.download.url.clone(), e.download.audio.clone(), &e.headers);
            req.filename = Some(final_name);
            req.existing_only = true;
            // Too many questions already: not lost, downloaded under a new name.
            if let Err(req) = self.wait_for_answer(req) {
                self.add(AddRequest { existing: Some(ExistingFile::Rename), existing_only: false, ..*req });
            }
            self.changed();
            return;
        }
        if let Some(name) = name.filter(|n| *n != entries[i].name)
            && matches!(entries[i].download.status(), Status::Queued | Status::Paused)
        {
            match target_for(&settings, &name, &entries, Some(id)) {
                Some(target) => {
                    entries[i].replaces = target.is_file();
                    entries[i].download.target = target;
                    entries[i].named();
                }
                None => {
                    entries.remove(i);
                    drop(entries);
                    self.notice(false, &trf!("Déjà téléchargé, ignoré : {name}", "Already downloaded, skipped: {name}", name = name));
                    self.changed();
                    return;
                }
            }
        }
        entries[i].resolving = false;
        drop(entries);
        self.changed();
    }

    /// `None` when the same link was added a moment ago (a double click, a page asking twice).
    fn insert(
        &self,
        url: Url,
        audio: Option<Url>,
        name: &str,
        headers: Vec<(String, String)>,
        resolving: bool,
        existing: Option<ExistingFile>,
    ) -> Option<DownloadId> {
        let mut settings = self.settings();
        if let Some(existing) = existing {
            settings.existing = existing;
        }
        let mut entries = lock(&self.entries);
        let twice = entries.iter().rev().take_while(|e| e.added.is_some_and(|t| t.elapsed() < DUPLICATE_WINDOW)).any(|e| {
            e.download.url == url && e.download.audio == audio && !matches!(e.download.status(), Status::Failed(_))
        });
        if twice {
            return None;
        }
        // A name already known (the page gave it): an existing file may mean "skip".
        let target = if resolving {
            unique_path(&settings.target_dir(name), name, &entries, None)
        } else {
            let Some(target) = target_for(&settings, name, &entries, None) else {
                drop(entries);
                self.notice(false, &trf!("Déjà téléchargé, ignoré : {name}", "Already downloaded, skipped: {name}", name = name));
                return None;
            };
            target
        };
        // Only "overwrite" keeps the name of an existing file.
        let replaces = target.is_file();
        let mut download = Download::new(url, target, settings.connections);
        download.audio = audio;
        let id = download.id;
        let mut entry = Entry::new(download, headers, 0, 0);
        entry.resolving = resolving;
        entry.replaces = replaces;
        entry.added = Some(Instant::now());
        entries.push(entry);
        drop(entries);
        self.changed();
        Some(id)
    }
}

/// The headers `req` asks for (cookies, referrer, user agent), for requests about its link.
pub fn header_map(req: &AddRequest) -> HeaderMap {
    to_header_map(&req.headers())
}
