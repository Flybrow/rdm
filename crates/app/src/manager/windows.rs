//! Download windows, like IDM's: a download from the browser asks in a window of its own, in front
//! of the others (folder, name, options for this download only), then shows its progress there, and
//! "Open" once done. The windows themselves are the interface's (`ui::download_window`); here, what
//! they ask and what the user answers.

use super::*;
use add::{AddOptions, NAME_TIMEOUT};

/// Browser downloads waiting in a window for the user's answer, at most (a page flooding the
/// extension): past that, the browser keeps them.
const MAX_ASKING: usize = 50;

/// One download waiting in its window for the user's answer.
pub type Ticket = u64;

/// What a download window is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DownloadWindow {
    /// A browser download waiting for the user's go-ahead.
    Ask(Ticket),
    /// A download of the list: its progress, then what to do with the file.
    Track(DownloadId),
}

/// What the server says about a link, asked as soon as its window opens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Inspection {
    #[default]
    Running,
    Done(Inspected),
    /// No answer (unreachable, refused, too slow): the download may still work.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspected {
    pub name: String,
    pub size: Option<u64>,
    /// Whether a pause resumes where it stopped (`None`: not known, an HLS stream).
    pub resumable: Option<bool>,
}

/// A download waiting in its window, as the window shows it.
#[derive(Debug, Clone)]
pub struct Asking {
    pub url: Url,
    /// The name the page gave (made safe), if any.
    pub given_name: Option<String>,
    pub inspection: Inspection,
    /// A video with its separate audio track, merged into one file.
    pub merged: bool,
}

/// The user's answer in the window.
#[derive(Debug, Clone)]
pub struct Choice {
    pub name: String,
    pub dir: PathBuf,
    pub connections: u8,
    /// Its own speed cap in KiB/s (0 = none).
    pub speed_limit_kib: u32,
    pub queue: u32,
    pub checksum: Option<domain::Checksum>,
    /// Added paused, started when the user says so.
    pub later: bool,
    /// What to do with a file of that name already in `dir`.
    pub existing: Option<ExistingFile>,
    /// Remember `dir` as the folder of this category.
    pub remember: Option<Category>,
    /// Stop asking for the next downloads (`confirm_browser` off).
    pub always: bool,
}

#[derive(Default)]
pub(super) struct Windows {
    last: Ticket,
    asking: HashMap<Ticket, (AddRequest, Inspection)>,
    /// Windows to open, not yet taken by the interface.
    opening: Vec<DownloadWindow>,
}

impl Manager {
    /// How the interface hears that a download window should open.
    pub fn on_window(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.window_hook.set(Box::new(f));
    }

    pub(super) fn open_window(&self, window: DownloadWindow) {
        lock(&self.windows).opening.push(window);
        if let Some(f) = self.window_hook.get() {
            f();
        }
    }

    /// Opens (or brings back) the progress window of a download of the list.
    pub fn show_progress(&self, id: DownloadId) {
        self.open_window(DownloadWindow::Track(id));
    }

    /// Windows wait to be opened by the interface.
    pub fn windows_to_open(&self) -> bool {
        !lock(&self.windows).opening.is_empty()
    }

    /// The windows to open since the last call.
    pub fn take_windows(&self) -> Vec<DownloadWindow> {
        std::mem::take(&mut lock(&self.windows).opening)
    }

    /// A browser download waits in its window; the server is asked about the link meanwhile (name,
    /// size). `Err`: no room left to ask, the request comes back.
    pub(super) fn ask_in_window(self: &Arc<Self>, req: AddRequest) -> Result<(), Box<AddRequest>> {
        let (ticket, url, headers) = {
            let mut windows = lock(&self.windows);
            if windows.asking.values().any(|(r, _)| r.url == req.url && r.audio_url == req.audio_url) {
                return Ok(()); // the very same link, just sent twice
            }
            if windows.asking.len() >= MAX_ASKING {
                return Err(Box::new(req));
            }
            windows.last += 1;
            let ticket = windows.last;
            let (url, audio, headers) = (req.url.clone(), req.audio_url.clone(), req.headers());
            windows.asking.insert(ticket, (req, Inspection::Running));
            (ticket, (url, audio), headers)
        };
        let this = self.clone();
        self.rt.spawn(async move {
            let headers = to_header_map(&headers);
            let (url, audio) = url;
            let found = tokio::time::timeout(NAME_TIMEOUT, async {
                let mut found = this.inspect(&url, &headers).await?;
                // A video and its audio track: the file holds both.
                if let Some(audio) = &audio {
                    let audio_size = this.inspect(audio, &headers).await.and_then(|a| a.size);
                    found.size = found.size.zip(audio_size).map(|(v, a)| v + a);
                }
                Some(found)
            })
            .await
            .ok()
            .flatten();
            if let Some((_, inspection)) = lock(&this.windows).asking.get_mut(&ticket) {
                *inspection = found.map_or(Inspection::Failed, Inspection::Done);
            }
        });
        self.open_window(DownloadWindow::Ask(ticket));
        Ok(())
    }

    /// What window `ticket` asks about (`None`: answered, or gone).
    pub fn asking(&self, ticket: Ticket) -> Option<Asking> {
        let windows = lock(&self.windows);
        let (req, inspection) = windows.asking.get(&ticket)?;
        Some(Asking { url: req.url.clone(), given_name: req.given_name(), inspection: inspection.clone(), merged: req.audio_url.is_some() })
    }

    /// The user answered in window `ticket`: the download it became (`None`: cancelled, or nothing
    /// to download — the same link a moment ago, a file to skip).
    pub fn answer_window(self: &Arc<Self>, ticket: Ticket, choice: Option<Choice>) -> Option<DownloadId> {
        let (mut req, _) = lock(&self.windows).asking.remove(&ticket)?;
        let choice = choice?;
        if choice.remember.is_some() || choice.always {
            let mut settings = self.settings();
            if let Some(category) = choice.remember {
                settings.category_dirs.insert(category, choice.dir.clone());
            }
            if choice.always {
                settings.confirm_browser = false;
            }
            self.apply_settings(settings);
            self.save_settings();
        }
        req.filename = Some(choice.name);
        req.existing = choice.existing;
        req.options = Some(AddOptions {
            dir: Some(choice.dir),
            connections: Some(choice.connections),
            speed_limit_kib: choice.speed_limit_kib,
            queue: choice.queue,
            checksum: choice.checksum,
            paused: choice.later,
        });
        self.add_new(req)
    }
}
