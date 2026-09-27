//! Links copied anywhere (a web page, a chat, a document): those pointing to a file of a captured
//! type are offered in the window (one click) or downloaded at once, as the settings say.

use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use url::Url;

use super::{AddRequest, Manager, lock};
use crate::settings::ClipboardMode;

/// How often the clipboard is looked at. On Windows only a counter is read until it changes; on
/// Linux the text itself has to be asked from the application that holds it: less often.
const POLL: Duration = if cfg!(windows) { Duration::from_secs(1) } else { Duration::from_secs(2) };
/// Longer texts are not scanned for links (a copied document, not a link).
const MAX_TEXT: usize = 64 << 10;
const MAX_LINKS: usize = 20;
/// An offer nobody answered goes away after this long.
pub const OFFER_TTL: Duration = Duration::from_secs(60);

/// Links found in the clipboard, waiting for the user's click.
#[derive(Debug, Clone)]
pub struct Offer {
    pub urls: Vec<Url>,
    pub at: Instant,
}

impl Manager {
    pub(super) fn spawn_clipboard_watch(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("clipboard".into()).spawn(move || watch(&weak));
    }

    /// The offer to show, if any (expired ones are dropped).
    pub fn clipboard_offer(&self) -> Option<Offer> {
        let mut offer = lock(&self.offer);
        if offer.as_ref().is_some_and(|o| o.at.elapsed() > OFFER_TTL) {
            *offer = None;
        }
        offer.clone()
    }

    /// The user answered the offer: download (`true`) or dismiss.
    pub fn answer_clipboard(self: &Arc<Self>, download: bool) {
        let Some(offer) = lock(&self.offer).take() else { return };
        if download {
            offer.urls.into_iter().for_each(|u| self.add(AddRequest::from_url(u)));
        }
        self.repaint();
    }

    /// Text RDM itself puts in the clipboard (copy link, copy path): not offered back.
    pub fn copied_by_rdm(&self, text: &str) {
        *lock(&self.own_copy) = Some(text.to_owned());
    }

    fn clipboard_found(self: &Arc<Self>, urls: Vec<Url>) {
        match self.with_settings(|s| s.clipboard) {
            ClipboardMode::Off => {}
            ClipboardMode::Ask => {
                *lock(&self.offer) = Some(Offer { urls, at: Instant::now() });
                self.repaint();
            }
            ClipboardMode::Auto => {
                let n = urls.len();
                urls.into_iter().for_each(|u| self.add(AddRequest::from_url(u)));
                self.notice(false, &crate::i18n::count(n as u64, ("lien copié ajouté", "liens copiés ajoutés"), ("copied link added", "copied links added")));
            }
        }
    }

    /// Links of `text` worth offering: http(s), of a captured type, not already in the list.
    fn worth_offering(&self, text: &str) -> Vec<Url> {
        let captured = self.with_settings(|s| s.captured.clone());
        let mut urls: Vec<Url> = links(text, &captured);
        self.view(|entries| urls.retain(|u| !entries.iter().any(|e| &e.download.url == u)));
        urls
    }
}

/// http(s) links of `text` whose file type is in `captured` (at most `MAX_LINKS`, no duplicates).
pub fn links(text: &str, captured: &str) -> Vec<Url> {
    let mut found: Vec<Url> = Vec::new();
    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | ',' | ';'));
        let Ok(url) = word.parse::<Url>() else { continue };
        if matches!(url.scheme(), "http" | "https") && domain::is_capturable(captured, url.as_str()) && !found.contains(&url) {
            found.push(url);
            if found.len() == MAX_LINKS {
                break;
            }
        }
    }
    found
}

/// Only a fingerprint of the last text is kept: a copied password does not linger in RDM's memory.
fn fingerprint(text: &str) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(text)
}

fn watch(manager: &Weak<Manager>) {
    let mut clipboard: Option<arboard::Clipboard> = None;
    let mut last: Option<u64> = None;
    let mut sequence = clipboard_sequence();
    let mut first = true;
    loop {
        std::thread::sleep(POLL);
        let Some(this) = manager.upgrade() else { return };
        if this.is_closing() {
            return;
        }
        if this.with_settings(|s| s.clipboard) == ClipboardMode::Off {
            clipboard = None; // let go of the display connection while unused
            continue;
        }
        let now = clipboard_sequence();
        if !first && now.is_some() && now == sequence {
            continue; // unchanged (Windows): nothing read
        }
        sequence = now;
        if clipboard.is_none() {
            clipboard = arboard::Clipboard::new().ok();
        }
        let Some(text) = clipboard.as_mut().and_then(|c| c.get_text().ok()) else { continue };
        let print = fingerprint(&text);
        if text.len() > MAX_TEXT || last == Some(print) {
            continue;
        }
        last = Some(print);
        // What was there before RDM started is not "just copied".
        if std::mem::take(&mut first) {
            continue;
        }
        if lock(&this.own_copy).take().is_some_and(|own| own == text) {
            continue;
        }
        let urls = this.worth_offering(&text);
        if !urls.is_empty() {
            this.clipboard_found(urls);
        }
    }
}

/// Windows counts clipboard changes: reading the counter costs nothing. `None` elsewhere.
fn clipboard_sequence() -> Option<u32> {
    #[cfg(windows)]
    {
        // SAFETY: no arguments, reads a counter.
        Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_captured_links_only() {
        let text = "Voir https://x.io/a.zip, puis <https://x.io/b.MP4> et https://x.io/page.html ftp://x.io/c.zip https://x.io/a.zip";
        let found: Vec<String> = links(text, "zip mp4").iter().map(ToString::to_string).collect();
        assert_eq!(found, ["https://x.io/a.zip", "https://x.io/b.MP4"]);
        assert!(links("no links here", "zip").is_empty());
    }
}
