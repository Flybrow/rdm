//! The user's settings: what they mean, their defaults, their bounds.

mod store;

use std::{collections::BTreeMap, io, path::PathBuf};

use directories::{BaseDirs, UserDirs};
use domain::Category;
use serde::{Deserialize, Serialize};

use crate::i18n::Language;
pub use store::{config_file, create_private_dir, keep_aside, load_json, read_config, save_json, with_suffix, write_if_changed, write_private};

pub const BRIDGE_PORT: u16 = 9614;
const FILE: &str = "settings.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

/// What to do with a link copied to the clipboard (one whose extension is in the capture list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ClipboardMode {
    Off,
    /// Offer it in the window (one click to download).
    #[default]
    Ask,
    /// Download it right away.
    Auto,
}

/// A file of the same name is already in the destination folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ExistingFile {
    /// RDM asks each time (raised window): keep both, or replace. Where it cannot ask, it renames.
    #[default]
    Ask,
    /// `name (1).ext`.
    Rename,
    Overwrite,
    /// Not downloaded again.
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ProxyMode {
    /// Direct connections, whatever the environment says.
    Off,
    /// The system's proxy (Windows settings, `*_PROXY` variables, GNOME settings).
    #[default]
    System,
    /// Always the proxy below.
    Manual,
    /// Direct first; through the proxy below when a download is slow or cannot connect.
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Proxy {
    #[serde(deserialize_with = "lenient")]
    pub mode: ProxyMode,
    /// `http://host:port`, `socks5://…` (names resolved by the proxy: `socks5h://`), `socks4a://…`.
    pub url: String,
    /// Proxy login (its password lives in the secret store, see `secrets`).
    pub user: String,
}

/// A named queue: downloads added to it start while fewer than `max_parallel` of its own run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    pub id: u32,
    pub name: String,
    pub max_parallel: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    /// Default sub-folder per category (`Downloads/Videos`…), like IDM.
    pub categorize: bool,
    /// User-chosen folder per category; wins over the default above.
    #[serde(deserialize_with = "lenient")]
    pub category_dirs: BTreeMap<Category, PathBuf>,
    /// Extensions the browser extension hands over to RDM (space-separated).
    pub captured: String,
    /// Revision of the defaults `captured` already includes (`domain::CAPTURED_REVISION`). Absent
    /// from files written before revisions: 0, so they receive every addition once.
    #[serde(default)]
    pub captured_revision: u8,
    /// A download sent by the browser extension waits in its own window (in front of the others)
    /// for the user's go-ahead; off: it starts right away.
    pub confirm_browser: bool,
    /// A download from the browser keeps its own window while it runs (progress, then "Open"),
    /// like IDM's; off: the window closes once the download is accepted.
    pub progress_window: bool,
    /// The extension's button over videos is a small icon, without its text.
    pub mini_button: bool,
    /// A browser opening without the extension (and the default one when RDM opens) gets it offered:
    /// RDM starts its installation there, and the browser asks the user.
    pub offer_extension: bool,
    pub connections: u8,
    /// Downloads of the main queue running at once; the rest wait.
    pub max_parallel: u8,
    /// More named queues (the main one, id 0, is implicit).
    #[serde(deserialize_with = "lenient")]
    pub queues: Vec<Queue>,
    /// Global cap in KiB/s, 0 = unlimited.
    pub speed_limit_kib: u32,
    pub autostart: bool,
    /// Closing the window keeps RDM running in the notification area.
    pub close_to_tray: bool,
    /// Desktop notification when a download finishes.
    pub notify: bool,
    #[serde(deserialize_with = "lenient")]
    pub theme: Theme,
    /// Where RDM 0.3.7 and older kept the VirusTotal key: now with the passwords (`secrets`), moved
    /// there at start. Written back only if that move failed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub virustotal_key: String,
    /// Look for a new release on GitHub at start and once a day.
    pub check_updates: bool,
    /// The browser-extension window was offered once on its own (first launch).
    pub extension_offered: bool,
    /// Install updates without asking, once no download is running.
    pub auto_update: bool,
    #[serde(deserialize_with = "lenient")]
    pub language: Language,
    #[serde(deserialize_with = "lenient")]
    pub clipboard: ClipboardMode,
    #[serde(deserialize_with = "lenient")]
    pub existing: ExistingFile,
    #[serde(deserialize_with = "lenient")]
    pub proxy: Proxy,
}

/// The system's download folder; the home folder when there is none.
fn default_download_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|u| u.download_dir().map(PathBuf::from))
        .or_else(|| BaseDirs::new().map(|b| b.home_dir().to_path_buf()))
        .unwrap_or_else(std::env::temp_dir)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            categorize: true,
            category_dirs: BTreeMap::new(),
            captured: domain::default_captured(),
            captured_revision: domain::CAPTURED_REVISION,
            confirm_browser: true,
            progress_window: true,
            mini_button: false,
            offer_extension: true,
            connections: domain::DEFAULT_CONNECTIONS,
            max_parallel: 3,
            queues: Vec::new(),
            speed_limit_kib: 0,
            autostart: false,
            close_to_tray: true,
            notify: true,
            theme: Theme::System,
            virustotal_key: String::new(),
            check_updates: true,
            extension_offered: false,
            auto_update: false,
            language: Language::Auto,
            // macOS may ask the user's leave for each program reading the clipboard: not out of
            // the blue at the first start, only for someone who turned this on.
            clipboard: if cfg!(target_os = "macos") { ClipboardMode::Off } else { ClipboardMode::Ask },
            existing: ExistingFile::Ask,
            proxy: Proxy::default(),
        }
    }
}

/// Most downloads a queue may run at once.
pub const MAX_PARALLEL: u8 = 16;

/// A value this version does not understand (written by a newer RDM, or edited by hand) falls
/// back to its default instead of making the whole file unreadable — which would reset every
/// setting.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

impl Settings {
    pub fn load() -> Self {
        let mut s: Self = load_json(FILE);
        s.sanitize();
        s
    }

    /// Values read from a file (or typed) brought back within their bounds.
    pub fn sanitize(&mut self) {
        if !self.download_dir.is_absolute() {
            self.download_dir = default_download_dir();
        }
        self.category_dirs.retain(|_, dir| dir.is_absolute());
        self.connections = self.connections.clamp(1, domain::MAX_CONNECTIONS);
        self.max_parallel = self.max_parallel.clamp(1, MAX_PARALLEL);
        if self.captured.trim().is_empty() {
            self.captured = domain::default_captured();
        }
        for ext in domain::captured_since(self.captured_revision) {
            if !domain::is_capturable(&self.captured, &format!("file.{ext}")) {
                self.captured.push(' ');
                self.captured.push_str(ext);
            }
        }
        self.captured_revision = domain::CAPTURED_REVISION;
        self.virustotal_key = self.virustotal_key.trim().to_owned();
        self.proxy.url = self.proxy.url.trim().to_owned();
        // Queue ids: unique, never 0 (the main queue); names trimmed.
        let mut seen = std::collections::HashSet::new();
        self.queues.retain(|q| q.id != 0 && seen.insert(q.id));
        for q in &mut self.queues {
            q.max_parallel = q.max_parallel.clamp(1, MAX_PARALLEL);
            q.name = q.name.trim().chars().take(40).collect();
        }
    }

    pub fn save(&self) -> io::Result<()> {
        save_json(FILE, self)
    }

    /// Folder a file of this category goes to.
    pub fn category_dir(&self, category: Category) -> PathBuf {
        match self.category_dirs.get(&category) {
            Some(dir) => dir.clone(),
            None if self.categorize => {
                // Folder names follow the interface language; one created under the other name
                // (the language changed since) keeps being used.
                let english = !crate::i18n::french();
                let (name, other) = (category.label(english), category.label(!english));
                let (dir, previous) = (self.download_dir.join(name), self.download_dir.join(other));
                if !dir.exists() && previous.is_dir() { previous } else { dir }
            }
            None => self.download_dir.clone(),
        }
    }

    pub fn target_dir(&self, file_name: &str) -> PathBuf {
        self.category_dir(Category::of(file_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_folders() {
        let mut s = Settings { download_dir: PathBuf::from("dl"), ..Settings::default() };
        let _one_at_a_time = crate::i18n::TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::i18n::set(crate::i18n::Language::French);
        assert_eq!(s.target_dir("a.mp4"), PathBuf::from("dl").join("Vidéos"));
        s.category_dirs.insert(Category::Video, PathBuf::from("films"));
        assert_eq!(s.target_dir("a.mp4"), PathBuf::from("films"));
        s.categorize = false;
        assert_eq!(s.target_dir("a.zip"), PathBuf::from("dl"));
        assert_eq!(s.target_dir("a.mkv"), PathBuf::from("films"));
    }

    #[test]
    fn old_settings_files_still_load() {
        let old: Settings = serde_json::from_str(r#"{"download_dir":"x","connections":32}"#).unwrap();
        assert!(old.notify && old.close_to_tray && !old.captured.is_empty());
    }

    #[test]
    fn a_list_saved_by_an_older_version_receives_the_new_types_once() {
        let mut saved: Settings = serde_json::from_str(r#"{"captured":"zip iso"}"#).unwrap();

        saved.sanitize();

        assert!(saved.captured.starts_with("zip iso "), "the user's own list is kept");
        assert!(domain::is_capturable(&saved.captured, "model.gguf"));
        assert!(!domain::is_capturable(&saved.captured, "film.mp4"), "a type removed before stays removed");

        saved.captured = "zip iso".into();
        saved.sanitize();

        assert_eq!(saved.captured, "zip iso", "a new type removed afterwards stays removed");
    }

    #[test]
    fn unknown_values_fall_back_instead_of_resetting_everything() {
        let newer = r#"{"download_dir":"x","connections":7,"theme":"Neon","language":"Klingon",
            "proxy":{"mode":"Pac","url":"http://p:1"},"queues":[{"id":3}],"existing":42,
            "category_dirs":{"Hologram":"x"}}"#;
        let s: Settings = serde_json::from_str(newer).unwrap();
        assert!(s.category_dirs.is_empty());
        let chosen: Settings = serde_json::from_str(r#"{"category_dirs":{"Video":"/films","Model":"/models"}}"#).unwrap();
        assert_eq!(chosen.category_dirs.get(&Category::Video), Some(&PathBuf::from("/films")), "the folders it knows are kept");
        assert_eq!(chosen.category_dirs.len(), 2);
        assert_eq!((s.connections, s.theme, s.language), (7, Theme::System, Language::Auto));
        assert_eq!((s.proxy.mode, s.proxy.url.as_str()), (ProxyMode::System, "http://p:1"), "only the unknown mode falls back");
        assert!(s.queues.is_empty() && s.existing == ExistingFile::Ask);
    }
}
