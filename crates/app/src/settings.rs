use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Mutex, PoisonError},
};

use directories::{ProjectDirs, UserDirs};
use domain::Category;
use serde::{Deserialize, Serialize};

use crate::i18n::Language;

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
    pub category_dirs: BTreeMap<Category, PathBuf>,
    /// Extensions the browser extension hands over to RDM (space-separated).
    pub captured: String,
    /// A download sent by the browser extension waits in RDM's window (brought to the front)
    /// for the user's go-ahead; off: it starts right away.
    pub confirm_browser: bool,
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
    /// The version that ran last: another one at start means an update was just installed.
    pub last_version: String,
    #[serde(deserialize_with = "lenient")]
    pub language: Language,
    #[serde(deserialize_with = "lenient")]
    pub clipboard: ClipboardMode,
    #[serde(deserialize_with = "lenient")]
    pub existing: ExistingFile,
    #[serde(deserialize_with = "lenient")]
    pub proxy: Proxy,
}

impl Default for Settings {
    fn default() -> Self {
        let download_dir = UserDirs::new()
            .and_then(|u| u.download_dir().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            download_dir,
            categorize: true,
            category_dirs: BTreeMap::new(),
            captured: domain::default_captured(),
            confirm_browser: true,
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
            last_version: String::new(),
            language: Language::Auto,
            clipboard: ClipboardMode::Ask,
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
        self.connections = self.connections.clamp(1, domain::MAX_CONNECTIONS);
        self.max_parallel = self.max_parallel.clamp(1, MAX_PARALLEL);
        if self.captured.trim().is_empty() {
            self.captured = domain::default_captured();
        }
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

    pub fn save(&self) {
        save_json(FILE, self);
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

/// `RDM_CONFIG_DIR` overrides the location (portable mode, e.g. on a USB stick).
pub fn config_file(name: &str) -> PathBuf {
    std::env::var_os("RDM_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| ProjectDirs::from("org", "rdm", "rdm").map(|d| d.config_dir().to_path_buf()))
        .unwrap_or_default()
        .join(name)
}

/// A JSON file of the settings folder; the default when there is none. A file this version cannot
/// read (damaged) is kept aside as `<name>.bad` rather than overwritten by the next save.
pub fn load_json<T: serde::de::DeserializeOwned + Default>(name: &str) -> T {
    load_json_at(&config_file(name))
}

fn load_json_at<T: serde::de::DeserializeOwned + Default>(path: &std::path::Path) -> T {
    let Ok(bytes) = fs::read(path) else { return T::default() };
    serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        let _ = fs::copy(path, with_suffix(path, ".bad"));
        T::default()
    })
}

/// Atomic write (temp file + rename), serialized: concurrent saves from the UI and download
/// threads can neither interleave in the temp file nor leave a truncated config after a crash.
pub fn save_json(name: &str, value: &impl Serialize) {
    static WRITE: Mutex<()> = Mutex::new(());
    let Ok(bytes) = serde_json::to_vec(value) else { return };
    let path = config_file(name);
    let _guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(dir) = path.parent() {
        let _ = create_private_dir(dir);
    }
    let tmp = path.with_extension("json.tmp");
    // Synced before the rename: a power cut right after must not leave an empty list behind.
    let written = fs::File::create(&tmp).and_then(|mut f| {
        std::io::Write::write_all(&mut f, &bytes)?;
        f.sync_all()
    });
    if written.is_ok() {
        let _ = fs::rename(tmp, path);
    }
}

/// RDM's settings folder, private to the user on Linux (0700, like `~/.ssh`): the download list
/// holds links with their access tokens. Windows: the profile folder is private already.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        // Made private too when an older RDM created it readable by everyone.
        if fs::metadata(dir).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)
}

/// Writes a file only the user can read: created 0600 on Linux — never readable by others, not
/// even for an instant; Windows: the profile folder is private to the account already.
pub fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        // A leftover file of that name keeps its mode when reopened: made private in any case.
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f
    };
    #[cfg(not(unix))]
    let mut f = fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()
}

pub use engine::with_suffix;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_folders() {
        let mut s = Settings { download_dir: PathBuf::from("dl"), ..Settings::default() };
        let _one_at_a_time = crate::i18n::TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        crate::i18n::set(crate::i18n::Language::French);
        assert_eq!(s.target_dir("a.mp4"), PathBuf::from("dl").join("Vidéos"));
        s.category_dirs.insert(Category::Video, PathBuf::from("films"));
        assert_eq!(s.target_dir("a.mp4"), PathBuf::from("films"));
        s.categorize = false;
        assert_eq!(s.target_dir("a.zip"), PathBuf::from("dl"));
        assert_eq!(s.target_dir("a.mkv"), PathBuf::from("films"));
    }

    #[cfg(unix)]
    #[test]
    fn the_settings_folder_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rdm-private-{}", std::process::id()));
        let nested = dir.join("rdm");
        create_private_dir(&nested).unwrap();
        assert_eq!(fs::metadata(&nested).unwrap().permissions().mode() & 0o777, 0o700);
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&nested).unwrap();
        assert_eq!(fs::metadata(&nested).unwrap().permissions().mode() & 0o777, 0o700, "an older, readable one is fixed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_file_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("rdm-load-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        assert_eq!(load_json_at::<Vec<u32>>(&path), Vec::<u32>::new(), "none yet: the default");
        fs::write(&path, b"[1, 2").unwrap();
        assert_eq!(load_json_at::<Vec<u32>>(&path), Vec::<u32>::new());
        assert_eq!(fs::read(with_suffix(&path, ".bad")).unwrap(), b"[1, 2", "kept for the user");
        fs::write(&path, b"[1, 2]").unwrap();
        assert_eq!(load_json_at::<Vec<u32>>(&path), [1, 2]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_settings_files_still_load() {
        let old: Settings = serde_json::from_str(r#"{"download_dir":"x","connections":32}"#).unwrap();
        assert!(old.notify && old.close_to_tray && !old.captured.is_empty());
    }

    #[test]
    fn unknown_values_fall_back_instead_of_resetting_everything() {
        let newer = r#"{"download_dir":"x","connections":7,"theme":"Neon","language":"Klingon",
            "proxy":{"mode":"Pac","url":"http://p:1"},"queues":[{"id":3}],"existing":42}"#;
        let s: Settings = serde_json::from_str(newer).unwrap();
        assert_eq!((s.connections, s.theme, s.language), (7, Theme::System, Language::Auto));
        assert_eq!((s.proxy.mode, s.proxy.url.as_str()), (ProxyMode::System, "http://p:1"), "only the unknown mode falls back");
        assert!(s.queues.is_empty() && s.existing == ExistingFile::Ask);
    }
}
