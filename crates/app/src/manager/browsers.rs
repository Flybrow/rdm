//! Browsers: extension installs, Firefox pairing and check-ins.

use super::*;

/// Firefox gives each install of an extension a random origin (`moz-extension://<uuid>`), which
/// cannot be pinned like Chrome's: the native connector pairs it (the browser vouches for the
/// extension), or, without the connector, the user approves it once in the RDM window.
#[derive(Default)]
pub(super) struct FirefoxPairing {
    /// Firefox, Waterfox, LibreWolf… each profile has its own origin; the latest few are kept.
    pub(super) paired: VecDeque<String>,
    pub(super) pending: Option<String>,
    /// Refused this session: never asked again until RDM restarts.
    pub(super) refused: HashSet<String>,
    /// "Later" (✕): the extension's periodic check-ins do not bring the question back before this.
    pub(super) snoozed_until: Option<Instant>,
}

/// How long "later" on the Firefox question lasts, unless the user clicks the extension's button.
const FIREFOX_SNOOZE: Duration = Duration::from_secs(30 * 60);

const FIREFOX_FILE: &str = "firefox.json";
const MAX_PAIRED: usize = 8;
/// Browsers the extension may report from (see `Manager::browser_seen`).
const MAX_BROWSERS: usize = 32;

impl FirefoxPairing {
    /// `firefox.json`: a list of origins (RDM 0.2 wrote a single one).
    pub(super) fn load() -> VecDeque<String> {
        let bytes = std::fs::read(crate::settings::config_file(FIREFOX_FILE)).unwrap_or_default();
        let origins = serde_json::from_slice::<VecDeque<String>>(&bytes)
            .or_else(|_| serde_json::from_slice::<String>(&bytes).map(|o| VecDeque::from([o])))
            .unwrap_or_default();
        origins.into_iter().filter(|o| is_firefox_origin(o)).take(MAX_PAIRED).collect()
    }

    fn pair(&mut self, origin: String) {
        self.refused.remove(&origin);
        if self.pending.as_ref() == Some(&origin) {
            self.pending = None;
        }
        if !self.paired.contains(&origin) {
            self.paired.push_back(origin);
            while self.paired.len() > MAX_PAIRED {
                self.paired.pop_front();
            }
            save_json(FIREFOX_FILE, &self.paired);
        }
    }
}

fn is_firefox_origin(origin: &str) -> bool {
    origin.strip_prefix("moz-extension://").is_some_and(|id| {
        id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    })
}

impl Manager {
    /// Whether this Firefox extension origin may use the bridge; an unknown one is put up for the
    /// user's approval (the window comes to the front) and refused meanwhile.
    pub fn firefox_allowed(&self, origin: &str) -> bool {
        if !is_firefox_origin(origin) {
            return false;
        }
        let mut ff = lock(&self.firefox);
        if ff.paired.iter().any(|o| o == origin) {
            return true;
        }
        let snoozed = ff.snoozed_until.is_some_and(|t| Instant::now() < t);
        if ff.pending.is_none() && !ff.refused.contains(origin) && !snoozed {
            ff.pending = Some(origin.to_owned());
            drop(ff);
            self.show();
            self.repaint();
        }
        false
    }

    /// The user clicked the extension's button: a question put off with "later" comes back now.
    pub fn firefox_wake(&self) {
        lock(&self.firefox).snoozed_until = None;
    }

    /// The Firefox origin waiting for the user's approval, if any.
    pub fn firefox_pending(&self) -> Option<String> {
        lock(&self.firefox).pending.clone()
    }

    /// `None`: dismissed without answering — asked again at the extension's next request.
    pub fn answer_firefox(&self, allow: Option<bool>) {
        let mut ff = lock(&self.firefox);
        let Some(origin) = ff.pending.take() else { return };
        match allow {
            Some(true) => ff.pair(origin),
            Some(false) => {
                ff.refused.insert(origin);
            }
            None => ff.snoozed_until = Some(Instant::now() + FIREFOX_SNOOZE),
        }
    }

    /// The native connector vouches for this Firefox extension origin (the browser started the
    /// connector for the RDM extension only): paired without asking.
    pub fn pair_firefox(&self, origin: &str) -> bool {
        if !is_firefox_origin(origin) {
            return false;
        }
        lock(&self.firefox).pair(origin.to_owned());
        self.repaint();
        true
    }

    /// The extension reported which browser it runs in (`x-rdm-browser`).
    pub fn browser_seen(&self, key: &str) {
        // A computer has a handful of browsers: past this, a caller making names up is ignored
        // (each browser met is kept for the whole session).
        let room = {
            let seen = lock(&self.browsers);
            seen.contains_key(key) || seen.len() < MAX_BROWSERS
        };
        if !room {
            return;
        }
        let Some(browser) = Browser::from_key(key) else { return };
        if self.uninstall_requested(key) {
            return; // on its way out: not "installed" again
        }
        let now = unix_now();
        let mut seen = lock(&self.browsers);
        let before = seen.insert(browser.key().to_owned(), now);
        // Written when news, not on every request (recordings post chunks many times a second).
        if before.is_none_or(|t| now.saturating_sub(t) > 600) {
            let copy = seen.clone();
            drop(seen);
            save_json(BROWSERS_FILE, &copy);
            self.repaint();
        }
    }

    /// When the extension was last heard from, per browser (Unix time).
    pub fn browser_last_seen(&self, browser: Browser) -> Option<u64> {
        lock(&self.browsers).get(browser.key()).copied()
    }

    /// "Remove the extension": RDM forgets it in `browser` at once, and the extension, if still
    /// there, uninstalls itself at its next check-in (at the browser's start, then every few minutes).
    pub fn remove_extension(&self, browser: Browser) {
        let key = browser.key().to_owned();
        let seen = {
            let mut seen = lock(&self.browsers);
            seen.remove(&key);
            seen.clone()
        };
        save_json(BROWSERS_FILE, &seen);
        let pending = {
            let mut pending = lock(&self.uninstalls);
            pending.insert(key, unix_now());
            pending.clone()
        };
        save_json(UNINSTALLS_FILE, &pending);
        lock(&self.installs).remove(&browser);
        self.repaint();
    }

    /// Whether the extension in the browser named `key` must uninstall itself.
    pub fn uninstall_requested(&self, key: &str) -> bool {
        lock(&self.uninstalls).get(key).is_some_and(|&t| unix_now().saturating_sub(t) < UNINSTALL_TTL_SECS)
    }

    /// The request is over: the extension is uninstalling itself, or the user installs it again.
    pub(super) fn forget_uninstall(&self, key: &str) {
        let pending = {
            let mut pending = lock(&self.uninstalls);
            if pending.remove(key).is_none() {
                return;
            }
            pending.clone()
        };
        save_json(UNINSTALLS_FILE, &pending);
    }

    /// The extension of the browser named `key` got the request and uninstalls itself now.
    pub fn extension_uninstalled(&self, key: &str) {
        self.forget_uninstall(key);
        self.repaint();
    }

    /// Every browser the extension was heard from, with when (Unix time).
    pub fn browsers_seen(&self) -> Vec<(Browser, u64)> {
        lock(&self.browsers).iter().filter_map(|(key, &t)| Some((Browser::from_key(key)?, t))).collect()
    }

    /// The browsers of this computer (Windows' list, the well-known ones, those added by hand),
    /// then those the extension was heard from that are not among them (a portable browser).
    pub fn browsers(&self) -> Vec<Browser> {
        let mut list = extension::installed(&lock(&self.custom_browsers));
        for (b, _) in self.browsers_seen() {
            if !list.contains(&b) {
                list.push(b);
            }
        }
        list
    }

    /// "Add a browser…": the browser at `exe`, remembered. `None`: not a browser the extension
    /// can run in (neither Chromium- nor Firefox-based).
    pub fn add_browser(&self, exe: PathBuf) -> Option<Browser> {
        let browser = Browser::at(&exe)?;
        let list = {
            let mut list = lock(&self.custom_browsers);
            if !list.contains(&exe) {
                list.push(exe);
            }
            list.clone()
        };
        save_json(CUSTOM_BROWSERS_FILE, &list);
        Some(browser)
    }

    /// Whether an installation is being prepared (the window shows a spinner).
    pub fn installing(&self) -> bool {
        lock(&self.installs).values().any(|i| matches!(i, Install::Working))
    }

    /// The extension just installed in the browser named `key`: whether RDM opened that browser
    /// for it a moment ago (then the tab it opened can go). Answered once.
    pub fn installed_by_rdm(&self, key: &str) -> bool {
        lock(&self.opened_to_install).remove(key).is_some_and(|at| at.elapsed() < INSTALL_TAB_TTL)
    }

    pub fn install_state(&self, browser: Browser) -> Option<Install> {
        lock(&self.installs).get(&browser).cloned()
    }

    /// Prepares the extension for `browser` and opens the browser where the user confirms it
    /// (Firefox and its derivatives: the Firefox store first).
    pub fn install_extension(self: &Arc<Self>, browser: Browser) {
        self.install_extension_with(browser, true);
    }

    /// Same, without the Firefox store: the package RDM ships (signed, or the temporary add-on).
    pub fn install_extension_without_store(self: &Arc<Self>, browser: Browser) {
        self.install_extension_with(browser, false);
    }

    fn install_extension_with(self: &Arc<Self>, browser: Browser, store: bool) {
        self.forget_uninstall(browser.key());
        if matches!(lock(&self.installs).insert(browser, Install::Working), Some(Install::Working)) {
            return; // already on it
        }
        self.repaint();
        let this = self.clone();
        self.rt.spawn(async move {
            let state = match this.install(browser, store).await {
                Ok(done) => Install::Done(done),
                Err(reason) => Install::Failed(reason),
            };
            if matches!(&state, Install::Done(done) if done.launched) {
                lock(&this.opened_to_install).insert(browser.key().to_owned(), Instant::now());
            }
            lock(&this.installs).insert(browser, state);
            this.repaint();
        });
    }

    pub(super) async fn install(&self, browser: Browser, store: bool) -> Result<Installed, String> {
        let flavour = browser.flavour();
        let blocking = |e: tokio::task::JoinError| e.to_string();
        let exe = tokio::task::spawn_blocking(move || browser.find()).await.map_err(blocking)?;
        // The connector too (a browser installed since RDM started has not got it yet).
        let _ = tokio::task::spawn_blocking(crate::native::register).await;
        let folder = tokio::task::spawn_blocking(move || extension::write(flavour))
            .await
            .map_err(blocking)?
            .map_err(|e| trf!("impossible d'écrire l'extension : {e}", "cannot write the extension: {e}", e = e))?;
        let mut done = Installed { folder, launched: false, store: false, signed: false, xpi: None };
        let open = |target: &str| exe.as_deref().is_some_and(|exe| extension::launch(exe, target).is_ok());
        if flavour == Flavour::Firefox {
            done.xpi = tokio::task::spawn_blocking(extension::write_xpi).await.map_err(blocking)?.ok();
            // The Firefox store: installed for good and kept up to date by the browser. If the
            // browser cannot be opened on it, the package RDM ships takes over.
            if store && open(extension::FIREFOX_STORE) {
                done.store = true;
                done.launched = true;
                return Ok(done);
            }
            let signed = extension::base().join("rdm-firefox-signed.xpi");
            if let Some(client) = self.web()
                && update::signed_firefox_xpi(&client, &signed).await.unwrap_or(false)
            {
                done.signed = true;
                done.launched = open(&signed.to_string_lossy());
                return Ok(done);
            }
            // Waterfox can install the unsigned package for good (signature check off: see the steps).
            if browser.key() == "waterfox"
                && let Some(xpi) = &done.xpi
            {
                done.launched = open(&xpi.to_string_lossy());
                return Ok(done);
            }
        }
        done.launched = open(browser.extensions_page());
        Ok(done)
    }
}
