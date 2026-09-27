use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Mutex, PoisonError},
};

use directories::{ProjectDirs, UserDirs};
use domain::Category;
use serde::{Deserialize, Serialize};

pub const BRIDGE_PORT: u16 = 9614;
const FILE: &str = "settings.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    /// Default sub-folder per category (`Téléchargements/Vidéos`…), like IDM.
    pub categorize: bool,
    /// User-chosen folder per category; wins over the default above.
    pub category_dirs: BTreeMap<Category, PathBuf>,
    /// Extensions the browser extension hands over to RDM (space-separated).
    pub captured: String,
    pub connections: u8,
    /// Downloads running at once; the rest wait in the queue.
    pub max_parallel: u8,
    /// Global cap in KiB/s, 0 = unlimited.
    pub speed_limit_kib: u32,
    pub autostart: bool,
    /// Closing the window keeps RDM running in the notification area.
    pub close_to_tray: bool,
    /// Desktop notification when a download finishes.
    pub notify: bool,
    pub theme: Theme,
    /// The user's own (free) VirusTotal API key; empty = not set up.
    pub virustotal_key: String,
    /// Look for a new release on GitHub at start and once a day.
    pub check_updates: bool,
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
            connections: domain::DEFAULT_CONNECTIONS,
            max_parallel: 3,
            speed_limit_kib: 0,
            autostart: false,
            close_to_tray: true,
            notify: true,
            theme: Theme::System,
            virustotal_key: String::new(),
            check_updates: true,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut s: Self = fs::read(config_file(FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        s.connections = s.connections.clamp(1, domain::MAX_CONNECTIONS);
        s.max_parallel = s.max_parallel.clamp(1, 16);
        if s.captured.trim().is_empty() {
            s.captured = domain::default_captured();
        }
        s.virustotal_key = s.virustotal_key.trim().to_owned();
        s
    }

    pub fn save(&self) {
        save_json(FILE, self);
    }

    /// Folder a file of this category goes to.
    pub fn category_dir(&self, category: Category) -> PathBuf {
        match self.category_dirs.get(&category) {
            Some(dir) => dir.clone(),
            None if self.categorize => self.download_dir.join(category.label()),
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

/// Atomic write (temp file + rename), serialized: concurrent saves from the UI and download
/// threads can neither interleave in the temp file nor leave a truncated config after a crash.
pub fn save_json(name: &str, value: &impl Serialize) {
    static WRITE: Mutex<()> = Mutex::new(());
    let Ok(bytes) = serde_json::to_vec(value) else { return };
    let path = config_file(name);
    let _guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_folders() {
        let mut s = Settings { download_dir: PathBuf::from("dl"), ..Settings::default() };
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
}
